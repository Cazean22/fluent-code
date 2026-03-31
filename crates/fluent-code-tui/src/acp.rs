use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc as StdArc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use fluent_code_app::config::Config;
use fluent_code_app::error::{FluentCodeError, Result};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::info;

use crate::terminal;
use crate::theme::TUI_THEME;

const ACP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const ACP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const ACP_TEST_PROBES_ENV_VAR: &str = "FLUENT_CODE_ACP_ENABLE_TEST_PROBES";
const PROJECTION_IDLE_INPUT_POLL: Duration = Duration::from_millis(50);
const PROMPT_IN_FLIGHT_REDRAW_CADENCE: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiProjectionState {
    pub session: SessionProjection,
    pub transcript_rows: Vec<TranscriptRowProjection>,
    pub tool_statuses: Vec<ToolStatusProjection>,
    pub pending_permission: Option<PendingPermissionProjection>,
    pub subprocess: SubprocessProjection,
    pub draft_input: String,
    pub prompt_in_flight: bool,
    pub prompt_error: Option<String>,
    pub startup_error: Option<String>,
    transcript_merge_start: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionProjection {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSource {
    User,
    Agent,
    Thought,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRowProjection {
    pub source: TranscriptSource,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatusProjection {
    pub tool_call_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermissionProjection {
    pub tool_call_id: String,
    pub title: String,
    pub options: Vec<PermissionOptionProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOptionProjection {
    pub option_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SubprocessStatus {
    #[default]
    NotStarted,
    Spawned {
        binary_path: PathBuf,
        pid: u32,
    },
    Initialized {
        binary_path: PathBuf,
        pid: u32,
        protocol_version: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubprocessProjection {
    pub status: SubprocessStatus,
}

impl TuiProjectionState {
    fn reset_session_projection(&mut self, session_id: Option<String>) {
        self.session = SessionProjection {
            session_id,
            ..SessionProjection::default()
        };
        self.transcript_rows.clear();
        self.transcript_merge_start = 0;
        self.tool_statuses.clear();
        self.pending_permission = None;
        self.draft_input.clear();
        self.prompt_in_flight = false;
        self.prompt_error = None;
    }

    fn mark_spawned(&mut self, binary_path: PathBuf, pid: u32) {
        self.startup_error = None;
        self.subprocess.status = SubprocessStatus::Spawned { binary_path, pid };
    }

    fn mark_initialized(
        &mut self,
        binary_path: PathBuf,
        pid: u32,
        response: &acp::InitializeResponse,
    ) {
        self.startup_error = None;
        self.subprocess.status = SubprocessStatus::Initialized {
            binary_path,
            pid,
            protocol_version: protocol_version_label(response.protocol_version.clone()),
        };
    }

    fn mark_startup_error(&mut self, message: String) {
        self.startup_error = Some(message.clone());
        self.prompt_in_flight = false;
        self.break_transcript_merge();
        self.subprocess.status = SubprocessStatus::Failed { message };
    }

    fn mark_session_created(&mut self, session_id: acp::SessionId) {
        self.startup_error = None;
        self.reset_session_projection(Some(session_id.to_string()));
    }

    fn prepare_session_load(&mut self, session_id: &acp::SessionId) {
        self.startup_error = None;
        self.reset_session_projection(Some(session_id.to_string()));
    }

    fn set_draft_input(&mut self, draft_input: impl Into<String>) {
        self.draft_input = draft_input.into();
    }

    fn mark_prompt_started(&mut self) {
        self.prompt_error = None;
        self.prompt_in_flight = true;
        self.draft_input.clear();
        self.break_transcript_merge();
    }

    fn mark_prompt_finished(&mut self) {
        self.prompt_in_flight = false;
        self.break_transcript_merge();
    }

    fn mark_prompt_error(&mut self, message: String) {
        self.prompt_in_flight = false;
        self.prompt_error = Some(message);
        self.break_transcript_merge();
    }

    fn can_edit_draft(&self) -> bool {
        self.startup_error.is_none() && !self.prompt_in_flight && self.pending_permission.is_none()
    }

    fn apply_session_notification(&mut self, notification: acp::SessionNotification) {
        let session_id = notification.session_id.to_string();
        if self
            .session
            .session_id
            .as_deref()
            .is_some_and(|current| current != session_id)
        {
            return;
        }

        self.session.session_id = Some(session_id);

        match notification.update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                self.transcript_rows.push(TranscriptRowProjection {
                    source: TranscriptSource::User,
                    content: content_block_label(chunk.content),
                });
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.append_transcript_chunk(TranscriptSource::Agent, chunk.content);
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                self.append_transcript_chunk(TranscriptSource::Thought, chunk.content);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                self.break_transcript_merge();
                self.upsert_tool_status(
                    tool_call.tool_call_id.to_string(),
                    tool_call.title,
                    tool_call_status_label(tool_call.status),
                );
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                self.break_transcript_merge();
                self.apply_tool_call_update(update);
            }
            acp::SessionUpdate::SessionInfoUpdate(update) => {
                if let Some(title) = update.title.take() {
                    self.session.title = Some(title);
                }
                if let Some(updated_at) = update.updated_at.take() {
                    self.session.updated_at = Some(updated_at);
                }
            }
            acp::SessionUpdate::Plan(_)
            | acp::SessionUpdate::AvailableCommandsUpdate(_)
            | acp::SessionUpdate::CurrentModeUpdate(_)
            | acp::SessionUpdate::ConfigOptionUpdate(_) => {}
            _ => {}
        }
    }

    fn apply_permission_request(&mut self, request: &acp::RequestPermissionRequest) {
        let session_id = request.session_id.to_string();
        if self
            .session
            .session_id
            .as_deref()
            .is_some_and(|current| current != session_id)
        {
            return;
        }

        self.session.session_id = Some(session_id);
        self.pending_permission = Some(PendingPermissionProjection {
            tool_call_id: request.tool_call.tool_call_id.to_string(),
            title: request
                .tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "permission request".to_string()),
            options: request
                .options
                .iter()
                .map(|option| PermissionOptionProjection {
                    option_id: option.option_id.to_string(),
                    name: option.name.clone(),
                })
                .collect(),
        });
    }

    fn append_transcript_chunk(&mut self, source: TranscriptSource, content: acp::ContentBlock) {
        let content = content_block_label(content);
        if content.is_empty() {
            return;
        }

        let can_merge_last_row = self.transcript_merge_start < self.transcript_rows.len()
            && self
                .transcript_rows
                .last()
                .is_some_and(|row| row.source == source);

        if can_merge_last_row {
            let existing = self
                .transcript_rows
                .last_mut()
                .expect("last row to exist when merge is allowed");
            existing.content.push_str(&content);
            return;
        }

        self.transcript_rows
            .push(TranscriptRowProjection { source, content });
    }

    fn break_transcript_merge(&mut self) {
        self.transcript_merge_start = self.transcript_rows.len();
    }

    fn upsert_tool_status(&mut self, tool_call_id: String, title: String, status: String) {
        if let Some(existing) = self
            .tool_statuses
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
        {
            existing.title = title;
            existing.status = status;
            return;
        }

        self.tool_statuses.push(ToolStatusProjection {
            tool_call_id,
            title,
            status,
        });
    }

    fn apply_tool_call_update(&mut self, update: acp::ToolCallUpdate) {
        let tool_call_id = update.tool_call_id.to_string();
        let status = update
            .fields
            .status
            .map(tool_call_status_label)
            .unwrap_or_else(|| "pending".to_string());
        let title = update
            .fields
            .title
            .unwrap_or_else(|| format!("tool {}", tool_call_id));
        self.upsert_tool_status(tool_call_id, title, status);
    }
}

#[derive(Debug)]
struct PendingPermissionRequest {
    request: acp::RequestPermissionRequest,
    response_sender: oneshot::Sender<acp::RequestPermissionResponse>,
}

#[derive(Debug, Clone, Default)]
struct AcpSessionRoots {
    session_cwds: Arc<StdMutex<HashMap<acp::SessionId, PathBuf>>>,
}

impl AcpSessionRoots {
    fn register_session_cwd(
        &self,
        session_id: acp::SessionId,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        let normalized_cwd = normalize_absolute_path(cwd.into(), "session cwd")?;
        let canonical_cwd = fs::canonicalize(&normalized_cwd)
            .map_err(|error| map_filesystem_error("resolve session cwd", &normalized_cwd, error))?;
        self.session_cwds
            .lock()
            .expect("session cwd registry mutex poisoned")
            .insert(session_id, canonical_cwd);
        Ok(())
    }

    fn resolve_existing_session_path(
        &self,
        session_id: &acp::SessionId,
        requested_path: &Path,
        operation: &str,
        subject: &str,
        capability_name: &str,
    ) -> acp::Result<PathBuf> {
        let normalized_path = normalize_absolute_path(requested_path, subject)?;
        let canonical_path = fs::canonicalize(&normalized_path)
            .map_err(|error| map_filesystem_error(operation, &normalized_path, error))?;
        self.ensure_path_within_session_cwd(
            session_id,
            &normalized_path,
            canonical_path,
            subject,
            capability_name,
        )
    }

    fn resolve_writable_session_path(
        &self,
        session_id: &acp::SessionId,
        requested_path: &Path,
    ) -> acp::Result<PathBuf> {
        let normalized_path = normalize_absolute_path(requested_path, "filesystem path")?;

        let resolved_path = if normalized_path.exists() {
            fs::canonicalize(&normalized_path).map_err(|error| {
                map_filesystem_error("resolve filesystem path", &normalized_path, error)
            })?
        } else {
            let parent = normalized_path.parent().ok_or_else(|| {
                acp::Error::invalid_params().data(serde_json::json!({
                    "message": format!(
                        "filesystem path `{}` must include a parent directory",
                        normalized_path.display()
                    )
                }))
            })?;
            let canonical_parent = fs::canonicalize(parent)
                .map_err(|error| map_filesystem_error("resolve parent directory", parent, error))?;
            canonical_parent.join(normalized_path.file_name().ok_or_else(|| {
                acp::Error::invalid_params().data(serde_json::json!({
                    "message": format!(
                        "filesystem path `{}` must include a file name",
                        normalized_path.display()
                    )
                }))
            })?)
        };

        self.ensure_path_within_session_cwd(
            session_id,
            &normalized_path,
            resolved_path,
            "filesystem path",
            "filesystem",
        )
    }

    fn resolve_terminal_cwd(
        &self,
        session_id: &acp::SessionId,
        requested_cwd: Option<&Path>,
    ) -> acp::Result<PathBuf> {
        match requested_cwd {
            Some(cwd) => self.resolve_existing_session_path(
                session_id,
                cwd,
                "resolve terminal cwd",
                "terminal cwd",
                "terminal",
            ),
            None => self.session_cwd(session_id, "terminal"),
        }
    }

    fn ensure_path_within_session_cwd(
        &self,
        session_id: &acp::SessionId,
        requested_path: &Path,
        resolved_path: PathBuf,
        subject: &str,
        capability_name: &str,
    ) -> acp::Result<PathBuf> {
        let session_cwd = self.session_cwd(session_id, capability_name)?;
        if resolved_path.starts_with(&session_cwd) {
            return Ok(resolved_path);
        }

        Err(acp::Error::invalid_params().data(serde_json::json!({
            "message": format!(
                "{subject} `{}` escapes session cwd `{}`",
                requested_path.display(),
                session_cwd.display()
            )
        })))
    }

    fn session_cwd(
        &self,
        session_id: &acp::SessionId,
        capability_name: &str,
    ) -> acp::Result<PathBuf> {
        self.session_cwds
            .lock()
            .expect("session cwd registry mutex poisoned")
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                acp::Error::invalid_params().data(serde_json::json!({
                    "message": format!(
                        "session `{}` does not have a registered {capability_name} cwd",
                        session_id
                    )
                }))
            })
    }
}

#[derive(Debug)]
struct ProjectionSharedState {
    projection: TuiProjectionState,
    pending_permission_request: Option<PendingPermissionRequest>,
    filesystem: AcpFilesystemService,
    terminal: AcpTerminalService,
}

impl Default for ProjectionSharedState {
    fn default() -> Self {
        let session_roots = AcpSessionRoots::default();
        Self {
            projection: TuiProjectionState::default(),
            pending_permission_request: None,
            filesystem: AcpFilesystemService::with_session_roots(session_roots.clone()),
            terminal: AcpTerminalService::with_session_roots(session_roots),
        }
    }
}

impl ProjectionSharedState {
    fn clear_pending_permission_request(&mut self) {
        if let Some(pending_request) = self.pending_permission_request.take() {
            let _ = pending_request
                .response_sender
                .send(cancelled_permission_response());
        }
        self.projection.pending_permission = None;
    }

    fn apply_permission_request(
        &mut self,
        request: acp::RequestPermissionRequest,
        response_sender: oneshot::Sender<acp::RequestPermissionResponse>,
    ) {
        if let Some(pending_request) = self.pending_permission_request.take() {
            let _ = pending_request
                .response_sender
                .send(cancelled_permission_response());
        }

        self.projection.apply_permission_request(&request);
        self.pending_permission_request = Some(PendingPermissionRequest {
            request,
            response_sender,
        });
    }

    fn take_pending_selection(
        &mut self,
        option_id: &str,
    ) -> Result<(
        oneshot::Sender<acp::RequestPermissionResponse>,
        acp::RequestPermissionResponse,
    )> {
        let pending_request = self.pending_permission_request.take().ok_or_else(|| {
            FluentCodeError::Provider("no ACP permission request is pending".to_string())
        })?;

        if !pending_request
            .request
            .options
            .iter()
            .any(|option| option.option_id.to_string() == option_id)
        {
            self.pending_permission_request = Some(pending_request);
            return Err(FluentCodeError::Provider(format!(
                "ACP permission option `{option_id}` is not available for the pending request"
            )));
        }

        self.projection.pending_permission = None;
        Ok((
            pending_request.response_sender,
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(option_id.to_string()),
            )),
        ))
    }

    fn take_pending_cancellation(
        &mut self,
    ) -> Option<(
        oneshot::Sender<acp::RequestPermissionResponse>,
        acp::RequestPermissionResponse,
    )> {
        self.pending_permission_request
            .take()
            .map(|pending_request| {
                self.projection.pending_permission = None;
                (
                    pending_request.response_sender,
                    cancelled_permission_response(),
                )
            })
    }

    fn register_session_cwd(
        &mut self,
        session_id: acp::SessionId,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        self.filesystem.register_session_cwd(session_id, cwd)
    }

    fn mark_session_created(&mut self, session_id: acp::SessionId) {
        self.clear_pending_permission_request();
        self.projection.mark_session_created(session_id);
    }

    fn prepare_session_load(&mut self, session_id: &acp::SessionId) {
        self.clear_pending_permission_request();
        self.projection.prepare_session_load(session_id);
    }

    fn set_draft_input(&mut self, draft_input: impl Into<String>) {
        self.projection.set_draft_input(draft_input);
    }

    fn mark_prompt_started(&mut self) {
        self.projection.mark_prompt_started();
    }

    fn mark_prompt_finished(&mut self) {
        self.projection.mark_prompt_finished();
    }

    fn mark_prompt_error(&mut self, message: String) {
        self.projection.mark_prompt_error(message);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProjectionClientCapabilities {
    filesystem: bool,
    terminal: bool,
}

impl ProjectionClientCapabilities {
    fn from_services(filesystem: &AcpFilesystemService, terminal: &AcpTerminalService) -> Self {
        Self {
            filesystem: filesystem.is_available(),
            terminal: terminal.is_available(),
        }
    }

    fn as_acp_client_capabilities(self) -> acp::ClientCapabilities {
        let mut capabilities = acp::ClientCapabilities::new();

        if self.filesystem {
            capabilities = capabilities.fs(acp::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true));
        }

        if self.terminal {
            capabilities = capabilities.terminal(true);
        }

        capabilities
    }
}

#[derive(Debug, Clone, Default)]
pub struct AcpFilesystemService {
    session_roots: AcpSessionRoots,
}

impl AcpFilesystemService {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_session_roots(session_roots: AcpSessionRoots) -> Self {
        Self { session_roots }
    }

    fn is_available(&self) -> bool {
        true
    }

    pub fn client_capabilities(&self) -> acp::ClientCapabilities {
        acp::ClientCapabilities::new().fs(acp::FileSystemCapabilities::new()
            .read_text_file(self.is_available())
            .write_text_file(self.is_available()))
    }

    pub fn register_session_cwd(
        &mut self,
        session_id: acp::SessionId,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        self.session_roots.register_session_cwd(session_id, cwd)
    }

    pub fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        let path = self.resolve_existing_session_path(&args.session_id, &args.path, "read")?;
        let content = fs::read_to_string(&path)
            .map_err(|error| map_filesystem_error("read text file", &path, error))?;
        let content = slice_text_lines(&content, args.line, args.limit)?;
        Ok(acp::ReadTextFileResponse::new(content))
    }

    pub fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        let path = self.resolve_writable_session_path(&args.session_id, &args.path)?;
        fs::write(&path, args.content)
            .map_err(|error| map_filesystem_error("write text file", &path, error))?;
        Ok(acp::WriteTextFileResponse::new())
    }

    fn resolve_existing_session_path(
        &self,
        session_id: &acp::SessionId,
        requested_path: &Path,
        operation: &str,
    ) -> acp::Result<PathBuf> {
        self.session_roots.resolve_existing_session_path(
            session_id,
            requested_path,
            operation,
            "filesystem path",
            "filesystem",
        )
    }

    fn resolve_writable_session_path(
        &self,
        session_id: &acp::SessionId,
        requested_path: &Path,
    ) -> acp::Result<PathBuf> {
        self.session_roots
            .resolve_writable_session_path(session_id, requested_path)
    }
}

#[derive(Debug, Clone, Default)]
pub struct AcpTerminalService {
    session_roots: AcpSessionRoots,
    terminals: Arc<Mutex<HashMap<acp::TerminalId, ManagedTerminal>>>,
}

impl AcpTerminalService {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_session_roots(session_roots: AcpSessionRoots) -> Self {
        Self {
            session_roots,
            terminals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn is_available(&self) -> bool {
        true
    }

    pub fn client_capabilities(&self) -> acp::ClientCapabilities {
        acp::ClientCapabilities::new().terminal(self.is_available())
    }

    pub fn register_session_cwd(
        &self,
        session_id: acp::SessionId,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        self.session_roots.register_session_cwd(session_id, cwd)
    }

    pub async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        let command_cwd = self
            .session_roots
            .resolve_terminal_cwd(&args.session_id, args.cwd.as_deref())?;

        let mut command = Command::new(&args.command);
        command
            .args(&args.args)
            .current_dir(&command_cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for env_var in &args.env {
            command.env(&env_var.name, &env_var.value);
        }

        let mut child = command.spawn().map_err(|error| {
            acp::Error::internal_error().data(serde_json::json!({
                "message": format!(
                    "failed to spawn terminal command `{}` in `{}`: {error}",
                    args.command,
                    command_cwd.display()
                )
            }))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            acp::Error::internal_error().data(serde_json::json!({
                "message": format!(
                    "terminal command `{}` did not provide a stdout pipe",
                    args.command
                )
            }))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            acp::Error::internal_error().data(serde_json::json!({
                "message": format!(
                    "terminal command `{}` did not provide a stderr pipe",
                    args.command
                )
            }))
        })?;

        let output = Arc::new(Mutex::new(TerminalOutputBuffer::new(
            args.output_byte_limit,
        )?));
        let reader_tasks = Arc::new(Mutex::new(TerminalReaderTasks::default()));
        {
            let mut tasks = reader_tasks.lock().await;
            tasks.stdout = Some(tokio::task::spawn_local(capture_terminal_stream(
                stdout,
                Arc::clone(&output),
            )));
            tasks.stderr = Some(tokio::task::spawn_local(capture_terminal_stream(
                stderr,
                Arc::clone(&output),
            )));
        }

        let terminal_id = acp::TerminalId::new(uuid::Uuid::new_v4().to_string());
        self.terminals.lock().await.insert(
            terminal_id.clone(),
            ManagedTerminal {
                session_id: args.session_id,
                process: Arc::new(Mutex::new(ManagedTerminalProcess::new(child))),
                output,
                reader_tasks,
            },
        );

        Ok(acp::CreateTerminalResponse::new(terminal_id))
    }

    pub async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        let terminal = self
            .terminal_for_session(&args.session_id, &args.terminal_id)
            .await?;
        let exit_status = terminal.refresh_exit_status(&args.terminal_id).await?;
        let snapshot = terminal.output_snapshot().await;
        Ok(
            acp::TerminalOutputResponse::new(snapshot.output, snapshot.truncated)
                .exit_status(exit_status),
        )
    }

    pub async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        let terminal = self
            .remove_terminal_for_session(&args.session_id, &args.terminal_id)
            .await?;
        terminal.release(&args.terminal_id).await?;
        Ok(acp::ReleaseTerminalResponse::new())
    }

    pub async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        let terminal = self
            .terminal_for_session(&args.session_id, &args.terminal_id)
            .await?;
        let exit_status = terminal.wait_for_exit(&args.terminal_id).await?;
        Ok(acp::WaitForTerminalExitResponse::new(exit_status))
    }

    pub async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        let terminal = self
            .terminal_for_session(&args.session_id, &args.terminal_id)
            .await?;
        terminal.kill(&args.terminal_id).await?;
        Ok(acp::KillTerminalResponse::new())
    }

    async fn terminal_for_session(
        &self,
        session_id: &acp::SessionId,
        terminal_id: &acp::TerminalId,
    ) -> acp::Result<ManagedTerminal> {
        let terminal = self
            .terminals
            .lock()
            .await
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| missing_terminal_error(terminal_id))?;
        ensure_terminal_session(session_id, terminal_id, &terminal)?;
        Ok(terminal)
    }

    async fn remove_terminal_for_session(
        &self,
        session_id: &acp::SessionId,
        terminal_id: &acp::TerminalId,
    ) -> acp::Result<ManagedTerminal> {
        let mut terminals = self.terminals.lock().await;
        let terminal = terminals
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| missing_terminal_error(terminal_id))?;
        ensure_terminal_session(session_id, terminal_id, &terminal)?;
        terminals
            .remove(terminal_id)
            .ok_or_else(|| missing_terminal_error(terminal_id))
    }
}

#[derive(Debug, Clone)]
struct ManagedTerminal {
    session_id: acp::SessionId,
    process: Arc<Mutex<ManagedTerminalProcess>>,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    reader_tasks: Arc<Mutex<TerminalReaderTasks>>,
}

impl ManagedTerminal {
    async fn output_snapshot(&self) -> TerminalOutputSnapshot {
        self.output.lock().await.snapshot()
    }

    async fn refresh_exit_status(
        &self,
        terminal_id: &acp::TerminalId,
    ) -> acp::Result<Option<acp::TerminalExitStatus>> {
        let mut process = self.process.lock().await;
        if process.exit_status.is_none()
            && let Some(status) = process.child.try_wait().map_err(|error| {
                map_terminal_runtime_error("check terminal status", terminal_id, error)
            })?
        {
            process.exit_status = Some(terminal_exit_status(status));
        }

        Ok(process.exit_status.clone())
    }

    async fn wait_for_exit(
        &self,
        terminal_id: &acp::TerminalId,
    ) -> acp::Result<acp::TerminalExitStatus> {
        let exit_status = {
            let mut process = self.process.lock().await;
            if let Some(exit_status) = &process.exit_status {
                exit_status.clone()
            } else {
                let status = process.child.wait().await.map_err(|error| {
                    map_terminal_runtime_error("wait for terminal exit", terminal_id, error)
                })?;
                let exit_status = terminal_exit_status(status);
                process.exit_status = Some(exit_status.clone());
                exit_status
            }
        };

        self.await_reader_tasks(terminal_id).await?;
        Ok(exit_status)
    }

    async fn kill(&self, terminal_id: &acp::TerminalId) -> acp::Result<()> {
        {
            let mut process = self.process.lock().await;
            if process.exit_status.is_none() {
                if let Some(status) = process.child.try_wait().map_err(|error| {
                    map_terminal_runtime_error("check terminal status", terminal_id, error)
                })? {
                    process.exit_status = Some(terminal_exit_status(status));
                } else {
                    process.child.kill().await.map_err(|error| {
                        map_terminal_runtime_error("kill terminal", terminal_id, error)
                    })?;
                    let status = process.child.wait().await.map_err(|error| {
                        map_terminal_runtime_error("wait for killed terminal", terminal_id, error)
                    })?;
                    process.exit_status = Some(terminal_exit_status(status));
                }
            }
        }

        self.await_reader_tasks(terminal_id).await
    }

    async fn release(&self, terminal_id: &acp::TerminalId) -> acp::Result<()> {
        self.kill(terminal_id).await
    }

    async fn await_reader_tasks(&self, terminal_id: &acp::TerminalId) -> acp::Result<()> {
        let mut tasks = self.reader_tasks.lock().await;
        let stdout_task = tasks.stdout.take();
        let stderr_task = tasks.stderr.take();
        drop(tasks);

        if let Some(stdout_task) = stdout_task {
            let result = stdout_task.await.map_err(|error| {
                acp::Error::internal_error().data(serde_json::json!({
                    "message": format!(
                        "terminal `{terminal_id}` output task failed to join: {error}"
                    )
                }))
            })?;
            result.map_err(|error| {
                map_terminal_runtime_error("read terminal stdout", terminal_id, error)
            })?;
        }

        if let Some(stderr_task) = stderr_task {
            let result = stderr_task.await.map_err(|error| {
                acp::Error::internal_error().data(serde_json::json!({
                    "message": format!(
                        "terminal `{terminal_id}` error-output task failed to join: {error}"
                    )
                }))
            })?;
            result.map_err(|error| {
                map_terminal_runtime_error("read terminal stderr", terminal_id, error)
            })?;
        }

        Ok(())
    }
}

#[derive(Debug)]
struct ManagedTerminalProcess {
    child: Child,
    exit_status: Option<acp::TerminalExitStatus>,
}

impl ManagedTerminalProcess {
    fn new(child: Child) -> Self {
        Self {
            child,
            exit_status: None,
        }
    }
}

#[derive(Debug, Default)]
struct TerminalReaderTasks {
    stdout: Option<JoinHandle<std::io::Result<()>>>,
    stderr: Option<JoinHandle<std::io::Result<()>>>,
}

#[derive(Debug)]
struct TerminalOutputBuffer {
    output: String,
    truncated: bool,
    output_byte_limit: Option<usize>,
}

impl TerminalOutputBuffer {
    fn new(output_byte_limit: Option<u64>) -> acp::Result<Self> {
        let output_byte_limit = output_byte_limit
            .map(|value| usize::try_from(value).map_err(|_| acp::Error::invalid_params()))
            .transpose()?;
        Ok(Self {
            output: String::new(),
            truncated: false,
            output_byte_limit,
        })
    }

    fn append_chunk(&mut self, chunk: &[u8]) {
        self.output.push_str(&String::from_utf8_lossy(chunk));
        self.enforce_byte_limit();
    }

    fn snapshot(&self) -> TerminalOutputSnapshot {
        TerminalOutputSnapshot {
            output: self.output.clone(),
            truncated: self.truncated,
        }
    }

    fn enforce_byte_limit(&mut self) {
        let Some(limit) = self.output_byte_limit else {
            return;
        };

        if limit == 0 {
            if !self.output.is_empty() {
                self.output.clear();
                self.truncated = true;
            }
            return;
        }

        let excess_bytes = self.output.len().saturating_sub(limit);
        if excess_bytes == 0 {
            return;
        }

        let mut split_index = excess_bytes;
        while split_index < self.output.len() && !self.output.is_char_boundary(split_index) {
            split_index += 1;
        }
        self.output.drain(..split_index);
        self.truncated = true;
    }
}

#[derive(Debug)]
struct TerminalOutputSnapshot {
    output: String,
    truncated: bool,
}

async fn capture_terminal_stream<R>(
    mut reader: R,
    output: Arc<Mutex<TerminalOutputBuffer>>,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(());
        }
        output.lock().await.append_chunk(&buffer[..bytes_read]);
    }
}

fn ensure_terminal_session(
    session_id: &acp::SessionId,
    terminal_id: &acp::TerminalId,
    terminal: &ManagedTerminal,
) -> acp::Result<()> {
    if &terminal.session_id == session_id {
        return Ok(());
    }

    Err(acp::Error::invalid_params().data(serde_json::json!({
        "message": format!(
            "terminal `{terminal_id}` does not belong to session `{session_id}`"
        )
    })))
}

fn missing_terminal_error(terminal_id: &acp::TerminalId) -> acp::Error {
    acp::Error::resource_not_found(Some(format!("terminal `{terminal_id}`")))
}

fn map_terminal_runtime_error(
    action: &str,
    terminal_id: &acp::TerminalId,
    error: std::io::Error,
) -> acp::Error {
    acp::Error::internal_error().data(serde_json::json!({
        "message": format!("failed to {action} for terminal `{terminal_id}`: {error}")
    }))
}

fn terminal_exit_status(status: std::process::ExitStatus) -> acp::TerminalExitStatus {
    let mut exit_status = acp::TerminalExitStatus::new()
        .exit_code(status.code().and_then(|code| u32::try_from(code).ok()));

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        exit_status = exit_status.signal(status.signal().map(|signal| signal.to_string()));
    }

    exit_status
}

#[derive(Debug, Clone)]
pub struct AcpLaunchOptions {
    pub binary_path: PathBuf,
    pub cwd: PathBuf,
}

impl AcpLaunchOptions {
    pub fn new(binary_path: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            cwd: cwd.into(),
        }
    }

    fn from_current_process() -> Result<Self> {
        Ok(Self {
            binary_path: default_acp_binary_path()?,
            cwd: std::env::current_dir()?,
        })
    }
}

struct AcpClientRuntimeInner {
    projection: Arc<Mutex<ProjectionSharedState>>,
    capabilities: ProjectionClientCapabilities,
}

impl AcpClientRuntimeInner {
    fn new(
        projection: Arc<Mutex<ProjectionSharedState>>,
        capabilities: ProjectionClientCapabilities,
    ) -> Self {
        Self {
            projection,
            capabilities,
        }
    }

    fn projection(&self) -> &Arc<Mutex<ProjectionSharedState>> {
        &self.projection
    }

    fn client_capabilities(&self) -> acp::ClientCapabilities {
        self.capabilities.as_acp_client_capabilities()
    }

    async fn projection_snapshot(&self) -> TuiProjectionState {
        self.projection.lock().await.projection.clone()
    }

    async fn register_session_cwd(
        &self,
        session_id: acp::SessionId,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        self.projection
            .lock()
            .await
            .register_session_cwd(session_id, cwd)
    }

    async fn mark_session_created(&self, session_id: acp::SessionId) {
        self.projection
            .lock()
            .await
            .mark_session_created(session_id);
    }

    async fn prepare_session_load(&self, session_id: &acp::SessionId) {
        self.projection
            .lock()
            .await
            .prepare_session_load(session_id);
    }

    async fn select_permission_option(&self, option_id: &str) -> Result<()> {
        resolve_pending_permission_selection(self.projection(), option_id).await
    }

    async fn cancel_pending_permission(&self) -> Result<()> {
        cancel_pending_permission(self.projection()).await
    }

    fn ensure_filesystem_enabled(&self) -> acp::Result<()> {
        if self.capabilities.filesystem {
            Ok(())
        } else {
            Err(acp::Error::method_not_found())
        }
    }

    fn ensure_terminal_enabled(&self) -> acp::Result<()> {
        if self.capabilities.terminal {
            Ok(())
        } else {
            Err(acp::Error::method_not_found())
        }
    }

    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.projection
            .lock()
            .await
            .apply_permission_request(args, response_sender);

        Ok(response_receiver
            .await
            .unwrap_or_else(|_| cancelled_permission_response()))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.projection
            .lock()
            .await
            .projection
            .apply_session_notification(args);
        Ok(())
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        self.ensure_filesystem_enabled()?;
        self.projection
            .lock()
            .await
            .filesystem
            .write_text_file(args)
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        self.ensure_filesystem_enabled()?;
        self.projection.lock().await.filesystem.read_text_file(args)
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        self.ensure_terminal_enabled()?;

        let terminal = self.projection.lock().await.terminal.clone();
        terminal.create_terminal(args).await
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        self.ensure_terminal_enabled()?;

        let terminal = self.projection.lock().await.terminal.clone();
        terminal.terminal_output(args).await
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        self.ensure_terminal_enabled()?;

        let terminal = self.projection.lock().await.terminal.clone();
        terminal.release_terminal(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        self.ensure_terminal_enabled()?;

        let terminal = self.projection.lock().await.terminal.clone();
        terminal.wait_for_terminal_exit(args).await
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        self.ensure_terminal_enabled()?;

        let terminal = self.projection.lock().await.terminal.clone();
        terminal.kill_terminal(args).await
    }
}

#[async_trait(?Send)]
impl acp::Client for AcpClientRuntimeInner {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        AcpClientRuntimeInner::request_permission(self, args).await
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        AcpClientRuntimeInner::session_notification(self, args).await
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        AcpClientRuntimeInner::write_text_file(self, args).await
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        AcpClientRuntimeInner::read_text_file(self, args).await
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        AcpClientRuntimeInner::create_terminal(self, args).await
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        AcpClientRuntimeInner::terminal_output(self, args).await
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        AcpClientRuntimeInner::release_terminal(self, args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        AcpClientRuntimeInner::wait_for_terminal_exit(self, args).await
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        AcpClientRuntimeInner::kill_terminal(self, args).await
    }
}

pub struct AcpClientRuntime {
    inner: Arc<AcpClientRuntimeInner>,
    initialize_response: acp::InitializeResponse,
    connection: StdArc<acp::ClientSideConnection>,
    io_task: JoinHandle<acp::Result<()>>,
    child: Child,
}

impl AcpClientRuntime {
    pub fn initialize_response(&self) -> &acp::InitializeResponse {
        &self.initialize_response
    }

    pub async fn projection_snapshot(&self) -> TuiProjectionState {
        self.inner.projection_snapshot().await
    }

    #[doc(hidden)]
    pub async fn new_session(
        &self,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<acp::NewSessionResponse> {
        let cwd = cwd.into();
        let response = self
            .connection
            .new_session(acp::NewSessionRequest::new(cwd.clone()))
            .await?;
        self.inner
            .register_session_cwd(response.session_id.clone(), cwd)
            .await?;
        self.inner
            .mark_session_created(response.session_id.clone())
            .await;
        Ok(response)
    }

    #[doc(hidden)]
    pub async fn load_session(
        &self,
        session_id: impl Into<acp::SessionId>,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<acp::LoadSessionResponse> {
        let session_id = session_id.into();
        let cwd = cwd.into();
        self.inner.prepare_session_load(&session_id).await;
        let response = self
            .connection
            .load_session(acp::LoadSessionRequest::new(
                session_id.clone(),
                cwd.clone(),
            ))
            .await?;
        self.inner.register_session_cwd(session_id, cwd).await?;
        Ok(response)
    }

    #[doc(hidden)]
    pub async fn prompt(
        &self,
        session_id: impl Into<acp::SessionId>,
        prompt: impl Into<String>,
    ) -> acp::Result<acp::PromptResponse> {
        self.connection
            .prompt(acp::PromptRequest::new(
                session_id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    prompt.into(),
                ))],
            ))
            .await
    }

    async fn cancel(&self, session_id: impl Into<acp::SessionId>) -> acp::Result<()> {
        self.connection
            .cancel(acp::CancelNotification::new(session_id.into()))
            .await
    }

    #[doc(hidden)]
    pub async fn write_text_file_via_acp_for_tests(
        &self,
        session_id: impl Into<acp::SessionId>,
        path: impl Into<PathBuf>,
        content: impl Into<String>,
    ) -> acp::Result<()> {
        self.ext_request::<_, serde_json::Value>(
            "fluent_code/test/write_text_file",
            &FilesystemWriteProbeRequest {
                session_id: session_id.into().to_string(),
                path: path.into(),
                content: content.into(),
            },
        )
        .await
        .map(|_| ())
    }

    #[doc(hidden)]
    pub async fn read_text_file_via_acp_for_tests(
        &self,
        session_id: impl Into<acp::SessionId>,
        path: impl Into<PathBuf>,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> acp::Result<String> {
        self.ext_request::<_, FilesystemReadProbeResponse>(
            "fluent_code/test/read_text_file",
            &FilesystemReadProbeRequest {
                session_id: session_id.into().to_string(),
                path: path.into(),
                line,
                limit,
            },
        )
        .await
        .map(|response| response.content)
    }

    #[doc(hidden)]
    pub async fn run_terminal_command_via_acp_for_tests(
        &self,
        session_id: impl Into<acp::SessionId>,
        command: impl Into<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<u32>,
    ) -> acp::Result<TerminalCommandProbeResponse> {
        self.ext_request(
            "fluent_code/test/run_terminal_command",
            &TerminalCommandProbeRequest {
                session_id: session_id.into().to_string(),
                command: command.into(),
                args,
                cwd,
                output_byte_limit,
            },
        )
        .await
    }

    async fn ext_request<TRequest, TResponse>(
        &self,
        method: &str,
        params: &TRequest,
    ) -> acp::Result<TResponse>
    where
        TRequest: Serialize,
        TResponse: DeserializeOwned,
    {
        let params = serde_json::value::to_raw_value(params).map_err(|error| {
            acp::Error::invalid_params().data(serde_json::json!({
                "message": format!("failed to encode ACP test request params: {error}")
            }))
        })?;
        let response = self
            .connection
            .ext_method(acp::ExtRequest::new(method.to_string(), params.into()))
            .await?;
        serde_json::from_str::<TResponse>(response.0.get()).map_err(|error| {
            acp::Error::internal_error().data(serde_json::json!({
                "message": format!("failed to decode ACP test response payload: {error}")
            }))
        })
    }

    #[doc(hidden)]
    pub async fn select_permission_option_for_tests(&self, option_id: &str) -> Result<()> {
        self.inner.select_permission_option(option_id).await
    }

    #[doc(hidden)]
    pub async fn cancel_pending_permission_for_tests(&self) -> Result<()> {
        self.inner.cancel_pending_permission().await
    }

    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.connection);
        self.io_task.abort();
        let _ = self.io_task.await;

        match timeout(ACP_SHUTDOWN_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(FluentCodeError::Provider(format!(
                "ACP subprocess exited unsuccessfully: {status}"
            ))),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => {
                terminate_child(&mut self.child).await;
                Ok(())
            }
        }
    }
}

#[async_trait(?Send)]
impl acp::Client for AcpClientRuntime {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        self.inner.request_permission(args).await
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.inner.session_notification(args).await
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        self.inner.write_text_file(args).await
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        self.inner.read_text_file(args).await
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        self.inner.create_terminal(args).await
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        self.inner.terminal_output(args).await
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        self.inner.release_terminal(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        self.inner.wait_for_terminal_exit(args).await
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        self.inner.kill_terminal(args).await
    }
}

#[doc(hidden)]
pub async fn bootstrap_client_for_tests(options: AcpLaunchOptions) -> Result<AcpClientRuntime> {
    bootstrap_client(
        options,
        Arc::new(Mutex::new(ProjectionSharedState::default())),
        true,
    )
    .await
}

#[doc(hidden)]
pub async fn initialize_default_session_for_tests(
    runtime: &AcpClientRuntime,
    cwd: impl AsRef<Path>,
) -> Result<()> {
    initialize_default_session(runtime, cwd.as_ref()).await
}

pub async fn run() -> Result<()> {
    let mut terminal = terminal::init()?;

    let app_result = tokio::task::LocalSet::new()
        .run_until(async { run_projection_client(&mut terminal).await })
        .await;
    let restore_result = terminal::restore(terminal);

    match (app_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Err(error), Err(_)) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn run_projection_client(terminal: &mut terminal::AppTerminal) -> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let launch_options = AcpLaunchOptions::from_current_process()?;
    let runtime =
        match bootstrap_client(launch_options.clone(), Arc::clone(&projection), false).await {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                projection
                    .lock()
                    .await
                    .projection
                    .mark_startup_error(error.to_string());
                None
            }
        };

    if let Some(runtime) = runtime.as_ref()
        && let Err(error) = initialize_default_session(runtime, &launch_options.cwd).await
    {
        projection
            .lock()
            .await
            .projection
            .mark_startup_error(error.to_string());
    }

    let mut controller =
        ProjectionController::new(Arc::clone(&projection), runtime, launch_options.cwd.clone());
    run_projection_loop(terminal, &mut controller).await?;

    if let Some(runtime) = controller.into_runtime() {
        runtime.shutdown().await?;
    }

    Ok(())
}

async fn initialize_default_session(runtime: &AcpClientRuntime, cwd: &Path) -> Result<()> {
    if let Some(session_id) = read_latest_session_id_from_config()? {
        match runtime
            .load_session(session_id.clone(), cwd.to_path_buf())
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if error.code == acp::ErrorCode::ResourceNotFound => {}
            Err(error) => {
                return Err(FluentCodeError::Provider(format!(
                    "failed to load latest ACP session `{session_id}`: {error}"
                )));
            }
        }
    }

    let new_session = runtime
        .new_session(cwd.to_path_buf())
        .await
        .map_err(|error| {
            FluentCodeError::Provider(format!(
                "failed to create default ACP session in `{}`: {error}",
                cwd.display()
            ))
        })?;
    runtime
        .load_session(new_session.session_id, cwd.to_path_buf())
        .await
        .map(|_| ())
        .map_err(|error| {
            FluentCodeError::Provider(format!(
                "failed to load newly created ACP session in `{}`: {error}",
                cwd.display()
            ))
        })
}

fn read_latest_session_id_from_config() -> Result<Option<acp::SessionId>> {
    let latest_session_path = Config::load()?.data_dir.join("latest_session");
    if !latest_session_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&latest_session_path)?;
    let session_id = contents.trim();
    if session_id.is_empty() {
        return Ok(None);
    }

    uuid::Uuid::parse_str(session_id)
        .map_err(|error| FluentCodeError::Session(format!("invalid latest session id: {error}")))?;

    Ok(Some(acp::SessionId::new(session_id.to_string())))
}

async fn run_projection_loop(
    terminal: &mut terminal::AppTerminal,
    controller: &mut ProjectionController,
) -> Result<()> {
    loop {
        if run_projection_iteration(
            controller,
            |snapshot| {
                terminal.draw(|frame| render_projection(frame, snapshot))?;
                Ok(())
            },
            next_projection_action,
        )
        .await?
        {
            break;
        }
    }

    Ok(())
}

async fn run_projection_iteration<Draw, NextAction>(
    controller: &mut ProjectionController,
    draw: Draw,
    next_action: NextAction,
) -> Result<bool>
where
    Draw: FnOnce(&TuiProjectionState) -> Result<()>,
    NextAction: FnOnce(&TuiProjectionState) -> Result<ProjectionAction>,
{
    controller.poll_active_prompt().await?;
    let snapshot = controller.snapshot().await;
    draw(&snapshot)?;
    apply_projection_action(controller, next_action(&snapshot)?).await
}

async fn apply_projection_action(
    controller: &mut ProjectionController,
    action: ProjectionAction,
) -> Result<bool> {
    match action {
        ProjectionAction::None => Ok(false),
        ProjectionAction::Quit => Ok(true),
        ProjectionAction::NewSession => {
            controller.create_new_session().await?;
            Ok(false)
        }
        ProjectionAction::UpdateDraft(draft_input) => {
            controller.set_draft_input(draft_input).await;
            Ok(false)
        }
        ProjectionAction::SubmitPrompt => {
            controller.submit_prompt().await?;
            Ok(false)
        }
        ProjectionAction::Select(option_id) => {
            resolve_pending_permission_selection(controller.projection(), &option_id).await?;
            Ok(false)
        }
        ProjectionAction::CancelPendingPermission => {
            cancel_pending_permission(controller.projection()).await?;
            Ok(false)
        }
        ProjectionAction::CancelActivePrompt => {
            controller.cancel_prompt().await?;
            Ok(false)
        }
    }
}

fn render_projection(frame: &mut Frame, projection: &TuiProjectionState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let status = Paragraph::new(Line::from(vec![
        Span::styled(" fluent-code ", TUI_THEME.title),
        Span::styled("│ ", TUI_THEME.text_muted),
        Span::styled(status_label(projection), status_style(projection)),
    ]));
    frame.render_widget(status, layout[0]);

    let body = Paragraph::new(Text::from(projection_lines(projection)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border)
                .title(Span::styled(" acp client ", TUI_THEME.title)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(body, layout[1]);

    let input = Paragraph::new(projection.draft_input.as_str())
        .style(TUI_THEME.text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border_active)
                .title(Span::styled(" > ", TUI_THEME.title)),
        );
    frame.render_widget(input, layout[2]);

    let footer = Paragraph::new(footer_label(projection)).style(TUI_THEME.text_muted);
    frame.render_widget(footer, layout[3]);

    if projection.can_edit_draft() {
        let cursor_x = layout[2]
            .x
            .saturating_add(projection.draft_input.chars().count() as u16 + 1);
        let cursor_y = layout[2].y.saturating_add(1);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn footer_label(projection: &TuiProjectionState) -> &'static str {
    if projection.pending_permission.is_some() {
        "Permission: Enter/y allow once, a allow always, n reject once, r reject always, q/Esc/Ctrl-C cancel."
    } else if projection.prompt_in_flight {
        "Prompt running through ACP. Esc/Ctrl-C cancels the active turn."
    } else {
        "Type a prompt and press Enter. Ctrl-N starts a new ACP session. q/Esc/Ctrl-C exits."
    }
}

fn projection_lines(projection: &TuiProjectionState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(line_from_status(&projection.subprocess.status));

    if let Some(session_id) = &projection.session.session_id {
        lines.push(Line::from(format!("Session: {session_id}")));
    } else {
        lines.push(Line::from("Session: not created yet"));
    }

    if let Some(title) = &projection.session.title {
        lines.push(Line::from(format!("Title: {title}")));
    }

    lines.push(Line::from(format!(
        "Transcript rows: {}  |  Tool updates: {}",
        projection.transcript_rows.len(),
        projection.tool_statuses.len()
    )));

    if projection.prompt_in_flight {
        lines.push(Line::from("Prompt status: running"));
    }

    if let Some(permission) = &projection.pending_permission {
        lines.push(Line::default());
        lines.push(Line::styled("Pending permission", TUI_THEME.warning));
        lines.push(Line::from(format!(
            "  {} ({})",
            permission.title, permission.tool_call_id
        )));
        for option in &permission.options {
            lines.push(Line::from(format!(
                "  - {} [{}]",
                option.name, option.option_id
            )));
        }
    }

    if let Some(error) = &projection.startup_error {
        lines.push(Line::default());
        lines.push(Line::styled("Startup error", TUI_THEME.error));
        lines.push(Line::from(format!("  {error}")));
    }

    if let Some(error) = &projection.prompt_error {
        lines.push(Line::default());
        lines.push(Line::styled("Prompt error", TUI_THEME.error));
        lines.push(Line::from(format!("  {error}")));
    }

    if !projection.transcript_rows.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled("Recent transcript", TUI_THEME.text_muted));
        for row in projection.transcript_rows.iter().rev().take(5).rev() {
            let label = match row.source {
                TranscriptSource::User => "you",
                TranscriptSource::Agent => "assistant",
                TranscriptSource::Thought => "reasoning",
            };
            lines.push(Line::from(format!("  {label}: {}", row.content)));
        }
    }

    lines
}

fn line_from_status(status: &SubprocessStatus) -> Line<'static> {
    match status {
        SubprocessStatus::NotStarted => Line::from("ACP subprocess: not started"),
        SubprocessStatus::Spawned { binary_path, pid } => Line::from(format!(
            "ACP subprocess: spawned {} (pid {pid})",
            binary_path.display()
        )),
        SubprocessStatus::Initialized {
            binary_path,
            pid,
            protocol_version,
        } => Line::from(format!(
            "ACP subprocess: initialized {} (pid {pid}, protocol v{protocol_version})",
            binary_path.display()
        )),
        SubprocessStatus::Failed { message } => {
            Line::from(format!("ACP subprocess: failed ({message})"))
        }
    }
}

fn status_label(projection: &TuiProjectionState) -> &'static str {
    if projection.startup_error.is_some() {
        return "startup error";
    }

    match projection.subprocess.status {
        SubprocessStatus::Initialized { .. } => "acp connected",
        SubprocessStatus::Spawned { .. } => "acp starting",
        SubprocessStatus::Failed { .. } => "startup error",
        SubprocessStatus::NotStarted => "bootstrapping",
    }
}

fn status_style(projection: &TuiProjectionState) -> ratatui::style::Style {
    if projection.startup_error.is_some() {
        return TUI_THEME.error;
    }

    match projection.subprocess.status {
        SubprocessStatus::Initialized { .. } => TUI_THEME.assistant_accent,
        SubprocessStatus::Spawned { .. } | SubprocessStatus::NotStarted => TUI_THEME.warning,
        SubprocessStatus::Failed { .. } => TUI_THEME.error,
    }
}

async fn bootstrap_client(
    options: AcpLaunchOptions,
    projection: Arc<Mutex<ProjectionSharedState>>,
    enable_test_probes: bool,
) -> Result<AcpClientRuntime> {
    let mut command = Command::new(&options.binary_path);
    command
        .current_dir(&options.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if enable_test_probes {
        command.env(ACP_TEST_PROBES_ENV_VAR, "1");
    }
    let mut child = command.spawn().map_err(|error| {
        FluentCodeError::Config(format!(
            "failed to launch ACP subprocess `{}`: {error}",
            options.binary_path.display()
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "ACP subprocess `{}` did not expose a process id",
            options.binary_path.display()
        ))
    })?;
    projection
        .lock()
        .await
        .projection
        .mark_spawned(options.binary_path.clone(), pid);

    let stdin = child.stdin.take().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "ACP subprocess `{}` did not provide a piped stdin handle",
            options.binary_path.display()
        ))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "ACP subprocess `{}` did not provide a piped stdout handle",
            options.binary_path.display()
        ))
    })?;

    let projection_capabilities = {
        let state = projection.lock().await;
        ProjectionClientCapabilities::from_services(&state.filesystem, &state.terminal)
    };
    let inner = Arc::new(AcpClientRuntimeInner::new(
        Arc::clone(&projection),
        projection_capabilities,
    ));
    let client_capabilities = inner.client_capabilities();
    let (connection, io_future) = acp::ClientSideConnection::new(
        Arc::clone(&inner),
        stdin.compat_write(),
        stdout.compat(),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    let connection = StdArc::new(connection);
    let io_task = tokio::task::spawn_local(io_future);

    let initialize_request = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_info(
            acp::Implementation::new("fluent-code-tui", env!("CARGO_PKG_VERSION"))
                .title("fluent-code TUI"),
        )
        .client_capabilities(client_capabilities);

    let initialize_response = match timeout(
        ACP_INITIALIZE_TIMEOUT,
        connection.initialize(initialize_request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            terminate_child(&mut child).await;
            io_task.abort();
            let _ = io_task.await;
            return Err(FluentCodeError::Provider(format!(
                "failed to initialize ACP connection over `{}`: {error}",
                options.binary_path.display()
            )));
        }
        Err(_) => {
            terminate_child(&mut child).await;
            io_task.abort();
            let _ = io_task.await;
            return Err(FluentCodeError::Provider(format!(
                "timed out waiting for ACP initialize response from `{}`",
                options.binary_path.display()
            )));
        }
    };

    projection.lock().await.projection.mark_initialized(
        options.binary_path.clone(),
        pid,
        &initialize_response,
    );

    info!(
        acp_binary = %options.binary_path.display(),
        protocol_version = %protocol_version_label(initialize_response.protocol_version.clone()),
        "initialized TUI ACP client subprocess"
    );

    Ok(AcpClientRuntime {
        inner,
        initialize_response,
        connection,
        io_task,
        child,
    })
}

async fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) | Err(_) => {}
        Ok(None) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectionAction {
    None,
    Quit,
    NewSession,
    UpdateDraft(String),
    SubmitPrompt,
    Select(String),
    CancelPendingPermission,
    CancelActivePrompt,
}

fn next_projection_action(snapshot: &TuiProjectionState) -> Result<ProjectionAction> {
    if !event::poll(projection_poll_timeout(snapshot))? {
        return Ok(ProjectionAction::None);
    }

    let event = event::read()?;
    Ok(match event {
        Event::Paste(text) if snapshot.can_edit_draft() => {
            let mut next = snapshot.draft_input.clone();
            next.push_str(&text);
            ProjectionAction::UpdateDraft(next)
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if snapshot.pending_permission.is_some() {
                permission_action_for_key(snapshot, key.code, key.modifiers)
            } else if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                if snapshot.prompt_in_flight {
                    ProjectionAction::CancelActivePrompt
                } else {
                    ProjectionAction::Quit
                }
            } else if key.code == KeyCode::Char('n')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !snapshot.prompt_in_flight
            {
                ProjectionAction::NewSession
            } else if snapshot.can_edit_draft() {
                draft_action_for_key(snapshot, key.code, key.modifiers)
            } else {
                ProjectionAction::None
            }
        }
        _ => ProjectionAction::None,
    })
}

fn projection_poll_timeout(snapshot: &TuiProjectionState) -> Duration {
    if snapshot.prompt_in_flight {
        PROMPT_IN_FLIGHT_REDRAW_CADENCE
    } else {
        PROJECTION_IDLE_INPUT_POLL
    }
}

fn permission_action_for_key(
    snapshot: &TuiProjectionState,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ProjectionAction {
    if matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
    {
        return ProjectionAction::CancelPendingPermission;
    }

    if !modifiers.is_empty() {
        return ProjectionAction::None;
    }

    match code {
        KeyCode::Enter | KeyCode::Char('y') => permission_option_action(snapshot, "allow_once"),
        KeyCode::Char('a') => permission_option_action(snapshot, "allow_always"),
        KeyCode::Char('n') => permission_option_action(snapshot, "reject_once"),
        KeyCode::Char('r') => permission_option_action(snapshot, "reject_always"),
        _ => ProjectionAction::None,
    }
}

fn draft_action_for_key(
    snapshot: &TuiProjectionState,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ProjectionAction {
    match code {
        KeyCode::Enter => ProjectionAction::SubmitPrompt,
        KeyCode::Backspace if modifiers.is_empty() => {
            let mut next = snapshot.draft_input.clone();
            next.pop();
            ProjectionAction::UpdateDraft(next)
        }
        KeyCode::Char(ch) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
            let mut next = snapshot.draft_input.clone();
            next.push(ch);
            ProjectionAction::UpdateDraft(next)
        }
        _ => ProjectionAction::None,
    }
}

fn permission_option_action(snapshot: &TuiProjectionState, option_id: &str) -> ProjectionAction {
    snapshot
        .pending_permission
        .as_ref()
        .and_then(|permission| {
            permission
                .options
                .iter()
                .any(|option| option.option_id == option_id)
                .then(|| ProjectionAction::Select(option_id.to_string()))
        })
        .unwrap_or(ProjectionAction::None)
}

async fn resolve_pending_permission_selection(
    projection: &Arc<Mutex<ProjectionSharedState>>,
    option_id: &str,
) -> Result<()> {
    let (response_sender, response) = projection.lock().await.take_pending_selection(option_id)?;
    let _ = response_sender.send(response);
    Ok(())
}

async fn cancel_pending_permission(projection: &Arc<Mutex<ProjectionSharedState>>) -> Result<()> {
    if let Some((response_sender, response)) = projection.lock().await.take_pending_cancellation() {
        let _ = response_sender.send(response);
    }

    Ok(())
}

fn cancelled_permission_response() -> acp::RequestPermissionResponse {
    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled)
}

struct ProjectionController {
    projection: Arc<Mutex<ProjectionSharedState>>,
    runtime: Option<AcpClientRuntime>,
    cwd: PathBuf,
    active_prompt: Option<ActivePromptRequest>,
}

struct ActivePromptRequest {
    session_id: acp::SessionId,
    task: JoinHandle<acp::Result<acp::PromptResponse>>,
}

impl ProjectionController {
    fn new(
        projection: Arc<Mutex<ProjectionSharedState>>,
        runtime: Option<AcpClientRuntime>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            projection,
            runtime,
            cwd,
            active_prompt: None,
        }
    }

    fn projection(&self) -> &Arc<Mutex<ProjectionSharedState>> {
        &self.projection
    }

    fn into_runtime(self) -> Option<AcpClientRuntime> {
        self.runtime
    }

    async fn snapshot(&self) -> TuiProjectionState {
        self.projection.lock().await.projection.clone()
    }

    async fn set_draft_input(&self, draft_input: impl Into<String>) {
        self.projection.lock().await.set_draft_input(draft_input);
    }

    async fn create_new_session(&mut self) -> Result<()> {
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(());
        };

        let new_session = runtime
            .new_session(self.cwd.clone())
            .await
            .map_err(|error| {
                FluentCodeError::Provider(format!(
                    "failed to create ACP session in `{}`: {error}",
                    self.cwd.display()
                ))
            })?;
        runtime
            .load_session(new_session.session_id, self.cwd.clone())
            .await
            .map(|_| ())
            .map_err(|error| {
                FluentCodeError::Provider(format!(
                    "failed to load newly created ACP session in `{}`: {error}",
                    self.cwd.display()
                ))
            })
    }

    async fn submit_prompt(&mut self) -> Result<()> {
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(());
        };
        if self.active_prompt.is_some() {
            return Ok(());
        }

        let (session_id, prompt) = {
            let mut state = self.projection.lock().await;
            let prompt = state.projection.draft_input.trim().to_string();
            if prompt.is_empty() {
                return Ok(());
            }

            let Some(session_id) = state.projection.session.session_id.clone() else {
                return Ok(());
            };
            state.mark_prompt_started();
            (acp::SessionId::new(session_id), prompt)
        };

        let connection = StdArc::clone(&runtime.connection);
        let prompt_session_id = session_id.clone();
        let task = tokio::task::spawn_local(async move {
            connection
                .prompt(acp::PromptRequest::new(
                    prompt_session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                ))
                .await
        });
        self.active_prompt = Some(ActivePromptRequest { task, session_id });
        Ok(())
    }

    async fn cancel_prompt(&mut self) -> Result<()> {
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(());
        };
        let Some(active_prompt) = self.active_prompt.as_ref() else {
            return Ok(());
        };

        runtime
            .cancel(active_prompt.session_id.clone())
            .await
            .map_err(|error| {
                FluentCodeError::Provider(format!(
                    "failed to cancel ACP prompt turn for session `{}`: {error}",
                    active_prompt.session_id
                ))
            })
    }

    async fn poll_active_prompt(&mut self) -> Result<()> {
        let Some(active_prompt) = self.active_prompt.as_ref() else {
            return Ok(());
        };
        if !active_prompt.task.is_finished() {
            return Ok(());
        }

        let active_prompt = self
            .active_prompt
            .take()
            .expect("active prompt should exist");
        match active_prompt.task.await {
            Ok(Ok(_response)) => {
                self.projection.lock().await.mark_prompt_finished();
            }
            Ok(Err(error)) => {
                self.projection.lock().await.mark_prompt_error(format!(
                    "ACP prompt turn for session `{}` failed: {error}",
                    active_prompt.session_id
                ));
            }
            Err(error) => {
                self.projection.lock().await.mark_prompt_error(format!(
                    "ACP prompt task for session `{}` failed to join: {error}",
                    active_prompt.session_id
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct FilesystemWriteProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: PathBuf,
    content: String,
}

#[derive(Debug, Serialize)]
struct FilesystemReadProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: PathBuf,
    line: Option<u32>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FilesystemReadProbeResponse {
    content: String,
}

#[derive(Debug, Serialize)]
struct TerminalCommandProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(rename = "outputByteLimit")]
    output_byte_limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalCommandProbeResponse {
    pub output: String,
    pub truncated: bool,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
}

fn normalize_absolute_path(path: impl Into<PathBuf>, subject: &str) -> acp::Result<PathBuf> {
    let path = path.into();
    if !path.is_absolute() {
        return Err(acp::Error::invalid_params().data(serde_json::json!({
            "message": format!("{subject} must be an absolute path")
        })));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(acp::Error::invalid_params().data(serde_json::json!({
                        "message": format!("{subject} escapes its root")
                    })));
                }
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }

    Ok(normalized)
}

fn slice_text_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> acp::Result<String> {
    let start_line = line.unwrap_or(1);
    if start_line == 0 {
        return Err(acp::Error::invalid_params().data(serde_json::json!({
            "message": "filesystem read line must be 1-based"
        })));
    }

    if let Some(limit) = limit
        && limit == 0
    {
        return Err(acp::Error::invalid_params().data(serde_json::json!({
            "message": "filesystem read limit must be greater than zero"
        })));
    }

    let start_index = usize::try_from(start_line - 1).map_err(|_| acp::Error::invalid_params())?;
    let limit = limit
        .map(|value| usize::try_from(value).map_err(|_| acp::Error::invalid_params()))
        .transpose()?;

    let mut selected = String::new();
    for line in content
        .split_inclusive('\n')
        .skip(start_index)
        .take(limit.unwrap_or(usize::MAX))
    {
        selected.push_str(line);
    }

    Ok(selected)
}

fn map_filesystem_error(action: &str, path: &Path, error: std::io::Error) -> acp::Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            acp::Error::resource_not_found(Some(path.display().to_string()))
        }
        _ => acp::Error::internal_error().data(serde_json::json!({
            "message": format!("failed to {action} `{}`: {error}", path.display())
        })),
    }
}

fn default_acp_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe()?;
    let binary_name = format!("fluent-code-acp{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = vec![current_exe.with_file_name(&binary_name)];

    if current_exe
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        == Some("deps")
        && let Some(target_dir) = current_exe.parent().and_then(Path::parent)
    {
        candidates.push(target_dir.join(&binary_name));
    }

    Ok(candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| current_exe.with_file_name(binary_name)))
}

fn protocol_version_label(version: acp::ProtocolVersion) -> String {
    match version {
        acp::ProtocolVersion::V1 => "1".to_string(),
        _ => format!("{version:?}"),
    }
}

fn tool_call_status_label(status: acp::ToolCallStatus) -> String {
    match status {
        acp::ToolCallStatus::Pending => "pending".to_string(),
        acp::ToolCallStatus::InProgress => "in_progress".to_string(),
        acp::ToolCallStatus::Completed => "completed".to_string(),
        acp::ToolCallStatus::Failed => "failed".to_string(),
        _ => format!("{status:?}"),
    }
}

fn content_block_label(content: acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(text) => text.text,
        acp::ContentBlock::ResourceLink(link) => link.uri,
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::Resource(_) => "<resource>".to_string(),
        _ => "<content>".to_string(),
    }
}

#[cfg(test)]
pub(crate) async fn assert_projection_loop_redraws_stream_updates_without_waiting_for_full_input_poll()
-> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let mut controller =
        ProjectionController::new(Arc::clone(&projection), None, PathBuf::default());

    {
        let mut state = projection.lock().await;
        state
            .projection
            .mark_session_created(acp::SessionId::new("session-1"));
        state.projection.mark_prompt_started();
        state
            .projection
            .apply_session_notification(acp::SessionNotification::new(
                "session-1",
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new("Mock assistant ")),
                )),
            ));
        state
            .projection
            .apply_session_notification(acp::SessionNotification::new(
                "session-1",
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new("response")),
                )),
            ));
    }

    let redraw_happened = AtomicBool::new(false);
    let should_quit = run_projection_iteration(
        &mut controller,
        |snapshot| {
            let agent_rows = snapshot
                .transcript_rows
                .iter()
                .filter(|row| row.source == TranscriptSource::Agent)
                .collect::<Vec<_>>();
            assert_eq!(agent_rows.len(), 1);
            assert_eq!(agent_rows[0].content, "Mock assistant response");
            assert!(snapshot.prompt_in_flight);
            redraw_happened.store(true, Ordering::SeqCst);
            Ok(())
        },
        |_snapshot| {
            assert!(
                redraw_happened.load(Ordering::SeqCst),
                "expected redraw to happen before the loop handed control to the input poll"
            );
            Ok(ProjectionAction::Quit)
        },
    )
    .await?;

    assert!(should_quit);
    Ok(())
}

#[cfg(test)]
pub(crate) async fn assert_projection_state_flushes_terminal_stream_updates_immediately()
-> Result<()> {
    for outcome in [
        PromptTerminalOutcome::Done,
        PromptTerminalOutcome::Cancelled,
        PromptTerminalOutcome::Failed,
    ] {
        assert_projection_state_flushes_terminal_stream_updates_immediately_for_outcome(outcome)
            .await?;
    }

    Ok(())
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum PromptTerminalOutcome {
    Done,
    Cancelled,
    Failed,
}

#[cfg(test)]
async fn assert_projection_state_flushes_terminal_stream_updates_immediately_for_outcome(
    outcome: PromptTerminalOutcome,
) -> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let mut controller =
        ProjectionController::new(Arc::clone(&projection), None, PathBuf::default());

    {
        let mut state = projection.lock().await;
        state
            .projection
            .mark_session_created(acp::SessionId::new("session-1"));
        state.projection.mark_prompt_started();
        state
            .projection
            .apply_session_notification(acp::SessionNotification::new(
                "session-1",
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new("final streamed chunk")),
                )),
            ));
    }

    controller.active_prompt = Some(ActivePromptRequest {
        session_id: acp::SessionId::new("session-1"),
        task: tokio::spawn(async move {
            match outcome {
                PromptTerminalOutcome::Done => {
                    Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
                }
                PromptTerminalOutcome::Cancelled => {
                    Ok(acp::PromptResponse::new(acp::StopReason::Cancelled))
                }
                PromptTerminalOutcome::Failed => {
                    Err(acp::Error::internal_error().data(serde_json::json!({
                        "message": "mock ACP prompt failure"
                    })))
                }
            }
        }),
    });
    tokio::task::yield_now().await;

    let redraw_happened = AtomicBool::new(false);
    let should_quit = run_projection_iteration(
        &mut controller,
        |snapshot| {
            assert!(
                !snapshot.prompt_in_flight,
                "expected {outcome:?} prompt completion to flush the terminal projection before waiting for more input"
            );
            assert_eq!(
                snapshot.transcript_rows,
                vec![TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "final streamed chunk".to_string(),
                }]
            );
            match outcome {
                PromptTerminalOutcome::Done | PromptTerminalOutcome::Cancelled => {
                    assert!(
                        snapshot.prompt_error.is_none(),
                        "expected {outcome:?} prompt completion to leave no prompt error"
                    );
                }
                PromptTerminalOutcome::Failed => {
                    assert!(
                        snapshot
                            .prompt_error
                            .as_deref()
                            .is_some_and(|error| error.contains("mock ACP prompt failure")),
                        "expected failed prompt completion to surface an immediate prompt error, got: {snapshot:?}"
                    );
                }
            }
            redraw_happened.store(true, Ordering::SeqCst);
            Ok(())
        },
        |_snapshot| {
            assert!(
                redraw_happened.load(Ordering::SeqCst),
                "expected the final {outcome:?} terminal-state redraw to happen before the next input wait"
            );
            Ok(ProjectionAction::Quit)
        },
    )
    .await?;

    assert!(should_quit);
    assert!(
        controller.active_prompt.is_none(),
        "expected polling the {outcome:?} prompt terminal outcome to clear the active prompt handle"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_session_notification_coalesces_adjacent_agent_message_chunks() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk("Mock assistant "));
        projection.apply_session_notification(agent_message_chunk("response: "));
        projection.apply_session_notification(agent_message_chunk("hello"));

        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "Mock assistant response: hello".to_string(),
            }]
        );
    }

    #[test]
    fn apply_session_notification_coalesces_adjacent_agent_thought_chunks() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_thought_chunk("thinking"));
        projection.apply_session_notification(agent_thought_chunk(" through"));
        projection.apply_session_notification(agent_thought_chunk(" steps"));

        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Thought,
                content: "thinking through steps".to_string(),
            }]
        );
    }

    #[test]
    fn apply_session_notification_ignores_empty_chunks() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk(""));
        projection.apply_session_notification(agent_thought_chunk(""));
        assert!(projection.transcript_rows.is_empty());

        projection.apply_session_notification(agent_message_chunk("hello"));
        projection.apply_session_notification(agent_message_chunk(""));
        projection.apply_session_notification(agent_thought_chunk(""));

        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn apply_session_notification_keeps_thought_and_message_rows_separate() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_thought_chunk("thinking"));
        projection.apply_session_notification(agent_message_chunk("answer"));
        projection.apply_session_notification(agent_thought_chunk(" more"));
        projection.apply_session_notification(agent_message_chunk(" done"));

        assert_eq!(
            projection.transcript_rows,
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::Thought,
                    content: "thinking".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "answer".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Thought,
                    content: " more".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: " done".to_string(),
                },
            ]
        );
    }

    #[test]
    fn apply_session_notification_does_not_merge_across_user_chunks() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk("first answer"));
        projection.apply_session_notification(user_message_chunk("follow-up"));
        projection.apply_session_notification(agent_message_chunk(" second answer"));

        assert_eq!(
            projection.transcript_rows,
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "first answer".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::User,
                    content: "follow-up".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: " second answer".to_string(),
                },
            ]
        );
    }

    #[test]
    fn apply_session_notification_does_not_merge_across_tool_updates() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk("before tool"));
        projection.apply_session_notification(tool_call_update("tool-1"));
        projection.apply_session_notification(agent_message_chunk(" after tool"));

        assert_eq!(
            projection.transcript_rows,
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "before tool".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: " after tool".to_string(),
                },
            ]
        );
    }

    #[test]
    fn apply_session_notification_does_not_merge_across_tool_calls() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk("before tool"));
        projection.apply_session_notification(tool_call("tool-1", "Run tool"));
        projection.apply_session_notification(agent_message_chunk(" after tool"));

        assert_eq!(
            projection.transcript_rows,
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "before tool".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: " after tool".to_string(),
                },
            ]
        );
    }

    #[test]
    fn apply_session_notification_resets_projection_on_session_reset() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(agent_message_chunk("stale output"));
        projection.apply_session_notification(agent_thought_chunk("stale reasoning"));

        let next_session = acp::SessionId::new("session-2");
        projection.prepare_session_load(&next_session);
        projection.apply_session_notification(agent_message_chunk_for_session(
            next_session.to_string(),
            "fresh output",
        ));

        assert_eq!(projection.session.session_id.as_deref(), Some("session-2"));
        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "fresh output".to_string(),
            }]
        );
    }

    #[test]
    fn apply_session_notification_ignores_stale_notifications_from_previous_session() {
        let mut projection = TuiProjectionState::default();

        projection.mark_session_created(acp::SessionId::new("session-1"));
        projection.apply_session_notification(agent_message_chunk_for_session(
            "session-1",
            "current output",
        ));

        let next_session = acp::SessionId::new("session-2");
        projection.prepare_session_load(&next_session);
        projection.apply_session_notification(agent_message_chunk_for_session(
            "session-1",
            "stale output",
        ));

        assert_eq!(projection.session.session_id.as_deref(), Some("session-2"));
        assert!(projection.transcript_rows.is_empty());

        projection.apply_session_notification(agent_message_chunk_for_session(
            "session-2",
            "fresh output",
        ));

        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "fresh output".to_string(),
            }]
        );
    }

    #[test]
    fn apply_permission_request_ignores_stale_requests_from_previous_session() {
        let mut projection = TuiProjectionState::default();

        projection.mark_session_created(acp::SessionId::new("session-1"));
        projection.apply_permission_request(&permission_request("session-1", "tool-1"));
        assert_eq!(
            projection
                .pending_permission
                .as_ref()
                .map(|pending| pending.tool_call_id.as_str()),
            Some("tool-1")
        );

        let next_session = acp::SessionId::new("session-2");
        projection.prepare_session_load(&next_session);
        projection.apply_permission_request(&permission_request("session-1", "tool-stale"));

        assert_eq!(projection.session.session_id.as_deref(), Some("session-2"));
        assert!(projection.pending_permission.is_none());
    }

    #[test]
    fn apply_session_notification_does_not_merge_across_prompt_turn_boundaries() {
        let mut projection = TuiProjectionState::default();

        projection.mark_prompt_started();
        projection.apply_session_notification(agent_message_chunk("first turn"));
        projection.mark_prompt_finished();

        projection.mark_prompt_started();
        projection.apply_session_notification(agent_message_chunk("second turn"));

        assert_eq!(
            projection.transcript_rows,
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "first turn".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "second turn".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn projection_loop_redraws_stream_updates_without_waiting_for_full_input_poll() {
        assert_projection_loop_redraws_stream_updates_without_waiting_for_full_input_poll()
            .await
            .expect("projection iteration to accept the redraw-order regression assertion");
    }

    #[tokio::test]
    async fn projection_state_flushes_terminal_stream_updates_immediately() {
        assert_projection_state_flushes_terminal_stream_updates_immediately()
            .await
            .expect("projection iteration to flush the terminal-state snapshot");
    }

    #[test]
    fn apply_session_notification_preserves_image_placeholder_labels() {
        let mut projection = TuiProjectionState::default();

        projection.apply_session_notification(session_notification(
            "session-1",
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                acp::ContentBlock::Image(acp::ImageContent::new("ZmFrZQ==", "image/png")),
            )),
        ));

        assert_eq!(
            projection.transcript_rows,
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "<image>".to_string(),
            }]
        );
    }

    fn agent_message_chunk(text: &str) -> acp::SessionNotification {
        agent_message_chunk_for_session("session-1", text)
    }

    fn agent_message_chunk_for_session(
        session_id: impl Into<acp::SessionId>,
        text: &str,
    ) -> acp::SessionNotification {
        session_notification(
            session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        )
    }

    fn agent_thought_chunk(text: &str) -> acp::SessionNotification {
        session_notification(
            "session-1",
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        )
    }

    fn user_message_chunk(text: &str) -> acp::SessionNotification {
        session_notification(
            "session-1",
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        )
    }

    fn tool_call_update(tool_call_id: &str) -> acp::SessionNotification {
        session_notification(
            "session-1",
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id.to_string(),
                acp::ToolCallUpdateFields::new(),
            )),
        )
    }

    fn tool_call(tool_call_id: &str, title: &str) -> acp::SessionNotification {
        session_notification(
            "session-1",
            acp::SessionUpdate::ToolCall(acp::ToolCall::new(
                tool_call_id.to_string(),
                title.to_string(),
            )),
        )
    }

    fn permission_request(
        session_id: impl Into<acp::SessionId>,
        tool_call_id: &str,
    ) -> acp::RequestPermissionRequest {
        acp::RequestPermissionRequest::new(
            session_id,
            acp::ToolCallUpdate::new(tool_call_id.to_string(), acp::ToolCallUpdateFields::new()),
            vec![acp::PermissionOption::new(
                "allow_once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            )],
        )
    }

    fn session_notification(
        session_id: impl Into<acp::SessionId>,
        update: acp::SessionUpdate,
    ) -> acp::SessionNotification {
        acp::SessionNotification::new(session_id, update)
    }
}
