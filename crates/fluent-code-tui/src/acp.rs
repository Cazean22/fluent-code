use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc as StdArc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use async_trait::async_trait;
use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use fluent_code_app::app::AppStatus;
use fluent_code_app::config::Config;
use fluent_code_app::error::{FluentCodeError, Result};
#[cfg(test)]
use fluent_code_app::session::model::ToolSource;
use fluent_code_app::session::model::{
    ForegroundPhase, Role, Session, TaskDelegationRecord, ToolApprovalState, ToolExecutionState,
    ToolInvocationRecord, TranscriptFidelity, TranscriptItemContent, TranscriptItemRecord,
    TranscriptStreamState, Turn,
};
use futures::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{info, warn};
use uuid::Uuid;

use crate::conversation::{
    ConversationRow, DerivedHistoryCells, ToolRow, derive_history_cells_for_session,
};
use crate::terminal;
use crate::theme::TUI_THEME;
use crate::view::{conversation_lines_from_rows, resolve_transcript_scroll};

const ACP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const ACP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const ACP_TEST_PROBES_ENV_VAR: &str = "FLUENT_CODE_ACP_ENABLE_TEST_PROBES";
const PROJECTION_ACTIVITY_BURST_DRAIN_BUDGET: Duration = Duration::from_millis(5);
const PROJECTION_PAGE_SCROLL_LINES: u16 = 8;
const ACP_META_LATEST_PROMPT_STATE_KEY: &str = "fluentCodeLatestPromptState";
const ACP_META_REPLAY_FIDELITY_KEY: &str = "fluentCodeReplayFidelity";
const ACP_META_TOOL_INVOCATION_KEY: &str = "fluentCodeToolInvocation";
const ACP_META_TRANSCRIPT_ITEM_KEY: &str = "fluentCodeTranscriptItem";
const SESSION_BROWSER_WIDTH: u16 = 36;

#[derive(Debug, Clone)]
pub struct TuiProjectionState {
    pub sessions: Vec<SessionBrowserEntryProjection>,
    pub session: SessionProjection,
    pub pending_permission: Option<PendingPermissionProjection>,
    pub subprocess: SubprocessProjection,
    pub draft_input: String,
    pub prompt_in_flight: bool,
    pub prompt_status: Option<PromptStatusProjection>,
    pub replay_fidelity: ReplayFidelityProjection,
    pub prompt_error: Option<String>,
    pub startup_error: Option<String>,
    pub transcript_scroll_top: u16,
    pub transcript_follow_tail: bool,
    transcript_session: Arc<Session>,
    transcript_run_id: Uuid,
    active_run_id: Option<Uuid>,
    current_user_turn_id: Option<Uuid>,
    current_assistant_turn_id: Option<Uuid>,
    current_reasoning_turn_id: Option<Uuid>,
    conversation_cache: Arc<ProjectionConversationCache>,
    cache_dirty: bool,
}

#[derive(Debug, Default)]
struct ProjectionConversationCache {
    #[cfg(test)]
    history_cells: DerivedHistoryCells,
    conversation_entries: Vec<ConversationEntryProjection>,
    transcript_rows: Vec<TranscriptRowProjection>,
    tool_statuses: Vec<ToolStatusProjection>,
    transcript_lines: Vec<Line<'static>>,
}

impl ProjectionConversationCache {
    fn build(session: &Session, status: &AppStatus, active_run_id: Option<Uuid>) -> Self {
        let history_cells = derive_history_cells_for_session(session, status, active_run_id);
        let conversation_entries = conversation_entries_from_history_cells(&history_cells);
        Self {
            #[cfg(test)]
            history_cells: history_cells.clone(),
            transcript_rows: transcript_rows_from_entries(&conversation_entries),
            tool_statuses: tool_statuses_from_entries(&conversation_entries),
            transcript_lines: transcript_lines_from_entries(&conversation_entries),
            conversation_entries,
        }
    }
}

impl Default for TuiProjectionState {
    fn default() -> Self {
        let transcript_run_id = Uuid::new_v4();
        let mut projection = Self {
            sessions: Vec::new(),
            session: SessionProjection::default(),
            pending_permission: None,
            subprocess: SubprocessProjection::default(),
            draft_input: String::new(),
            prompt_in_flight: false,
            prompt_status: None,
            replay_fidelity: ReplayFidelityProjection::Exact,
            prompt_error: None,
            startup_error: None,
            transcript_scroll_top: 0,
            transcript_follow_tail: true,
            transcript_session: Arc::new(Session::new("ACP session")),
            transcript_run_id,
            active_run_id: None,
            current_user_turn_id: None,
            current_assistant_turn_id: None,
            current_reasoning_turn_id: None,
            conversation_cache: Arc::new(ProjectionConversationCache::default()),
            cache_dirty: true,
        };
        projection.refresh_conversation_cache();
        projection
    }
}

impl PartialEq for TuiProjectionState {
    fn eq(&self, other: &Self) -> bool {
        self.sessions == other.sessions
            && self.session == other.session
            && self.conversation_entries_ref() == other.conversation_entries_ref()
            && self.pending_permission == other.pending_permission
            && self.subprocess == other.subprocess
            && self.draft_input == other.draft_input
            && self.prompt_in_flight == other.prompt_in_flight
            && self.prompt_status == other.prompt_status
            && self.replay_fidelity == other.replay_fidelity
            && self.prompt_error == other.prompt_error
            && self.startup_error == other.startup_error
            && self.transcript_scroll_top == other.transcript_scroll_top
            && self.transcript_follow_tail == other.transcript_follow_tail
    }
}

impl Eq for TuiProjectionState {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBrowserEntryProjection {
    pub session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionProjection {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplayFidelityProjection {
    #[default]
    Exact,
    Approximate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStatusProjection {
    Running,
    AwaitingToolApproval,
    RunningTool,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
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

#[derive(Debug, Clone)]
pub struct ConversationEntryProjection {
    row: ConversationRow,
    pub kind: ConversationEntryKind,
}

impl ConversationEntryProjection {
    fn from_row(row: &ConversationRow) -> Self {
        let kind = match row {
            ConversationRow::Turn(turn) => {
                ConversationEntryKind::Turn(ConversationTurnEntryProjection {
                    role: turn.role,
                    content: turn.content.clone(),
                    is_streaming: turn.is_streaming,
                })
            }
            ConversationRow::Reasoning(reasoning) => {
                ConversationEntryKind::Reasoning(ConversationReasoningEntryProjection {
                    content: reasoning.content.clone(),
                    is_streaming: reasoning.is_streaming,
                })
            }
            ConversationRow::Tool(tool) => {
                ConversationEntryKind::Tool(tool_status_projection(tool.as_ref()))
            }
            ConversationRow::ToolGroup(group) => {
                ConversationEntryKind::ToolGroup(ToolGroupEntryProjection {
                    items: group.items.iter().map(tool_status_projection).collect(),
                })
            }
            ConversationRow::RunMarker(marker) => {
                ConversationEntryKind::RunMarker(RunMarkerProjection {
                    label: marker.label.clone(),
                })
            }
        };

        Self {
            row: row.clone(),
            kind,
        }
    }

    fn row(&self) -> &ConversationRow {
        &self.row
    }
}

impl PartialEq for ConversationEntryProjection {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Eq for ConversationEntryProjection {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationEntryKind {
    Turn(ConversationTurnEntryProjection),
    Reasoning(ConversationReasoningEntryProjection),
    Tool(ToolStatusProjection),
    ToolGroup(ToolGroupEntryProjection),
    RunMarker(RunMarkerProjection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationTurnEntryProjection {
    pub role: Role,
    pub content: String,
    pub is_streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationReasoningEntryProjection {
    pub content: String,
    pub is_streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolGroupEntryProjection {
    pub items: Vec<ToolStatusProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMarkerProjection {
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermissionProjection {
    pub tool_call_id: String,
    pub title: String,
    pub options: Vec<PermissionOptionProjection>,
}

#[derive(Debug, Clone, Default)]
struct ToolCallPayload {
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    content: Option<Vec<acp::ToolCallContent>>,
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

#[derive(Debug, Clone, Default)]
struct LoadedSessionMetadataProjection {
    replay_fidelity: Option<ReplayFidelityProjection>,
    prompt_status: Option<Option<PromptStatusProjection>>,
}

impl LoadedSessionMetadataProjection {
    fn is_complete(&self) -> bool {
        self.replay_fidelity.is_some() && self.prompt_status.is_some()
    }

    fn apply_fallback_session(&mut self, session: &Session) {
        self.replay_fidelity
            .get_or_insert(match session.transcript_fidelity {
                TranscriptFidelity::Approximate => ReplayFidelityProjection::Approximate,
                TranscriptFidelity::Exact => ReplayFidelityProjection::Exact,
            });
        self.prompt_status.get_or_insert_with(|| {
            Some(
                match session.foreground_owner.as_ref().map(|owner| owner.phase) {
                    Some(ForegroundPhase::Generating) => PromptStatusProjection::Running,
                    Some(ForegroundPhase::AwaitingToolApproval) => {
                        PromptStatusProjection::AwaitingToolApproval
                    }
                    Some(ForegroundPhase::RunningTool) => PromptStatusProjection::RunningTool,
                    None => return None,
                },
            )
        });
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionActivitySnapshot {
    pub projection: TuiProjectionState,
    pub activity_sequence: u64,
}

impl TuiProjectionState {
    fn refresh_conversation_cache(&mut self) {
        self.cache_dirty = true;
    }

    fn ensure_conversation_cache_fresh(&mut self) {
        if self.cache_dirty {
            self.conversation_cache = Arc::new(ProjectionConversationCache::build(
                &self.transcript_session,
                &self.app_status(),
                self.active_run_id,
            ));
            self.cache_dirty = false;
        }
    }

    fn session_mut(&mut self) -> &mut Session {
        Arc::make_mut(&mut self.transcript_session)
    }

    fn apply_session_list(&mut self, sessions: Vec<SessionBrowserEntryProjection>) {
        self.sessions = sessions;
    }

    fn update_session_browser_entry(
        &mut self,
        session_id: &str,
        title: Option<&str>,
        updated_at: Option<&str>,
    ) {
        let Some(entry) = self
            .sessions
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
        else {
            return;
        };

        if let Some(title) = title {
            entry.title = Some(title.to_string());
        }
        if let Some(updated_at) = updated_at {
            entry.updated_at = Some(updated_at.to_string());
        }
    }

    fn adjacent_session_id(&self, direction: SessionBrowserDirection) -> Option<String> {
        let current_session_id = self.session.session_id.as_deref()?;
        let current_index = self
            .sessions
            .iter()
            .position(|session| session.session_id == current_session_id)?;

        match direction {
            SessionBrowserDirection::Previous => current_index
                .checked_sub(1)
                .and_then(|index| self.sessions.get(index))
                .map(|session| session.session_id.clone()),
            SessionBrowserDirection::Next => self
                .sessions
                .get(current_index.saturating_add(1))
                .map(|session| session.session_id.clone()),
        }
    }

    fn reset_session_projection(&mut self, session_id: Option<String>) {
        self.session = SessionProjection {
            session_id,
            ..SessionProjection::default()
        };
        self.pending_permission = None;
        self.draft_input.clear();
        self.set_prompt_status(None);
        self.replay_fidelity = ReplayFidelityProjection::Exact;
        self.prompt_error = None;
        self.transcript_scroll_top = 0;
        self.transcript_follow_tail = true;
        self.transcript_session = Arc::new(Session::new(
            self.session
                .session_id
                .clone()
                .unwrap_or_else(|| "ACP session".to_string()),
        ));
        self.session_mut().transcript_fidelity = TranscriptFidelity::Exact;
        self.transcript_run_id = Uuid::new_v4();
        self.active_run_id = None;
        self.break_transcript_merge();
        self.refresh_conversation_cache();
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
        self.set_prompt_status(None);
        self.break_transcript_merge();
        self.subprocess.status = SubprocessStatus::Failed { message };
        self.refresh_conversation_cache();
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
        self.draft_input.clear();
        self.transcript_run_id = Uuid::new_v4();
        self.active_run_id = Some(self.transcript_run_id);
        self.set_prompt_status(Some(PromptStatusProjection::Running));
        self.break_transcript_merge();
        self.refresh_conversation_cache();
    }

    fn mark_prompt_finished(&mut self, stop_reason: acp::StopReason) {
        self.active_run_id = None;
        self.set_prompt_status(Some(match stop_reason {
            acp::StopReason::Cancelled => PromptStatusProjection::Cancelled,
            _ => PromptStatusProjection::Completed,
        }));
        self.commit_open_transcript_items();
        self.break_transcript_merge();
        self.refresh_conversation_cache();
    }

    fn mark_prompt_error(&mut self, message: String) {
        self.active_run_id = None;
        self.set_prompt_status(Some(PromptStatusProjection::Failed));
        self.prompt_error = Some(message);
        self.commit_open_transcript_items();
        self.break_transcript_merge();
        self.refresh_conversation_cache();
    }

    fn apply_loaded_session_projection(&mut self, metadata: LoadedSessionMetadataProjection) {
        if let Some(replay_fidelity) = metadata.replay_fidelity {
            self.replay_fidelity = replay_fidelity;
            self.session_mut().transcript_fidelity = match replay_fidelity {
                ReplayFidelityProjection::Approximate => TranscriptFidelity::Approximate,
                ReplayFidelityProjection::Exact => TranscriptFidelity::Exact,
            };
        }
        if let Some(prompt_status) = metadata.prompt_status {
            self.set_prompt_status(prompt_status);
        }
        self.active_run_id = self.prompt_in_flight.then_some(self.transcript_run_id);
        self.refresh_conversation_cache();
    }

    #[cfg(test)]
    fn apply_loaded_session_metadata(&mut self, session: &Session) {
        let mut metadata = LoadedSessionMetadataProjection::default();
        metadata.apply_fallback_session(session);
        self.apply_loaded_session_projection(metadata);
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

        self.session.session_id = Some(session_id.clone());

        match notification.update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                self.append_user_chunk(chunk.content);
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.append_assistant_message_chunk(chunk.content);
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                self.append_assistant_reasoning_chunk(chunk.content);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                self.break_transcript_merge();
                self.apply_tool_call_snapshot(tool_call);
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                self.break_transcript_merge();
                self.apply_tool_call_update(update);
            }
            acp::SessionUpdate::SessionInfoUpdate(update) => {
                if let Some(title) = update.title.take() {
                    self.session_mut().title = title.clone();
                    self.session.title = Some(title);
                    let current_title = self.session.title.clone();
                    self.update_session_browser_entry(&session_id, current_title.as_deref(), None);
                }
                if let Some(updated_at) = update.updated_at.take() {
                    self.session.updated_at = Some(updated_at);
                    let current_updated_at = self.session.updated_at.clone();
                    self.update_session_browser_entry(
                        &session_id,
                        None,
                        current_updated_at.as_deref(),
                    );
                }
                if let Some(item) = transcript_item_from_meta(update.meta.as_ref()) {
                    self.apply_transcript_metadata_item(item);
                    self.refresh_conversation_cache();
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
        if let Some(index) =
            self.find_tool_invocation_index(&request.tool_call.tool_call_id.to_string())
        {
            let session = self.session_mut();
            session.tool_invocations[index].approval_state = ToolApprovalState::Pending;
            let transcript_item =
                TranscriptItemRecord::from_tool_invocation(&session.tool_invocations[index]);
            session.upsert_transcript_item(transcript_item);
        }
        self.set_prompt_status(Some(PromptStatusProjection::AwaitingToolApproval));
        self.active_run_id = Some(self.transcript_run_id);
        self.refresh_conversation_cache();
    }

    fn apply_permission_selection(&mut self, option_id: &str) {
        let Some(tool_call_id) = self
            .pending_permission
            .as_ref()
            .map(|permission| permission.tool_call_id.clone())
        else {
            return;
        };

        if let Some(index) = self.find_tool_invocation_index(&tool_call_id) {
            let session = self.session_mut();
            session.tool_invocations[index].approval_state = if option_id.starts_with("allow") {
                ToolApprovalState::Approved
            } else {
                ToolApprovalState::Denied
            };
            let transcript_item =
                TranscriptItemRecord::from_tool_invocation(&session.tool_invocations[index]);
            session.upsert_transcript_item(transcript_item);
        }

        self.pending_permission = None;
        self.set_prompt_status(Some(if option_id.starts_with("allow") {
            PromptStatusProjection::RunningTool
        } else {
            PromptStatusProjection::Running
        }));
        self.refresh_conversation_cache();
    }

    fn append_user_chunk(&mut self, content: acp::ContentBlock) {
        self.current_assistant_turn_id = None;
        self.current_reasoning_turn_id = None;
        let content = content_block_label(content);
        if content.is_empty() {
            return;
        }

        if let Some(turn_id) = self.current_user_turn_id
            && let Some(turn) = self
                .session_mut()
                .turns
                .iter_mut()
                .find(|turn| turn.id == turn_id)
        {
            turn.content.push_str(&content);
            let transcript_item = TranscriptItemRecord::from_turn(turn);
            self.session_mut().upsert_transcript_item(transcript_item);
            self.refresh_conversation_cache();
            return;
        }

        let turn = Turn {
            id: Uuid::new_v4(),
            run_id: self.transcript_run_id,
            role: Role::User,
            content,
            reasoning: String::new(),
            sequence_number: self.session_mut().allocate_replay_sequence(),
            timestamp: Utc::now(),
        };
        self.current_user_turn_id = Some(turn.id);
        self.session_mut().turns.push(turn.clone());
        self.session_mut()
            .upsert_transcript_item(TranscriptItemRecord::from_turn(&turn));
        self.refresh_conversation_cache();
    }

    fn break_transcript_merge(&mut self) {
        self.current_user_turn_id = None;
        self.current_assistant_turn_id = None;
        self.current_reasoning_turn_id = None;
    }

    fn append_assistant_message_chunk(&mut self, content: acp::ContentBlock) {
        if self.prompt_in_flight {
            self.set_prompt_status(Some(PromptStatusProjection::Running));
            self.active_run_id = Some(self.transcript_run_id);
        }
        self.append_assistant_chunk(content, TranscriptSource::Agent);
    }

    fn append_assistant_reasoning_chunk(&mut self, content: acp::ContentBlock) {
        if self.prompt_in_flight {
            self.set_prompt_status(Some(PromptStatusProjection::Running));
            self.active_run_id = Some(self.transcript_run_id);
        }
        self.append_assistant_chunk(content, TranscriptSource::Thought);
    }

    fn append_assistant_chunk(&mut self, content: acp::ContentBlock, source: TranscriptSource) {
        match source {
            TranscriptSource::Agent => {
                self.current_reasoning_turn_id = None;
                self.current_user_turn_id = None;
            }
            TranscriptSource::Thought => {
                self.current_assistant_turn_id = None;
                self.current_user_turn_id = None;
            }
            TranscriptSource::User => return,
        }

        let content = content_block_label(content);
        if content.is_empty() {
            return;
        }

        let existing_turn_id = match source {
            TranscriptSource::User => return,
            TranscriptSource::Agent => self.current_assistant_turn_id,
            TranscriptSource::Thought => self.current_reasoning_turn_id,
        };

        if let Some(existing_turn_id) = existing_turn_id {
            self.update_assistant_transcript_item(existing_turn_id, source, &content);
            self.refresh_conversation_cache();
            return;
        }

        let new_turn_id = Uuid::new_v4();
        let sequence_number = self.session_mut().allocate_replay_sequence();
        let transcript_item = match source {
            TranscriptSource::Agent => TranscriptItemRecord::assistant_text(
                self.transcript_run_id,
                new_turn_id,
                sequence_number,
                content,
                TranscriptStreamState::Open,
            ),
            TranscriptSource::Thought => TranscriptItemRecord::assistant_reasoning(
                self.transcript_run_id,
                new_turn_id,
                sequence_number,
                content,
                TranscriptStreamState::Open,
            ),
            TranscriptSource::User => return,
        };
        match source {
            TranscriptSource::Agent => self.current_assistant_turn_id = Some(new_turn_id),
            TranscriptSource::Thought => self.current_reasoning_turn_id = Some(new_turn_id),
            TranscriptSource::User => return,
        }
        self.session_mut().upsert_transcript_item(transcript_item);
        self.refresh_conversation_cache();
    }

    fn apply_tool_call_update(&mut self, update: acp::ToolCallUpdate) {
        let tool_call_id = update.tool_call_id.to_string();
        let title = update
            .fields
            .title
            .clone()
            .unwrap_or_else(|| format!("tool {tool_call_id}"));
        let tool_name = normalized_tool_name(&title, update.fields.kind);
        let raw_input = update.fields.raw_input.clone();
        let raw_output = update.fields.raw_output.clone();
        let content = update.fields.content.clone();
        let status = update.fields.status;
        let tool_invocation = tool_invocation_from_meta(update.meta.as_ref());

        self.upsert_tool_invocation(
            &tool_call_id,
            &tool_name,
            ToolCallPayload {
                raw_input,
                raw_output,
                content,
            },
            status,
            tool_invocation,
        );

        if matches!(status, Some(acp::ToolCallStatus::InProgress)) {
            self.set_prompt_status(Some(PromptStatusProjection::RunningTool));
            self.active_run_id = Some(self.transcript_run_id);
        }
        self.refresh_conversation_cache();
    }

    fn apply_tool_call_snapshot(&mut self, tool_call: acp::ToolCall) {
        let tool_call_id = tool_call.tool_call_id.to_string();
        let tool_name = normalized_tool_name(&tool_call.title, Some(tool_call.kind));
        self.upsert_tool_invocation(
            &tool_call_id,
            &tool_name,
            ToolCallPayload {
                raw_input: tool_call.raw_input,
                raw_output: tool_call.raw_output,
                content: Some(tool_call.content),
            },
            Some(tool_call.status),
            tool_invocation_from_meta(tool_call.meta.as_ref()),
        );
        self.refresh_conversation_cache();
    }

    fn upsert_tool_invocation(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        payload: ToolCallPayload,
        status: Option<acp::ToolCallStatus>,
        tool_invocation: Option<ToolInvocationRecord>,
    ) {
        let meta_result = tool_invocation
            .as_ref()
            .and_then(|invocation| invocation.result.clone());
        let meta_error = tool_invocation
            .as_ref()
            .and_then(|invocation| invocation.error.clone());
        let (result, error) = tool_output_previews(payload.raw_output, payload.content.as_deref());
        let now = Utc::now();
        let invocation_index = self
            .transcript_session
            .tool_invocations
            .iter()
            .position(|invocation| invocation.tool_call_id == tool_call_id);
        let approval_state = invocation_index
            .and_then(|index| {
                self.transcript_session
                    .tool_invocations
                    .get(index)
                    .map(|invocation| invocation.approval_state)
            })
            .unwrap_or(ToolApprovalState::Approved);
        let execution_state = tool_execution_state(status);

        let invocation = ToolInvocationRecord {
            id: tool_invocation
                .as_ref()
                .map(|invocation| invocation.id)
                .or_else(|| {
                    invocation_index.and_then(|index| {
                        self.transcript_session
                            .tool_invocations
                            .get(index)
                            .map(|invocation| invocation.id)
                    })
                })
                .unwrap_or_else(Uuid::new_v4),
            run_id: tool_invocation
                .as_ref()
                .map(|invocation| invocation.run_id)
                .unwrap_or(self.transcript_run_id),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_source: tool_invocation
                .as_ref()
                .map(|invocation| invocation.tool_source.clone())
                .unwrap_or(fluent_code_app::session::model::ToolSource::BuiltIn),
            arguments: payload.raw_input.unwrap_or_else(|| {
                tool_invocation
                    .as_ref()
                    .map(|invocation| invocation.arguments.clone())
                    .unwrap_or_else(|| json!({}))
            }),
            preceding_turn_id: tool_invocation
                .as_ref()
                .and_then(|invocation| invocation.preceding_turn_id)
                .or_else(|| self.latest_transcript_turn_id()),
            approval_state,
            execution_state,
            result: result.or(meta_result),
            error: error.or(meta_error),
            delegation: tool_invocation
                .as_ref()
                .and_then(|invocation| invocation.delegation.clone())
                .or_else(|| {
                    invocation_index.and_then(|index| {
                        self.transcript_session
                            .tool_invocations
                            .get(index)
                            .and_then(|invocation| invocation.delegation.clone())
                    })
                }),
            sequence_number: tool_invocation
                .as_ref()
                .map(|invocation| invocation.sequence_number)
                .or_else(|| {
                    invocation_index.and_then(|index| {
                        self.transcript_session
                            .tool_invocations
                            .get(index)
                            .map(|invocation| invocation.sequence_number)
                    })
                })
                .unwrap_or_else(|| self.session_mut().allocate_replay_sequence()),
            requested_at: tool_invocation
                .as_ref()
                .map(|invocation| invocation.requested_at)
                .or_else(|| {
                    invocation_index.and_then(|index| {
                        self.transcript_session
                            .tool_invocations
                            .get(index)
                            .map(|invocation| invocation.requested_at)
                    })
                })
                .unwrap_or(now),
            approved_at: tool_invocation
                .as_ref()
                .and_then(|invocation| invocation.approved_at),
            completed_at: matches!(
                execution_state,
                ToolExecutionState::Completed | ToolExecutionState::Failed
            )
            .then_some(now)
            .or_else(|| {
                tool_invocation
                    .as_ref()
                    .and_then(|invocation| invocation.completed_at)
            }),
        };

        match invocation_index {
            Some(index) => self.session_mut().tool_invocations[index] = invocation.clone(),
            None => self.session_mut().tool_invocations.push(invocation.clone()),
        }

        self.session_mut()
            .upsert_transcript_item(TranscriptItemRecord::from_tool_invocation(&invocation));
    }

    fn apply_transcript_metadata_item(&mut self, item: TranscriptItemRecord) {
        let mut parent_invocation_transcript_item = None;
        if let TranscriptItemContent::DelegatedChild(content) = &item.content
            && let Some(tool_invocation_id) = item.parent_tool_invocation_id
            && let Some(invocation) = self
                .session_mut()
                .tool_invocations
                .iter_mut()
                .find(|invocation| invocation.id == tool_invocation_id)
        {
            invocation.delegation = Some(TaskDelegationRecord {
                child_run_id: content.child_run_id,
                agent_name: content.agent_name.clone(),
                prompt: content.prompt.clone(),
                status: content.status,
            });
            parent_invocation_transcript_item =
                Some(TranscriptItemRecord::from_tool_invocation(invocation));
        }

        if let Some(parent_invocation_transcript_item) = parent_invocation_transcript_item {
            self.session_mut()
                .upsert_transcript_item(parent_invocation_transcript_item);
        }

        self.session_mut().next_replay_sequence = self
            .transcript_session
            .next_replay_sequence
            .max(item.sequence_number.saturating_add(1));
        self.session_mut().upsert_transcript_item(item);
    }

    fn update_assistant_transcript_item(
        &mut self,
        turn_id: Uuid,
        source: TranscriptSource,
        content_chunk: &str,
    ) {
        let item_id = match source {
            TranscriptSource::Agent => {
                fluent_code_app::session::model::transcript_assistant_text_item_id(turn_id)
            }
            TranscriptSource::Thought => {
                fluent_code_app::session::model::transcript_assistant_reasoning_item_id(turn_id)
            }
            TranscriptSource::User => return,
        };
        let Some(existing) = self.session_mut().find_transcript_item_mut(item_id) else {
            return;
        };

        if let TranscriptItemContent::Turn(turn) = &mut existing.content {
            match source {
                TranscriptSource::Agent => turn.content.push_str(content_chunk),
                TranscriptSource::Thought => turn.reasoning.push_str(content_chunk),
                TranscriptSource::User => {}
            }
        }
    }

    fn latest_transcript_turn_id(&self) -> Option<Uuid> {
        self.transcript_session
            .transcript_items
            .iter()
            .rev()
            .find_map(|item| item.turn_id)
    }

    fn find_tool_invocation_index(&self, tool_call_id: &str) -> Option<usize> {
        self.transcript_session
            .tool_invocations
            .iter()
            .position(|invocation| invocation.tool_call_id == tool_call_id)
    }

    fn commit_open_transcript_items(&mut self) {
        for item in &mut self.session_mut().transcript_items {
            if item.stream_state == TranscriptStreamState::Open {
                item.stream_state = TranscriptStreamState::Committed;
            }
        }
    }

    fn set_prompt_status(&mut self, prompt_status: Option<PromptStatusProjection>) {
        self.prompt_in_flight = prompt_status.is_some_and(prompt_status_is_active);
        self.prompt_status = prompt_status;
    }

    fn app_status(&self) -> AppStatus {
        if let Some(error) = self.prompt_error.as_ref().or(self.startup_error.as_ref()) {
            return AppStatus::Error(error.clone());
        }

        match self.prompt_status {
            Some(PromptStatusProjection::AwaitingToolApproval) => AppStatus::AwaitingToolApproval,
            Some(PromptStatusProjection::RunningTool) => AppStatus::RunningTool,
            Some(PromptStatusProjection::Running) => AppStatus::Generating,
            _ => AppStatus::Idle,
        }
    }

    #[cfg(test)]
    fn history_cells(&mut self) -> &DerivedHistoryCells {
        self.ensure_conversation_cache_fresh();
        &self.conversation_cache.history_cells
    }

    fn transcript_rows_ref(&self) -> &[TranscriptRowProjection] {
        &self.conversation_cache.transcript_rows
    }

    fn conversation_entries_ref(&self) -> &[ConversationEntryProjection] {
        &self.conversation_cache.conversation_entries
    }

    pub fn conversation_entries(&self) -> Vec<ConversationEntryProjection> {
        self.conversation_entries_ref().to_vec()
    }

    pub fn transcript_rows(&self) -> Vec<TranscriptRowProjection> {
        self.transcript_rows_ref().to_vec()
    }

    fn tool_statuses_ref(&self) -> &[ToolStatusProjection] {
        &self.conversation_cache.tool_statuses
    }

    pub fn tool_statuses(&self) -> Vec<ToolStatusProjection> {
        self.tool_statuses_ref().to_vec()
    }

    fn transcript_lines(&self) -> &[Line<'static>] {
        &self.conversation_cache.transcript_lines
    }

    fn scroll_transcript_up(&mut self, lines: u16) {
        self.transcript_follow_tail = false;
        self.transcript_scroll_top = self.transcript_scroll_top.saturating_sub(lines);
    }

    fn scroll_transcript_down(&mut self, lines: u16) {
        self.transcript_follow_tail = false;
        self.transcript_scroll_top = self.transcript_scroll_top.saturating_add(lines);
    }

    fn jump_transcript_top(&mut self) {
        self.transcript_follow_tail = false;
        self.transcript_scroll_top = 0;
    }

    fn jump_transcript_bottom(&mut self) {
        self.transcript_follow_tail = true;
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
    activity_sequence: u64,
    wake_signal: Arc<ProjectionWakeSignal>,
}

impl Default for ProjectionSharedState {
    fn default() -> Self {
        let session_roots = AcpSessionRoots::default();
        let wake_signal = Arc::new(ProjectionWakeSignal::default());
        Self {
            projection: TuiProjectionState::default(),
            pending_permission_request: None,
            filesystem: AcpFilesystemService::with_session_roots(session_roots.clone()),
            terminal: AcpTerminalService::with_session_roots(session_roots),
            activity_sequence: 0,
            wake_signal,
        }
    }
}

impl ProjectionSharedState {
    fn snapshot(&mut self) -> ProjectionSnapshot {
        self.projection.ensure_conversation_cache_fresh();
        ProjectionSnapshot {
            projection: self.projection.clone(),
            activity_sequence: self.activity_sequence,
        }
    }

    fn wake_signal(&self) -> Arc<ProjectionWakeSignal> {
        Arc::clone(&self.wake_signal)
    }

    fn mark_activity(&mut self) {
        self.activity_sequence = self.activity_sequence.wrapping_add(1);
        self.wake_signal.wake();
    }

    fn apply_session_list(&mut self, sessions: Vec<SessionBrowserEntryProjection>) {
        self.projection.apply_session_list(sessions);
        self.mark_activity();
    }

    fn clear_pending_permission_request(&mut self) {
        if let Some(pending_request) = self.pending_permission_request.take() {
            let _ = pending_request
                .response_sender
                .send(cancelled_permission_response());
        }
        self.projection.pending_permission = None;
        self.mark_activity();
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
        self.mark_activity();
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

        self.projection.apply_permission_selection(option_id);
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

    fn mark_spawned(&mut self, binary_path: PathBuf, pid: u32) {
        self.projection.mark_spawned(binary_path, pid);
        self.mark_activity();
    }

    fn mark_initialized(
        &mut self,
        binary_path: PathBuf,
        pid: u32,
        response: &acp::InitializeResponse,
    ) {
        self.projection.mark_initialized(binary_path, pid, response);
        self.mark_activity();
    }

    fn mark_startup_error(&mut self, message: String) {
        self.projection.mark_startup_error(message);
        self.mark_activity();
    }

    fn mark_session_created(&mut self, session_id: acp::SessionId) {
        self.clear_pending_permission_request();
        self.projection.mark_session_created(session_id);
        self.mark_activity();
    }

    fn prepare_session_load(&mut self, session_id: &acp::SessionId) {
        self.clear_pending_permission_request();
        self.projection.prepare_session_load(session_id);
        self.mark_activity();
    }

    fn set_draft_input(&mut self, draft_input: impl Into<String>) {
        self.projection.set_draft_input(draft_input);
        self.mark_activity();
    }

    fn scroll_transcript_up(&mut self, lines: u16) {
        self.projection.scroll_transcript_up(lines);
        self.mark_activity();
    }

    fn scroll_transcript_down(&mut self, lines: u16) {
        self.projection.scroll_transcript_down(lines);
        self.mark_activity();
    }

    fn jump_transcript_top(&mut self) {
        self.projection.jump_transcript_top();
        self.mark_activity();
    }

    fn jump_transcript_bottom(&mut self) {
        self.projection.jump_transcript_bottom();
        self.mark_activity();
    }

    fn mark_prompt_started(&mut self) {
        self.projection.mark_prompt_started();
        self.mark_activity();
    }

    fn mark_prompt_finished(&mut self, stop_reason: acp::StopReason) {
        self.projection.mark_prompt_finished(stop_reason);
        self.mark_activity();
    }

    fn mark_prompt_error(&mut self, message: String) {
        self.projection.mark_prompt_error(message);
        self.mark_activity();
    }

    fn apply_loaded_session_projection(&mut self, metadata: LoadedSessionMetadataProjection) {
        self.projection.apply_loaded_session_projection(metadata);
        self.mark_activity();
    }

    fn apply_session_notification(&mut self, notification: acp::SessionNotification) {
        self.projection.apply_session_notification(notification);
        self.mark_activity();
    }

    fn mark_external_activity(&mut self) {
        self.mark_activity();
    }
}

#[derive(Debug, Clone)]
struct ProjectionSnapshot {
    projection: TuiProjectionState,
    activity_sequence: u64,
}

#[derive(Debug, Default)]
struct ProjectionWakeSignal {
    sequence: AtomicU64,
    notify: Notify,
}

impl ProjectionWakeSignal {
    fn wake(&self) {
        self.sequence.fetch_add(1, Ordering::Release);
        self.notify.notify_one();
    }

    async fn wait_for_activity(&self, observed_sequence: u64) {
        loop {
            let notified = self.notify.notified();
            if self.sequence.load(Ordering::Acquire) > observed_sequence {
                return;
            }
            notified.await;
            if self.sequence.load(Ordering::Acquire) > observed_sequence {
                return;
            }
        }
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
        let mut guard = self.projection.lock().await;
        guard.projection.ensure_conversation_cache_fresh();
        guard.projection.clone()
    }

    async fn projection_activity_snapshot(&self) -> ProjectionActivitySnapshot {
        let snapshot = self.projection.lock().await.snapshot();
        ProjectionActivitySnapshot {
            projection: snapshot.projection,
            activity_sequence: snapshot.activity_sequence,
        }
    }

    async fn wait_for_projection_activity(
        &self,
        observed_sequence: u64,
    ) -> ProjectionActivitySnapshot {
        let wake_signal = { self.projection.lock().await.wake_signal() };
        wake_signal.wait_for_activity(observed_sequence).await;
        self.projection_activity_snapshot().await
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

    async fn apply_loaded_session_projection(&self, metadata: LoadedSessionMetadataProjection) {
        self.projection
            .lock()
            .await
            .apply_loaded_session_projection(metadata);
    }

    async fn apply_session_list(&self, sessions: Vec<SessionBrowserEntryProjection>) {
        self.projection.lock().await.apply_session_list(sessions);
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
    pub async fn projection_activity_snapshot_for_tests(&self) -> ProjectionActivitySnapshot {
        self.inner.projection_activity_snapshot().await
    }

    #[doc(hidden)]
    pub async fn wait_for_projection_activity_for_tests(
        &self,
        observed_sequence: u64,
    ) -> ProjectionActivitySnapshot {
        self.inner
            .wait_for_projection_activity(observed_sequence)
            .await
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
        self.inner
            .register_session_cwd(session_id.clone(), cwd.clone())
            .await?;
        let mut loaded_metadata = loaded_session_metadata_from_response(&response);
        if !loaded_metadata.is_complete()
            && let Ok(Some(session_metadata)) = load_local_session_metadata(&session_id)
        {
            loaded_metadata.apply_fallback_session(&session_metadata);
        }
        self.inner
            .apply_loaded_session_projection(loaded_metadata)
            .await;
        self.refresh_session_browser_after_session_change(cwd)
            .await?;
        Ok(response)
    }

    async fn refresh_session_browser_after_session_change(
        &self,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<()> {
        let cwd = cwd.into();
        match self.refresh_session_browser(cwd.clone()).await {
            Ok(_) => Ok(()),
            Err(error) if session_browser_refresh_is_legacy_decode_error(&error) => {
                warn!(
                    cwd = %cwd.display(),
                    error = %error,
                    "ignoring legacy ACP session browser refresh failure"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn refresh_session_browser(
        &self,
        cwd: impl Into<PathBuf>,
    ) -> acp::Result<Vec<SessionBrowserEntryProjection>> {
        let cwd = cwd.into();
        let request = acp::ListSessionsRequest::new().cwd(cwd);
        let response = self.connection.list_sessions(request).await?;
        let sessions = session_browser_entries_from_response(response);
        self.inner.apply_session_list(sessions.clone()).await;
        Ok(sessions)
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
    let mut input = ProjectionLoopInput::spawn()?;
    loop {
        if run_projection_iteration(controller, &mut input, |snapshot| {
            terminal.draw(|frame| render_projection(frame, snapshot))?;
            Ok(())
        })
        .await?
        {
            break;
        }
    }

    Ok(())
}

async fn run_projection_iteration<Draw>(
    controller: &mut ProjectionController,
    input: &mut ProjectionLoopInput,
    draw: Draw,
) -> Result<bool>
where
    Draw: FnOnce(&TuiProjectionState) -> Result<()>,
{
    controller.poll_active_prompt().await?;
    let snapshot = controller.snapshot().await;
    draw(&snapshot.projection)?;
    match wait_for_projection_action(controller, input, &snapshot).await? {
        ProjectionWaitOutcome::Activity => {
            drain_projection_activity_burst(controller, snapshot.activity_sequence).await?;
            apply_projection_action(controller, ProjectionAction::None).await
        }
        ProjectionWaitOutcome::Action(action) => apply_projection_action(controller, action).await,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectionWaitOutcome {
    Activity,
    Action(ProjectionAction),
}

async fn wait_for_projection_action(
    controller: &ProjectionController,
    input: &mut ProjectionLoopInput,
    snapshot: &ProjectionSnapshot,
) -> Result<ProjectionWaitOutcome> {
    if let Some(event) = input.try_next_event()? {
        return Ok(ProjectionWaitOutcome::Action(projection_action_from_event(
            &snapshot.projection,
            event,
        )));
    }

    let activity_wait = controller.wait_for_activity(snapshot.activity_sequence);
    tokio::pin!(activity_wait);

    let input_wait = input.next_event();
    tokio::pin!(input_wait);

    tokio::select! {
        biased;
        () = &mut activity_wait => Ok(ProjectionWaitOutcome::Activity),
        result = &mut input_wait => {
            let event = result?;
            Ok(ProjectionWaitOutcome::Action(projection_action_from_event(
                &snapshot.projection,
                event,
            )))
        }
    }
}

async fn drain_projection_activity_burst(
    controller: &mut ProjectionController,
    mut observed_sequence: u64,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + PROJECTION_ACTIVITY_BURST_DRAIN_BUDGET;
    loop {
        tokio::task::yield_now().await;
        controller.poll_active_prompt().await?;

        let latest_sequence = controller.activity_sequence().await;
        if latest_sequence == observed_sequence || tokio::time::Instant::now() >= deadline {
            break;
        }

        observed_sequence = latest_sequence;
    }

    Ok(())
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
        ProjectionAction::PreviousSession => {
            controller
                .switch_session(SessionBrowserDirection::Previous)
                .await?;
            Ok(false)
        }
        ProjectionAction::NextSession => {
            controller
                .switch_session(SessionBrowserDirection::Next)
                .await?;
            Ok(false)
        }
        ProjectionAction::ScrollUp => {
            controller.scroll_transcript_up(1).await;
            Ok(false)
        }
        ProjectionAction::ScrollDown => {
            controller.scroll_transcript_down(1).await;
            Ok(false)
        }
        ProjectionAction::PageUp => {
            controller
                .scroll_transcript_up(PROJECTION_PAGE_SCROLL_LINES)
                .await;
            Ok(false)
        }
        ProjectionAction::PageDown => {
            controller
                .scroll_transcript_down(PROJECTION_PAGE_SCROLL_LINES)
                .await;
            Ok(false)
        }
        ProjectionAction::JumpTop => {
            controller.jump_transcript_top().await;
            Ok(false)
        }
        ProjectionAction::JumpBottom => {
            controller.jump_transcript_bottom().await;
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

    let body_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SESSION_BROWSER_WIDTH),
            Constraint::Min(10),
        ])
        .split(layout[1]);

    let session_browser = Paragraph::new(Text::from(session_browser_lines(projection)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border)
                .title(Span::styled(" sessions ", TUI_THEME.title)),
        )
        .scroll((session_browser_scroll(projection, body_layout[0]), 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(session_browser, body_layout[0]);

    let body_lines = projection_lines(projection);
    let body_scroll = projection_body_scroll(projection, &body_lines, body_layout[1]);
    let body = Paragraph::new(Text::from(body_lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border)
                .title(Span::styled(" conversation ", TUI_THEME.title)),
        )
        .scroll((body_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(body, body_layout[1]);

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
        "Type a prompt and press Enter. Ctrl-J/K switch sessions. Ctrl-N starts a new ACP session. q/Esc/Ctrl-C exits."
    }
}

fn session_browser_lines(projection: &TuiProjectionState) -> Vec<Line<'static>> {
    if projection.sessions.is_empty() {
        return vec![
            Line::default(),
            Line::styled("  No sessions yet.", TUI_THEME.text_muted),
        ];
    }

    projection
        .sessions
        .iter()
        .map(|session| {
            let is_current = projection.session.session_id.as_deref() == Some(&session.session_id);
            let prefix_style = if is_current {
                TUI_THEME.assistant_accent
            } else {
                TUI_THEME.text_muted
            };
            let label_style = if is_current {
                TUI_THEME.text
            } else {
                TUI_THEME.text_muted
            };
            let title = session
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(&session.session_id);
            let label = truncate_projection_text(
                &format!("{title} · {}", abbreviated_session_id(&session.session_id)),
                SESSION_BROWSER_WIDTH.saturating_sub(4) as usize,
            );

            Line::from(vec![
                Span::styled(if is_current { "› " } else { "  " }, prefix_style),
                Span::styled(label, label_style),
            ])
        })
        .collect()
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

    lines.push(Line::from(vec![
        Span::styled("Replay fidelity: ", TUI_THEME.text_muted),
        Span::styled(
            match projection.replay_fidelity {
                ReplayFidelityProjection::Exact => "exact",
                ReplayFidelityProjection::Approximate => "approximate",
            },
            match projection.replay_fidelity {
                ReplayFidelityProjection::Exact => TUI_THEME.success,
                ReplayFidelityProjection::Approximate => TUI_THEME.warning,
            },
        ),
    ]));

    if let Some(prompt_status) = projection.prompt_status {
        lines.push(Line::from(vec![
            Span::styled("Prompt status: ", TUI_THEME.text_muted),
            Span::styled(
                prompt_status_label(prompt_status),
                prompt_status_style(prompt_status),
            ),
        ]));
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

    if !projection.transcript_lines().is_empty() {
        lines.push(Line::default());
        lines.extend(projection.transcript_lines().iter().cloned());
    }

    lines
}

fn projection_body_scroll(projection: &TuiProjectionState, lines: &[Line<'_>], area: Rect) -> u16 {
    resolve_transcript_scroll(
        lines,
        area.width,
        area.height,
        projection.transcript_follow_tail,
        projection.transcript_scroll_top,
    )
}

fn session_browser_scroll(projection: &TuiProjectionState, area: Rect) -> u16 {
    let inner_height = area.height.saturating_sub(2) as usize;
    if inner_height == 0 {
        return 0;
    }

    let Some(current_index) = projection
        .session
        .session_id
        .as_deref()
        .and_then(|session_id| {
            projection
                .sessions
                .iter()
                .position(|session| session.session_id == session_id)
        })
    else {
        return 0;
    };

    current_index
        .saturating_add(1)
        .saturating_sub(inner_height)
        .min(u16::MAX as usize) as u16
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

fn session_browser_entries_from_response(
    response: acp::ListSessionsResponse,
) -> Vec<SessionBrowserEntryProjection> {
    response
        .sessions
        .into_iter()
        .map(|session| SessionBrowserEntryProjection {
            session_id: session.session_id.to_string(),
            title: session.title,
            updated_at: session.updated_at,
        })
        .collect()
}

fn session_browser_refresh_is_legacy_decode_error(error: &acp::Error) -> bool {
    error.code == acp::ErrorCode::InternalError
        && (session_browser_refresh_error_contains_legacy_decode_hint(&error.message)
            || error
                .data
                .as_ref()
                .is_some_and(session_browser_refresh_error_data_contains_legacy_decode_hint))
}

fn session_browser_refresh_error_data_contains_legacy_decode_hint(data: &Value) -> bool {
    match data {
        Value::String(message) => {
            session_browser_refresh_error_contains_legacy_decode_hint(message)
        }
        Value::Array(values) => values
            .iter()
            .any(session_browser_refresh_error_data_contains_legacy_decode_hint),
        Value::Object(fields) => fields
            .values()
            .any(session_browser_refresh_error_data_contains_legacy_decode_hint),
        _ => false,
    }
}

fn session_browser_refresh_error_contains_legacy_decode_hint(message: &str) -> bool {
    message.contains("failed to deserialize response")
        || message.contains("missing field `cwd`")
        || message.contains("missing field cwd")
}

fn abbreviated_session_id(session_id: &str) -> &str {
    session_id.split('-').next().unwrap_or(session_id)
}

fn truncate_projection_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() && max_chars > 0 {
        let mut shortened = truncated
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        shortened.push('…');
        shortened
    } else {
        truncated
    }
}

fn conversation_entries_from_history_cells(
    history_cells: &DerivedHistoryCells,
) -> Vec<ConversationEntryProjection> {
    history_cells
        .iter_rows()
        .map(ConversationEntryProjection::from_row)
        .collect()
}

#[cfg(test)]
fn transcript_rows_from_history_cells(
    history_cells: &DerivedHistoryCells,
) -> Vec<TranscriptRowProjection> {
    let conversation_entries = conversation_entries_from_history_cells(history_cells);
    transcript_rows_from_entries(&conversation_entries)
}

#[cfg(test)]
fn tool_statuses_from_history_cells(
    history_cells: &DerivedHistoryCells,
) -> Vec<ToolStatusProjection> {
    let conversation_entries = conversation_entries_from_history_cells(history_cells);
    tool_statuses_from_entries(&conversation_entries)
}

#[cfg(test)]
fn uncached_history_cells(projection: &TuiProjectionState) -> DerivedHistoryCells {
    derive_history_cells_for_session(
        &projection.transcript_session,
        &projection.app_status(),
        projection.active_run_id,
    )
}

#[cfg(test)]
fn uncached_conversation_entries(
    projection: &TuiProjectionState,
) -> Vec<ConversationEntryProjection> {
    conversation_entries_from_history_cells(&uncached_history_cells(projection))
}

#[cfg(test)]
fn uncached_transcript_rows(projection: &TuiProjectionState) -> Vec<TranscriptRowProjection> {
    transcript_rows_from_history_cells(&uncached_history_cells(projection))
}

#[cfg(test)]
fn uncached_tool_statuses(projection: &TuiProjectionState) -> Vec<ToolStatusProjection> {
    tool_statuses_from_history_cells(&uncached_history_cells(projection))
}

#[cfg(test)]
fn uncached_projection_lines(projection: &TuiProjectionState) -> Vec<Line<'static>> {
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

    lines.push(Line::from(vec![
        Span::styled("Replay fidelity: ", TUI_THEME.text_muted),
        Span::styled(
            match projection.replay_fidelity {
                ReplayFidelityProjection::Exact => "exact",
                ReplayFidelityProjection::Approximate => "approximate",
            },
            match projection.replay_fidelity {
                ReplayFidelityProjection::Exact => TUI_THEME.success,
                ReplayFidelityProjection::Approximate => TUI_THEME.warning,
            },
        ),
    ]));

    if let Some(prompt_status) = projection.prompt_status {
        lines.push(Line::from(vec![
            Span::styled("Prompt status: ", TUI_THEME.text_muted),
            Span::styled(
                prompt_status_label(prompt_status),
                prompt_status_style(prompt_status),
            ),
        ]));
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

    let transcript_lines =
        transcript_lines_from_entries(&uncached_conversation_entries(projection));
    if !transcript_lines.is_empty() {
        lines.push(Line::default());
        lines.extend(transcript_lines);
    }

    lines
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

    projection.lock().await.mark_initialized(
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
    PreviousSession,
    NextSession,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    UpdateDraft(String),
    SubmitPrompt,
    Select(String),
    CancelPendingPermission,
    CancelActivePrompt,
}

fn projection_action_from_event(snapshot: &TuiProjectionState, event: Event) -> ProjectionAction {
    match event {
        Event::Paste(text) if snapshot.can_edit_draft() => {
            let mut next = snapshot.draft_input.clone();
            next.push_str(&text);
            ProjectionAction::UpdateDraft(next)
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if snapshot.pending_permission.is_some() {
                permission_action_for_key(snapshot, key.code, key.modifiers)
            } else if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Up => ProjectionAction::ScrollUp,
                    KeyCode::Down => ProjectionAction::ScrollDown,
                    KeyCode::PageUp => ProjectionAction::PageUp,
                    KeyCode::PageDown => ProjectionAction::PageDown,
                    KeyCode::Home => ProjectionAction::JumpTop,
                    KeyCode::End => ProjectionAction::JumpBottom,
                    _ => projection_key_action(snapshot, key.code, key.modifiers),
                }
            } else {
                projection_key_action(snapshot, key.code, key.modifiers)
            }
        }
        _ => ProjectionAction::None,
    }
}

fn projection_key_action(
    snapshot: &TuiProjectionState,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ProjectionAction {
    if matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
    {
        if snapshot.prompt_in_flight {
            return ProjectionAction::CancelActivePrompt;
        }
        return ProjectionAction::Quit;
    }

    if code == KeyCode::Char('n')
        && modifiers.contains(KeyModifiers::CONTROL)
        && !snapshot.prompt_in_flight
    {
        return ProjectionAction::NewSession;
    }

    if snapshot.can_edit_draft() && modifiers == KeyModifiers::CONTROL {
        return match code {
            KeyCode::Char('k') => ProjectionAction::PreviousSession,
            KeyCode::Char('j') => ProjectionAction::NextSession,
            _ => ProjectionAction::None,
        };
    }

    if snapshot.can_edit_draft() {
        return draft_action_for_key(snapshot, code, modifiers);
    }

    ProjectionAction::None
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionBrowserDirection {
    Previous,
    Next,
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

    async fn snapshot(&self) -> ProjectionSnapshot {
        self.projection.lock().await.snapshot()
    }

    async fn activity_sequence(&self) -> u64 {
        self.projection.lock().await.activity_sequence
    }

    async fn wait_for_activity(&self, observed_sequence: u64) {
        let wake_signal = { self.projection.lock().await.wake_signal() };
        wake_signal.wait_for_activity(observed_sequence).await;
    }

    async fn set_draft_input(&self, draft_input: impl Into<String>) {
        self.projection.lock().await.set_draft_input(draft_input);
    }

    async fn scroll_transcript_up(&self, lines: u16) {
        self.projection.lock().await.scroll_transcript_up(lines);
    }

    async fn scroll_transcript_down(&self, lines: u16) {
        self.projection.lock().await.scroll_transcript_down(lines);
    }

    async fn jump_transcript_top(&self) {
        self.projection.lock().await.jump_transcript_top();
    }

    async fn jump_transcript_bottom(&self) {
        self.projection.lock().await.jump_transcript_bottom();
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

    async fn switch_session(&mut self, direction: SessionBrowserDirection) -> Result<()> {
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(());
        };

        let next_session_id = {
            let state = self.projection.lock().await;
            state.projection.adjacent_session_id(direction)
        };
        let Some(next_session_id) = next_session_id else {
            return Ok(());
        };

        runtime
            .load_session(
                acp::SessionId::new(next_session_id.clone()),
                self.cwd.clone(),
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                FluentCodeError::Provider(format!(
                    "failed to load ACP session `{next_session_id}` in `{}`: {error}",
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
        let projection = Arc::clone(&self.projection);
        let task = tokio::task::spawn_local(async move {
            let result = connection
                .prompt(acp::PromptRequest::new(
                    prompt_session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                ))
                .await;
            projection.lock().await.mark_external_activity();
            result
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
            Ok(Ok(response)) => {
                self.projection
                    .lock()
                    .await
                    .mark_prompt_finished(response.stop_reason);
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

        if let Some(runtime) = self.runtime.as_ref() {
            runtime
                .refresh_session_browser_after_session_change(self.cwd.clone())
                .await
                .map_err(|error| {
                    FluentCodeError::Provider(format!(
                        "failed to refresh ACP session browser in `{}`: {error}",
                        self.cwd.display()
                    ))
                })?;
        }

        Ok(())
    }
}

struct ProjectionLoopInput {
    receiver: mpsc::UnboundedReceiver<std::io::Result<Event>>,
    _reader_task: Option<JoinHandle<()>>,
}

impl ProjectionLoopInput {
    fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let mut stream = EventStream::new();
            while let Some(event) = stream.next().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            receiver: rx,
            _reader_task: Some(handle),
        })
    }

    #[cfg(test)]
    fn from_receiver(receiver: mpsc::UnboundedReceiver<std::io::Result<Event>>) -> Self {
        Self {
            receiver,
            _reader_task: None,
        }
    }

    fn try_next_event(&mut self) -> Result<Option<Event>> {
        match self.receiver.try_recv() {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) => Err(error.into()),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(FluentCodeError::Provider(
                "ACP projection input pump stopped unexpectedly".to_string(),
            )),
        }
    }

    async fn next_event(&mut self) -> Result<Event> {
        match self.receiver.recv().await {
            Some(Ok(event)) => Ok(event),
            Some(Err(error)) => Err(error.into()),
            None => Err(FluentCodeError::Provider(
                "ACP projection input pump stopped unexpectedly".to_string(),
            )),
        }
    }
}

impl Drop for ProjectionLoopInput {
    fn drop(&mut self) {
        if let Some(handle) = self._reader_task.take() {
            handle.abort();
        }
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

fn meta_value<T>(meta: Option<&acp::Meta>, key: &str) -> Option<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(meta?.get(key)?.clone()).ok()
}

fn meta_optional_string(meta: Option<&acp::Meta>, key: &str) -> Option<Option<String>> {
    let value = meta?.get(key)?;
    if value.is_null() {
        return Some(None);
    }

    serde_json::from_value::<String>(value.clone())
        .ok()
        .map(Some)
}

fn tool_invocation_from_meta(meta: Option<&acp::Meta>) -> Option<ToolInvocationRecord> {
    meta_value(meta, ACP_META_TOOL_INVOCATION_KEY)
}

fn transcript_item_from_meta(meta: Option<&acp::Meta>) -> Option<TranscriptItemRecord> {
    meta_value(meta, ACP_META_TRANSCRIPT_ITEM_KEY)
}

fn loaded_session_metadata_from_response(
    response: &acp::LoadSessionResponse,
) -> LoadedSessionMetadataProjection {
    let replay_fidelity =
        meta_optional_string(response.meta.as_ref(), ACP_META_REPLAY_FIDELITY_KEY)
            .and_then(|value| value)
            .and_then(|value| match value.as_str() {
                "exact" => Some(ReplayFidelityProjection::Exact),
                "approximate" => Some(ReplayFidelityProjection::Approximate),
                _ => None,
            });
    let prompt_status =
        match meta_optional_string(response.meta.as_ref(), ACP_META_LATEST_PROMPT_STATE_KEY) {
            Some(Some(value)) => match value.as_str() {
                "running" => Some(Some(PromptStatusProjection::Running)),
                "awaiting_tool_approval" => {
                    Some(Some(PromptStatusProjection::AwaitingToolApproval))
                }
                "running_tool" => Some(Some(PromptStatusProjection::RunningTool)),
                "completed" => Some(Some(PromptStatusProjection::Completed)),
                "cancelled" => Some(Some(PromptStatusProjection::Cancelled)),
                "failed" => Some(Some(PromptStatusProjection::Failed)),
                "interrupted" => Some(Some(PromptStatusProjection::Interrupted)),
                _ => None,
            },
            Some(None) => Some(None),
            None => None,
        };

    LoadedSessionMetadataProjection {
        replay_fidelity,
        prompt_status,
    }
}

fn load_local_session_metadata(session_id: &acp::SessionId) -> Result<Option<Session>> {
    let session_path = Config::load()?
        .data_dir
        .join("sessions")
        .join(session_id.to_string())
        .join("session.json");
    if !session_path.exists() {
        return Ok(None);
    }

    let session_contents = fs::read_to_string(&session_path)?;
    let mut session = serde_json::from_str::<Session>(&session_contents).map_err(|error| {
        FluentCodeError::Session(format!(
            "failed to parse ACP session metadata `{}`: {error}",
            session_path.display()
        ))
    })?;
    session.normalize_persistence();
    Ok(Some(session))
}

fn prompt_status_is_active(status: PromptStatusProjection) -> bool {
    matches!(
        status,
        PromptStatusProjection::Running
            | PromptStatusProjection::AwaitingToolApproval
            | PromptStatusProjection::RunningTool
    )
}

fn prompt_status_label(status: PromptStatusProjection) -> &'static str {
    match status {
        PromptStatusProjection::Running => "running",
        PromptStatusProjection::AwaitingToolApproval => "awaiting approval",
        PromptStatusProjection::RunningTool => "running tool",
        PromptStatusProjection::Completed => "completed",
        PromptStatusProjection::Cancelled => "cancelled",
        PromptStatusProjection::Failed => "failed",
        PromptStatusProjection::Interrupted => "interrupted",
    }
}

fn prompt_status_style(status: PromptStatusProjection) -> ratatui::style::Style {
    match status {
        PromptStatusProjection::Running | PromptStatusProjection::RunningTool => TUI_THEME.info,
        PromptStatusProjection::AwaitingToolApproval => TUI_THEME.warning,
        PromptStatusProjection::Completed => TUI_THEME.success,
        PromptStatusProjection::Cancelled
        | PromptStatusProjection::Failed
        | PromptStatusProjection::Interrupted => TUI_THEME.error,
    }
}

fn normalized_tool_name(title: &str, kind: Option<acp::ToolKind>) -> String {
    let base_title = title.split(" (").next().unwrap_or(title).trim();
    if !base_title.is_empty() {
        return base_title.replace(' ', "_");
    }

    match kind {
        Some(acp::ToolKind::Read) => "read".to_string(),
        Some(acp::ToolKind::Edit) => "edit".to_string(),
        Some(acp::ToolKind::Delete) => "delete".to_string(),
        Some(acp::ToolKind::Move) => "move".to_string(),
        Some(acp::ToolKind::Search) => "grep".to_string(),
        Some(acp::ToolKind::Execute) => "bash".to_string(),
        Some(acp::ToolKind::Think) => "task".to_string(),
        Some(acp::ToolKind::Fetch) => "fetch".to_string(),
        Some(acp::ToolKind::SwitchMode) => "switch_mode".to_string(),
        _ => "tool".to_string(),
    }
}

fn tool_execution_state(status: Option<acp::ToolCallStatus>) -> ToolExecutionState {
    match status {
        Some(acp::ToolCallStatus::InProgress) => ToolExecutionState::Running,
        Some(acp::ToolCallStatus::Completed) => ToolExecutionState::Completed,
        Some(acp::ToolCallStatus::Failed) => ToolExecutionState::Failed,
        _ => ToolExecutionState::NotStarted,
    }
}

fn tool_execution_label(
    execution_state: ToolExecutionState,
    approval_state: ToolApprovalState,
) -> String {
    if approval_state == ToolApprovalState::Pending {
        return "pending".to_string();
    }

    match execution_state {
        ToolExecutionState::NotStarted => "queued".to_string(),
        ToolExecutionState::Running => "in_progress".to_string(),
        ToolExecutionState::Completed => "completed".to_string(),
        ToolExecutionState::Failed => "failed".to_string(),
        ToolExecutionState::Skipped => "skipped".to_string(),
    }
}

fn transcript_rows_from_entries(
    conversation_entries: &[ConversationEntryProjection],
) -> Vec<TranscriptRowProjection> {
    let mut transcript_rows = Vec::new();

    for entry in conversation_entries {
        match &entry.kind {
            ConversationEntryKind::Turn(turn)
                if turn.role == Role::User && !turn.content.is_empty() =>
            {
                transcript_rows.push(TranscriptRowProjection {
                    source: TranscriptSource::User,
                    content: turn.content.clone(),
                });
            }
            ConversationEntryKind::Reasoning(reasoning) if !reasoning.content.is_empty() => {
                transcript_rows.push(TranscriptRowProjection {
                    source: TranscriptSource::Thought,
                    content: reasoning.content.clone(),
                });
            }
            ConversationEntryKind::Turn(turn)
                if turn.role == Role::Assistant && !turn.content.is_empty() =>
            {
                transcript_rows.push(TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: turn.content.clone(),
                });
            }
            _ => {}
        }
    }

    transcript_rows
}

fn tool_statuses_from_entries(
    conversation_entries: &[ConversationEntryProjection],
) -> Vec<ToolStatusProjection> {
    let mut tool_statuses = Vec::new();

    for entry in conversation_entries {
        match &entry.kind {
            ConversationEntryKind::Tool(tool) => tool_statuses.push(tool.clone()),
            ConversationEntryKind::ToolGroup(group) => {
                tool_statuses.extend(group.items.iter().cloned())
            }
            _ => {}
        }
    }

    tool_statuses
}

fn transcript_lines_from_entries(
    conversation_entries: &[ConversationEntryProjection],
) -> Vec<Line<'static>> {
    conversation_lines_from_rows(
        conversation_entries
            .iter()
            .map(ConversationEntryProjection::row),
        false,
    )
}

fn tool_status_projection(tool: &ToolRow) -> ToolStatusProjection {
    ToolStatusProjection {
        tool_call_id: tool.tool_call_id.clone(),
        title: tool.display_name.clone(),
        status: tool_execution_label(tool.execution_state, tool.approval_state),
    }
}

fn tool_output_previews(
    raw_output: Option<Value>,
    _content: Option<&[acp::ToolCallContent]>,
) -> (Option<String>, Option<String>) {
    if let Some(raw_output) = raw_output {
        if let Some(result) = raw_output.get("result") {
            return (Some(tool_output_value_to_string(result)), None);
        }
        if let Some(error) = raw_output.get("error") {
            return (None, Some(tool_output_value_to_string(error)));
        }
    }

    (None, None)
}

fn tool_output_value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
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
    let (input_sender, input_receiver) = mpsc::unbounded_channel();
    let mut input = ProjectionLoopInput::from_receiver(input_receiver);

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
    let projection_for_wake = Arc::clone(&projection);
    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            let agent_rows = snapshot
                .transcript_rows()
                .into_iter()
                .filter(|row| row.source == TranscriptSource::Agent)
                .collect::<Vec<_>>();
            assert_eq!(agent_rows.len(), 1);
            assert_eq!(agent_rows[0].content, "Mock assistant response");
            assert!(snapshot.prompt_in_flight);
            redraw_happened.store(true, Ordering::SeqCst);

            let projection_for_wake = Arc::clone(&projection_for_wake);
            tokio::spawn(async move {
                projection_for_wake.lock().await.mark_external_activity();
            });
            Ok(())
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            "timed out waiting for the ACP redraw loop to react to projection activity".to_string(),
        )
    })??;

    drop(input_sender);
    assert!(!should_quit);
    assert!(
        redraw_happened.load(Ordering::SeqCst),
        "expected streamed ACP output to redraw before the loop waited on any terminal input"
    );
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
fn regression_agent_message_chunk(text: &str) -> acp::SessionNotification {
    acp::SessionNotification::new(
        "session-1",
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )
}

#[cfg(test)]
fn quit_key_event() -> Event {
    Event::Key(crossterm::event::KeyEvent {
        code: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}

#[cfg(test)]
pub(crate) async fn assert_projection_loop_batches_notifications_without_starving_input()
-> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let mut controller =
        ProjectionController::new(Arc::clone(&projection), None, PathBuf::default());
    let (input_sender, input_receiver) = mpsc::unbounded_channel();
    let mut input = ProjectionLoopInput::from_receiver(input_receiver);

    {
        let mut state = projection.lock().await;
        state
            .projection
            .mark_session_created(acp::SessionId::new("session-1"));
    }

    let draw_count = AtomicU64::new(0);
    let projection_for_burst = Arc::clone(&projection);
    let input_sender_for_burst = input_sender.clone();
    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            assert_eq!(draw_count.fetch_add(1, Ordering::SeqCst), 0);
            assert!(snapshot.transcript_rows().is_empty());

            let projection_for_burst = Arc::clone(&projection_for_burst);
            let input_sender_for_burst = input_sender_for_burst.clone();
            tokio::spawn(async move {
                let _ = input_sender_for_burst.send(Ok(quit_key_event()));
                for chunk in ["burst ", "activity ", "done"] {
                    projection_for_burst
                        .lock()
                        .await
                        .apply_session_notification(regression_agent_message_chunk(chunk));
                    tokio::task::yield_now().await;
                }
            });
            Ok(())
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            "timed out waiting for the projection loop to coalesce a burst notification wake"
                .to_string(),
        )
    })??;

    assert!(!should_quit);

    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            assert_eq!(draw_count.fetch_add(1, Ordering::SeqCst), 1);
            assert_eq!(
                snapshot.transcript_rows(),
                vec![TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "burst activity done".to_string(),
                }]
            );
            Ok(())
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            "timed out waiting for the projection loop to redraw the coalesced burst snapshot"
                .to_string(),
        )
    })??;

    assert!(should_quit);
    assert_eq!(draw_count.load(Ordering::SeqCst), 2);

    Ok(())
}

#[cfg(test)]
pub(crate) async fn assert_projection_wake_does_not_miss_activity_with_release_acquire_ordering()
-> Result<()> {
    let wake_signal = ProjectionWakeSignal::default();

    wake_signal.wake();
    wake_signal.notify.notified().await;

    timeout(Duration::from_millis(100), wake_signal.wait_for_activity(0))
        .await
        .map_err(|_| {
            FluentCodeError::Provider(
                "timed out waiting for stale activity to be observed after its wake permit was consumed"
                    .to_string(),
            )
        })?;

    Ok(())
}

#[cfg(test)]
pub(crate) async fn assert_projection_wait_for_activity_still_blocks_without_new_sequence()
-> Result<()> {
    let wake_signal = Arc::new(ProjectionWakeSignal::default());

    wake_signal.wake();
    wake_signal.notify.notified().await;

    let observed_sequence = wake_signal.sequence.load(Ordering::Acquire);
    let wait_signal = Arc::clone(&wake_signal);
    let wait_task = tokio::spawn(async move {
        wait_signal.wait_for_activity(observed_sequence).await;
    });

    tokio::task::yield_now().await;
    if wait_task.is_finished() {
        return Err(FluentCodeError::Provider(
            "projection wait_for_activity should remain blocked until a newer activity sequence arrives"
                .to_string(),
        ));
    }

    wake_signal.wake();
    timeout(Duration::from_millis(100), wait_task)
        .await
        .map_err(|_| {
            FluentCodeError::Provider(
                "timed out waiting for wait_for_activity to resume after a newer activity sequence"
                    .to_string(),
            )
        })?
        .expect("projection wait task should join cleanly");

    Ok(())
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn assert_projection_wait_path_stays_idle_until_activity_wake() -> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let wake_signal = { projection.lock().await.wake_signal() };
    let wait_task = tokio::spawn(async move {
        wake_signal.wait_for_activity(0).await;
    });

    tokio::task::yield_now().await;
    assert!(
        !wait_task.is_finished(),
        "expected the ACP projection wait path to remain idle until projection activity arrives"
    );

    projection.lock().await.mark_external_activity();
    timeout(Duration::from_millis(100), wait_task)
        .await
        .map_err(|_| {
            FluentCodeError::Provider(
                "timed out waiting for the ACP projection wait path to wake after activity"
                    .to_string(),
            )
        })?
        .expect("projection wait task should join cleanly");

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
    let (input_sender, input_receiver) = mpsc::unbounded_channel();
    let mut input = ProjectionLoopInput::from_receiver(input_receiver);

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
    input_sender
        .send(Ok(Event::Key(crossterm::event::KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })))
        .map_err(|_| {
            FluentCodeError::Provider(
                "failed to queue the ACP projection quit input for the regression test".to_string(),
            )
        })?;

    let redraw_happened = AtomicBool::new(false);
    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            assert!(
                !snapshot.prompt_in_flight,
                "expected {outcome:?} prompt completion to flush the terminal projection before waiting for more input"
            );
            assert_eq!(
                snapshot.transcript_rows(),
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
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            format!(
                "timed out waiting for the final {outcome:?} ACP prompt state to flush"
            ),
        )
    })??;

    assert!(should_quit);
    assert!(
        redraw_happened.load(Ordering::SeqCst),
        "expected the final {outcome:?} terminal-state redraw to happen before the queued quit input was handled"
    );
    assert!(
        controller.active_prompt.is_none(),
        "expected polling the {outcome:?} prompt terminal outcome to clear the active prompt handle"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) async fn assert_projection_loop_flushes_terminal_update_before_queued_quit_under_burst_activity()
-> Result<()> {
    let projection = Arc::new(Mutex::new(ProjectionSharedState::default()));
    let mut controller =
        ProjectionController::new(Arc::clone(&projection), None, PathBuf::default());
    let (input_sender, input_receiver) = mpsc::unbounded_channel();
    let mut input = ProjectionLoopInput::from_receiver(input_receiver);

    {
        let mut state = projection.lock().await;
        state
            .projection
            .mark_session_created(acp::SessionId::new("session-1"));
        state.projection.mark_prompt_started();
        state.apply_session_notification(regression_agent_message_chunk("streamed "));
    }

    let projection_for_prompt = Arc::clone(&projection);
    controller.active_prompt = Some(ActivePromptRequest {
        session_id: acp::SessionId::new("session-1"),
        task: tokio::spawn(async move {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            projection_for_prompt.lock().await.mark_external_activity();
            Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
        }),
    });

    let draw_count = AtomicU64::new(0);
    let projection_for_burst = Arc::clone(&projection);
    let input_sender_for_burst = input_sender.clone();
    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            assert_eq!(draw_count.fetch_add(1, Ordering::SeqCst), 0);
            assert!(snapshot.prompt_in_flight);
            assert_eq!(
                snapshot.transcript_rows(),
                vec![TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "streamed ".to_string(),
                }]
            );

            let projection_for_burst = Arc::clone(&projection_for_burst);
            let input_sender_for_burst = input_sender_for_burst.clone();
            tokio::spawn(async move {
                let _ = input_sender_for_burst.send(Ok(quit_key_event()));
                for chunk in ["final ", "response"] {
                    projection_for_burst
                        .lock()
                        .await
                        .apply_session_notification(regression_agent_message_chunk(chunk));
                    tokio::task::yield_now().await;
                }
            });
            Ok(())
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            "timed out waiting for the burst activity wake before the queued quit input"
                .to_string(),
        )
    })??;

    assert!(!should_quit);

    let should_quit = timeout(
        Duration::from_millis(100),
        run_projection_iteration(&mut controller, &mut input, |snapshot| {
            assert_eq!(draw_count.fetch_add(1, Ordering::SeqCst), 1);
            assert!(
                !snapshot.prompt_in_flight,
                "expected the prompt completion to flush before the queued quit was applied"
            );
            assert!(snapshot.prompt_error.is_none());
            assert_eq!(
                snapshot.transcript_rows(),
                vec![TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "streamed final response".to_string(),
                }]
            );
            Ok(())
        }),
    )
    .await
    .map_err(|_| {
        FluentCodeError::Provider(
            "timed out waiting for the prompt completion snapshot to flush under burst activity"
                .to_string(),
        )
    })??;

    assert!(should_quit);
    assert_eq!(draw_count.load(Ordering::SeqCst), 2);
    assert!(controller.active_prompt.is_none());

    Ok(())
}

const TEST_FRAME_WIDTH: u16 = 110;
const TEST_FRAME_HEIGHT: u16 = 28;

fn normalize_projection_buffer(buffer: &Buffer) -> String {
    let mut lines = Vec::with_capacity(buffer.area.height as usize);

    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        lines.push(line.trim_end_matches(' ').to_string());
    }

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

fn expected_projection_panel_lines(
    title: &str,
    width: u16,
    height: u16,
    inner_lines: &[&str],
) -> Vec<String> {
    assert!(
        height >= 2,
        "panel height must include top and bottom borders"
    );

    let inner_width = width.saturating_sub(2) as usize;
    let inner_height = height.saturating_sub(2) as usize;
    assert!(
        inner_lines.len() <= inner_height,
        "expected at most {inner_height} lines inside `{title}`, got {}",
        inner_lines.len()
    );

    let mut lines = Vec::with_capacity(height as usize);
    let title_width = title.chars().count();
    let top_fill = width as usize - 2 - title_width;
    lines.push(format!("┌{title}{}┐", "─".repeat(top_fill)));

    for line in inner_lines {
        let visible_width = line.chars().count();
        assert!(
            visible_width <= inner_width,
            "line `{line}` exceeds panel width {inner_width}"
        );
        lines.push(format!(
            "│{line}{: <padding$}│",
            "",
            padding = inner_width - visible_width
        ));
    }

    for _ in inner_lines.len()..inner_height {
        lines.push(format!("│{}│", " ".repeat(inner_width)));
    }

    lines.push(format!("└{}┘", "─".repeat(inner_width)));
    lines
}

#[doc(hidden)]
pub fn expected_projection_frame_text_for_tests(
    status_line: &str,
    session_lines: &[&str],
    conversation_lines: &[&str],
    draft_input: &str,
    footer_line: &str,
) -> String {
    let body_height = TEST_FRAME_HEIGHT - 1 - 3 - 1;
    let mut lines = Vec::new();
    lines.push(status_line.to_string());
    let session_panel_lines = expected_projection_panel_lines(
        " sessions ",
        SESSION_BROWSER_WIDTH,
        body_height,
        session_lines,
    );
    let conversation_panel_lines = expected_projection_panel_lines(
        " conversation ",
        TEST_FRAME_WIDTH.saturating_sub(SESSION_BROWSER_WIDTH),
        body_height,
        conversation_lines,
    );
    lines.extend(
        session_panel_lines
            .into_iter()
            .zip(conversation_panel_lines)
            .map(|(session_line, conversation_line)| format!("{session_line}{conversation_line}")),
    );
    lines.extend(expected_projection_panel_lines(
        " > ",
        TEST_FRAME_WIDTH,
        3,
        &[draft_input],
    ));
    lines.push(footer_line.to_string());
    lines.join("\n")
}

#[doc(hidden)]
pub fn render_projection_frame_text_for_tests(projection: &TuiProjectionState) -> String {
    let backend = TestBackend::new(TEST_FRAME_WIDTH, TEST_FRAME_HEIGHT);
    let mut terminal = Terminal::new(backend).expect("create ACP render regression terminal");
    terminal
        .draw(|frame| render_projection(frame, projection))
        .expect("draw ACP projection regression frame");
    normalize_projection_buffer(terminal.backend().buffer())
}

#[cfg(test)]
fn projection_with_session(
    session_id: &str,
    session: Session,
    prompt_status: Option<PromptStatusProjection>,
    active_run_id: Option<Uuid>,
    pending_permission: Option<PendingPermissionProjection>,
) -> TuiProjectionState {
    let mut projection = TuiProjectionState::default();
    projection.session.session_id = Some(session_id.to_string());
    projection.sessions = vec![SessionBrowserEntryProjection {
        session_id: session_id.to_string(),
        title: Some(session.title.clone()),
        updated_at: Some(session.updated_at.to_rfc3339()),
    }];
    projection.replay_fidelity = match session.transcript_fidelity {
        TranscriptFidelity::Approximate => ReplayFidelityProjection::Approximate,
        TranscriptFidelity::Exact => ReplayFidelityProjection::Exact,
    };
    projection.transcript_session = Arc::new(session);
    projection.pending_permission = pending_permission;
    projection.active_run_id = active_run_id;
    projection.set_prompt_status(prompt_status);
    projection.ensure_conversation_cache_fresh();
    projection
}

#[cfg(test)]
fn make_turn(run_id: Uuid, role: Role, content: &str, sequence_number: u64) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        run_id,
        role,
        content: content.to_string(),
        reasoning: String::new(),
        sequence_number,
        timestamp: Utc::now(),
    }
}

#[cfg(test)]
fn make_tool_invocation(
    run_id: Uuid,
    preceding_turn_id: Option<Uuid>,
    tool_name: &str,
    arguments: Value,
    sequence_number: u64,
) -> ToolInvocationRecord {
    ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: format!("call-{sequence_number}"),
        tool_name: tool_name.to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments,
        preceding_turn_id,
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Completed,
        result: None,
        error: None,
        delegation: None,
        sequence_number,
        requested_at: Utc::now(),
        approved_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
    }
}

#[cfg(test)]
fn completed_projection_for_regression() -> TuiProjectionState {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("completed transcript");
    let user_turn = make_turn(run_id, Role::User, "first prompt", 1);
    let assistant_turn = make_turn(run_id, Role::Assistant, "first answer", 2);

    session
        .turns
        .extend([user_turn.clone(), assistant_turn.clone()]);
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::from_turn(&assistant_turn),
    ];
    session.upsert_run(
        run_id,
        fluent_code_app::session::model::RunStatus::Completed,
    );

    projection_with_session(
        "session-completed",
        session,
        Some(PromptStatusProjection::Completed),
        None,
        None,
    )
}

#[cfg(test)]
fn streaming_projection_for_regression() -> TuiProjectionState {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("streaming transcript");
    let user_turn = make_turn(run_id, Role::User, "follow-up", 1);
    let assistant_turn_id = Uuid::new_v4();

    session.turns.push(user_turn.clone());
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::assistant_text(
            run_id,
            assistant_turn_id,
            2,
            "streamed answer in progress",
            TranscriptStreamState::Open,
        ),
    ];

    projection_with_session(
        "session-streaming",
        session,
        Some(PromptStatusProjection::Running),
        Some(run_id),
        None,
    )
}

#[cfg(test)]
fn permission_projection_for_regression() -> TuiProjectionState {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("permission transcript");
    let user_turn = make_turn(run_id, Role::User, "inspect src/main.rs", 1);
    let mut invocation = make_tool_invocation(
        run_id,
        Some(user_turn.id),
        "read",
        json!({"path": "src/main.rs"}),
        2,
    );
    invocation.approval_state = ToolApprovalState::Pending;
    invocation.execution_state = ToolExecutionState::NotStarted;
    invocation.approved_at = None;
    invocation.completed_at = None;

    session.turns.push(user_turn.clone());
    session.tool_invocations.push(invocation.clone());
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::from_tool_invocation(&invocation),
        TranscriptItemRecord::permission(
            &invocation,
            3,
            fluent_code_app::session::model::TranscriptPermissionState::Pending,
            None,
        ),
    ];

    projection_with_session(
        "session-permission",
        session,
        Some(PromptStatusProjection::AwaitingToolApproval),
        Some(run_id),
        Some(PendingPermissionProjection {
            tool_call_id: invocation.tool_call_id.clone(),
            title: "read src/main.rs".to_string(),
            options: vec![PermissionOptionProjection {
                option_id: "allow_once".to_string(),
                name: "Allow once".to_string(),
            }],
        }),
    )
}

#[cfg(test)]
fn delegation_projection_for_regression() -> TuiProjectionState {
    let parent_run_id = Uuid::new_v4();
    let child_run_id = Uuid::new_v4();
    let mut session = Session::new("delegation transcript");
    let parent_turn = make_turn(parent_run_id, Role::Assistant, "Delegating child work.", 1);
    let child_turn = make_turn(
        child_run_id,
        Role::Assistant,
        "Child output remains visible after replay.",
        4,
    );
    let mut invocation = make_tool_invocation(
        parent_run_id,
        Some(parent_turn.id),
        "task",
        json!({"agent": "explore", "prompt": "Inspect delegated child output"}),
        2,
    );
    invocation.delegation = Some(fluent_code_app::session::model::TaskDelegationRecord {
        child_run_id: Some(child_run_id),
        agent_name: Some("explore".to_string()),
        prompt: Some("Inspect delegated child output".to_string()),
        status: fluent_code_app::session::model::TaskDelegationStatus::Completed,
    });

    session
        .turns
        .extend([parent_turn.clone(), child_turn.clone()]);
    session.tool_invocations.push(invocation.clone());
    session.upsert_run(
        parent_run_id,
        fluent_code_app::session::model::RunStatus::Completed,
    );
    session.upsert_run_with_parent(
        child_run_id,
        fluent_code_app::session::model::RunStatus::Completed,
        Some(parent_run_id),
        Some(invocation.id),
    );
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&parent_turn),
        TranscriptItemRecord::from_tool_invocation(&invocation),
        TranscriptItemRecord::delegated_child(&invocation, 3),
        TranscriptItemRecord::from_turn(&child_turn),
    ];

    projection_with_session("session-delegation", session, None, None, None)
}

#[cfg(test)]
fn legacy_approximate_projection_for_regression() -> TuiProjectionState {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("legacy transcript");
    session.transcript_fidelity = TranscriptFidelity::Approximate;
    session.turns.extend([
        make_turn(run_id, Role::User, "legacy prompt", 1),
        make_turn(run_id, Role::Assistant, "legacy answer", 2),
    ]);

    projection_with_session("session-legacy", session, None, None, None)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn assert_session_render_regression_completed_and_streaming() {
    let completed_frame =
        render_projection_frame_text_for_tests(&completed_projection_for_regression());
    let expected_completed = expected_projection_frame_text_for_tests(
        " fluent-code │ bootstrapping",
        &["› completed transcript · session"],
        &[
            "ACP subprocess: not started",
            "Session: session-completed",
            "Replay fidelity: exact",
            "Prompt status: completed",
            "",
            "you",
            "first prompt",
            "",
            "",
            "assistant",
            "first answer",
        ],
        "",
        "Type a prompt and press Enter. Ctrl-J/K switch sessions. Ctrl-N starts a new ACP session. q/Esc/Ctrl-C exits.",
    );
    assert_eq!(completed_frame, expected_completed);

    let streaming_frame =
        render_projection_frame_text_for_tests(&streaming_projection_for_regression());
    let expected_streaming = expected_projection_frame_text_for_tests(
        " fluent-code │ bootstrapping",
        &["› streaming transcript · session"],
        &[
            "ACP subprocess: not started",
            "Session: session-streaming",
            "Replay fidelity: exact",
            "Prompt status: running",
            "",
            "you",
            "follow-up",
            "",
            "",
            "assistant",
            "streamed answer in progress",
            "",
            "",
            "  ● running",
        ],
        "",
        "Prompt running through ACP. Esc/Ctrl-C cancels the active turn.",
    );
    assert_eq!(streaming_frame, expected_streaming);
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn assert_session_render_regression_permission_delegation_and_legacy() {
    let permission_frame =
        render_projection_frame_text_for_tests(&permission_projection_for_regression());
    let expected_permission = expected_projection_frame_text_for_tests(
        " fluent-code │ bootstrapping",
        &["› permission transcript · session"],
        &[
            "ACP subprocess: not started",
            "Session: session-permission",
            "Replay fidelity: exact",
            "Prompt status: awaiting approval",
            "",
            "Pending permission",
            "  read src/main.rs (call-2)",
            "  - Allow once [allow_once]",
            "",
            "you",
            "inspect src/main.rs",
            "",
            "",
            "  ⏵ read · pending / queued",
            "    read src/main.rs",
            "    action Enter/Y allow once • A always allow • N deny batch",
            "",
            "  ● awaiting approval",
        ],
        "",
        "Permission: Enter/y allow once, a allow always, n reject once, r reject always, q/Esc/Ctrl-C cancel.",
    );
    assert_eq!(permission_frame, expected_permission);

    let delegation_frame =
        render_projection_frame_text_for_tests(&delegation_projection_for_regression());
    let expected_delegation = expected_projection_frame_text_for_tests(
        " fluent-code │ bootstrapping",
        &["› delegation transcript · session"],
        &[
            "ACP subprocess: not started",
            "Session: session-delegation",
            "Replay fidelity: exact",
            "",
            "assistant",
            "Delegating child work.",
            "",
            "",
            "  ⏵ task explore · approved / completed",
            "    task explore · Inspect delegated child output",
            "",
            "assistant",
            "Child output remains visible after replay.",
        ],
        "",
        "Type a prompt and press Enter. Ctrl-J/K switch sessions. Ctrl-N starts a new ACP session. q/Esc/Ctrl-C exits.",
    );
    assert_eq!(delegation_frame, expected_delegation);

    let legacy_frame =
        render_projection_frame_text_for_tests(&legacy_approximate_projection_for_regression());
    let expected_legacy = expected_projection_frame_text_for_tests(
        " fluent-code │ bootstrapping",
        &["› legacy transcript · session"],
        &[
            "ACP subprocess: not started",
            "Session: session-legacy",
            "Replay fidelity: approximate",
            "",
            "you",
            "legacy prompt",
            "",
            "",
            "assistant",
            "legacy answer",
        ],
        "",
        "Type a prompt and press Enter. Ctrl-J/K switch sessions. Ctrl-N starts a new ACP session. q/Esc/Ctrl-C exits.",
    );
    assert_eq!(legacy_frame, expected_legacy);
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn assert_acp_default_render_shows_full_transcript_with_active_cell() {
    let mut projection = TuiProjectionState::default();

    projection.mark_session_created(acp::SessionId::new("session-1"));
    projection.mark_prompt_started();
    projection.apply_session_notification(tests::user_message_chunk("first prompt"));
    projection.apply_session_notification(tests::agent_message_chunk("first answer"));
    projection.mark_prompt_finished(acp::StopReason::EndTurn);

    projection.mark_prompt_started();
    projection.apply_session_notification(tests::user_message_chunk("second prompt"));
    projection.apply_session_notification(tests::agent_message_chunk("second answer"));
    projection.mark_prompt_finished(acp::StopReason::EndTurn);

    projection.mark_prompt_started();
    projection.apply_session_notification(tests::user_message_chunk("follow-up"));
    projection.apply_session_notification(tests::agent_thought_chunk("thinking through the reply"));
    projection.apply_session_notification(tests::agent_message_chunk(
        "# Partial third answer\n- streamed bullet",
    ));

    projection.ensure_conversation_cache_fresh();
    let rendered = projection_lines(&projection)
        .into_iter()
        .map(tests::line_to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(projection.prompt_in_flight);
    assert!(rendered.contains("Session: session-1"));
    assert!(rendered.contains("Replay fidelity: exact"));
    assert!(rendered.contains("Prompt status: running"));
    assert!(rendered.contains("first prompt"));
    assert!(rendered.contains("first answer"));
    assert!(rendered.contains("second prompt"));
    assert!(rendered.contains("second answer"));
    assert!(rendered.contains("follow-up"));
    assert!(rendered.contains("thinking through the reply"));
    assert!(rendered.contains("Partial third answer"));
    assert!(rendered.contains("streamed bullet"));
    assert!(rendered.contains("running"));
    assert!(!rendered.contains("Transcript rows:"));
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn assert_acp_default_render_preserves_markdown_scroll_and_fidelity_state() {
    let mut projection = TuiProjectionState::default();
    projection.mark_session_created(acp::SessionId::new("session-1"));

    let mut persisted_session = Session::new("session-1");
    persisted_session.transcript_fidelity = TranscriptFidelity::Approximate;
    projection.apply_loaded_session_metadata(&persisted_session);

    projection.mark_prompt_started();
    projection.apply_session_notification(tests::user_message_chunk("history"));
    projection.apply_session_notification(tests::agent_message_chunk(
        "# Heading\nUse **bold** and [docs](https://example.com).",
    ));
    projection.mark_prompt_finished(acp::StopReason::EndTurn);

    projection.mark_prompt_started();
    projection.apply_session_notification(tests::agent_message_chunk(
        "Committed line\nUse [docs](https://exam",
    ));
    projection.transcript_follow_tail = false;
    projection.transcript_scroll_top = 1;
    projection.ensure_conversation_cache_fresh();

    let lines = projection_lines(&projection);
    let rendered = lines
        .iter()
        .cloned()
        .map(tests::line_to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let manual_scroll = projection_body_scroll(&projection, &lines, Rect::new(0, 0, 20, 6));

    assert_eq!(
        projection.replay_fidelity,
        ReplayFidelityProjection::Approximate
    );
    assert_eq!(manual_scroll, 1);
    assert!(rendered.contains("Replay fidelity: approximate"));
    assert!(rendered.contains("Heading"));
    assert!(rendered.contains("docs (https://example.com)"));
    assert!(rendered.contains("Committed line Use [docs](https://exam"));
}

#[cfg(test)]
pub(crate) fn assert_render_contract_distinguishes_committed_history_from_active_cell() {
    assert_acp_default_render_shows_full_transcript_with_active_cell();
}

#[cfg(test)]
pub(crate) fn assert_conversation_projection_cache_preserves_history_output() {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("projection cache parity");
    let user_turn = make_turn(run_id, Role::User, "summarize the latest update", 1);
    let assistant_turn = make_turn(run_id, Role::Assistant, "Committed line\n\n# Heading", 2);
    let mut invocation = make_tool_invocation(
        run_id,
        Some(user_turn.id),
        "read",
        json!({"path": "src/lib.rs"}),
        3,
    );
    invocation.result = Some("ok".to_string());

    session
        .turns
        .extend([user_turn.clone(), assistant_turn.clone()]);
    session.tool_invocations.push(invocation.clone());
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::from_turn(&assistant_turn),
        TranscriptItemRecord::from_tool_invocation(&invocation),
        TranscriptItemRecord::assistant_text(
            run_id,
            Uuid::new_v4(),
            4,
            "streaming tail",
            TranscriptStreamState::Open,
        ),
    ];
    session.upsert_run(
        run_id,
        fluent_code_app::session::model::RunStatus::InProgress,
    );

    let projection = projection_with_session(
        "session-cache-preserve",
        session,
        Some(PromptStatusProjection::Running),
        Some(run_id),
        None,
    );

    assert_eq!(
        projection.conversation_entries(),
        uncached_conversation_entries(&projection)
    );
    assert_eq!(
        projection.transcript_rows(),
        uncached_transcript_rows(&projection)
    );
    assert_eq!(
        projection.tool_statuses(),
        uncached_tool_statuses(&projection)
    );
    assert_eq!(
        projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
        uncached_projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
    );
}

#[cfg(test)]
pub(crate) fn assert_conversation_projection_cache_invalidates_on_transcript_change() {
    let mut projection = TuiProjectionState::default();
    projection.ensure_conversation_cache_fresh();
    let initial_cache = Arc::clone(&projection.conversation_cache);

    projection.apply_session_notification(tests::agent_message_chunk("first chunk"));
    projection.ensure_conversation_cache_fresh();
    let after_first_chunk_cache = Arc::clone(&projection.conversation_cache);

    assert!(!Arc::ptr_eq(&initial_cache, &after_first_chunk_cache));
    assert_eq!(
        projection.conversation_entries(),
        uncached_conversation_entries(&projection)
    );
    assert_eq!(
        projection.transcript_rows(),
        uncached_transcript_rows(&projection)
    );
    assert_eq!(
        projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
        uncached_projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
    );

    projection.apply_session_notification(tests::agent_message_chunk(" plus more"));
    projection.ensure_conversation_cache_fresh();

    assert!(!Arc::ptr_eq(
        &after_first_chunk_cache,
        &projection.conversation_cache
    ));
    assert_eq!(
        projection.transcript_rows(),
        vec![TranscriptRowProjection {
            source: TranscriptSource::Agent,
            content: "first chunk plus more".to_string(),
        }]
    );
    assert_eq!(
        projection.transcript_rows(),
        uncached_transcript_rows(&projection)
    );
}

#[cfg(test)]
pub(crate) fn assert_startup_restore_with_projection_cache_matches_uncached_output() {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("restored session");
    let user_turn = make_turn(run_id, Role::User, "restore this", 1);
    session.turns.push(user_turn.clone());
    session.transcript_fidelity = TranscriptFidelity::Approximate;
    session.foreground_owner = Some(fluent_code_app::session::model::ForegroundOwnerRecord {
        run_id,
        phase: ForegroundPhase::Generating,
        batch_anchor_turn_id: None,
    });
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::assistant_text(
            run_id,
            Uuid::new_v4(),
            2,
            "restored answer",
            TranscriptStreamState::Open,
        ),
    ];
    session.upsert_run(
        run_id,
        fluent_code_app::session::model::RunStatus::InProgress,
    );

    let mut projection =
        projection_with_session("session-restored", session.clone(), None, None, None);
    projection.apply_loaded_session_metadata(&session);

    projection.ensure_conversation_cache_fresh();
    assert_eq!(
        projection.conversation_entries(),
        uncached_conversation_entries(&projection)
    );
    assert_eq!(
        projection.replay_fidelity,
        ReplayFidelityProjection::Approximate
    );
    assert_eq!(
        projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
        uncached_projection_lines(&projection)
            .into_iter()
            .map(tests::line_to_string)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        projection.transcript_rows(),
        uncached_transcript_rows(&projection)
    );
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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
        projection.ensure_conversation_cache_fresh();
        assert!(projection.transcript_rows().is_empty());

        projection.apply_session_notification(agent_message_chunk("hello"));
        projection.apply_session_notification(agent_message_chunk(""));
        projection.apply_session_notification(agent_thought_chunk(""));

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::User,
                    content: "follow-up".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "first answer".to_string(),
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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
        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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
        projection.ensure_conversation_cache_fresh();
        assert!(projection.transcript_rows().is_empty());

        projection.apply_session_notification(agent_message_chunk_for_session(
            "session-2",
            "fresh output",
        ));

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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
    fn load_session_metadata_from_response_overrides_local_fallback_values() {
        let mut projection = TuiProjectionState::default();
        let mut fallback_session = Session::new("session-1");
        fallback_session.transcript_fidelity = TranscriptFidelity::Approximate;
        fallback_session.foreground_owner =
            Some(fluent_code_app::session::model::ForegroundOwnerRecord {
                run_id: Uuid::new_v4(),
                phase: ForegroundPhase::RunningTool,
                batch_anchor_turn_id: None,
            });

        let mut metadata = loaded_session_metadata_from_response(&load_session_response_with_meta(
            Some("exact"),
            Some(Some("interrupted")),
        ));
        metadata.apply_fallback_session(&fallback_session);
        projection.apply_loaded_session_projection(metadata);

        assert_eq!(projection.replay_fidelity, ReplayFidelityProjection::Exact);
        assert_eq!(
            projection.prompt_status,
            Some(PromptStatusProjection::Interrupted)
        );
        assert!(!projection.prompt_in_flight);
    }

    #[test]
    fn session_browser_refresh_legacy_decode_errors_are_non_fatal() {
        assert!(session_browser_refresh_is_legacy_decode_error(
            &acp::Error::internal_error().data("failed to deserialize response")
        ));
        assert!(session_browser_refresh_is_legacy_decode_error(
            &acp::Error::internal_error().data(json!({
                "message": "missing field `cwd`"
            }))
        ));
        assert!(!session_browser_refresh_is_legacy_decode_error(
            &acp::Error::internal_error().data("connection closed")
        ));
        assert!(!session_browser_refresh_is_legacy_decode_error(
            &acp::Error::resource_not_found(None)
        ));
    }

    #[test]
    fn apply_session_notification_preserves_delegation_and_terminal_marker_metadata() {
        let mut projection = TuiProjectionState::default();
        projection.mark_session_created(acp::SessionId::new("session-1"));

        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let mut invocation = make_tool_invocation(
            root_run_id,
            None,
            "task",
            json!({"agent":"explore","prompt":"Inspect delegated child state"}),
            1,
        );
        invocation.tool_call_id = "task-call-1".to_string();
        invocation.delegation = Some(fluent_code_app::session::model::TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect delegated child state".to_string()),
            status: fluent_code_app::session::model::TaskDelegationStatus::Running,
        });

        projection.apply_session_notification(tool_call_with_metadata("session-1", &invocation));
        projection.apply_session_notification(session_info_with_transcript_item(
            "session-1",
            TranscriptItemRecord::delegated_child(&invocation, 2),
        ));
        projection.apply_session_notification(session_info_with_transcript_item(
            "session-1",
            TranscriptItemRecord::run_terminal(&fluent_code_app::session::model::RunRecord {
                id: root_run_id,
                status: fluent_code_app::session::model::RunStatus::Failed,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 1,
                terminal_sequence: Some(3),
                terminal_stop_reason: Some(
                    fluent_code_app::session::model::RunTerminalStopReason::Interrupted,
                ),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }),
        ));

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.tool_statuses(),
            vec![ToolStatusProjection {
                tool_call_id: "task-call-1".to_string(),
                title: "task explore".to_string(),
                status: "queued".to_string(),
            }]
        );
        assert_eq!(
            projection.transcript_session.tool_invocations[0]
                .delegation
                .as_ref()
                .and_then(|delegation| delegation.child_run_id),
            Some(child_run_id)
        );
        assert!(projection.history_cells().iter_rows().any(|row| {
            matches!(
                row,
                ConversationRow::RunMarker(marker) if marker.label == "interrupted"
            )
        }));
        assert!(projection.conversation_entries().iter().any(|entry| {
            matches!(
                &entry.kind,
                ConversationEntryKind::RunMarker(RunMarkerProjection { label }) if label == "interrupted"
            )
        }));
    }

    #[test]
    fn conversation_entries_preserve_interleaved_history_row_order() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("interleaved conversation");
        let user_turn = make_turn(run_id, Role::User, "first prompt", 1);
        let assistant_turn = make_turn(run_id, Role::Assistant, "answer after tool", 3);
        let mut read_tool = make_tool_invocation(
            run_id,
            Some(user_turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            2,
        );
        read_tool.tool_call_id = "tool-1".to_string();
        let mut write_tool = make_tool_invocation(
            run_id,
            Some(assistant_turn.id),
            "write",
            json!({"path": "src/lib.rs"}),
            4,
        );
        write_tool.tool_call_id = "tool-2".to_string();

        session
            .turns
            .extend([user_turn.clone(), assistant_turn.clone()]);
        session
            .tool_invocations
            .extend([read_tool.clone(), write_tool.clone()]);
        session.transcript_items = vec![
            TranscriptItemRecord::from_turn(&user_turn),
            TranscriptItemRecord::from_tool_invocation(&read_tool),
            TranscriptItemRecord::from_turn(&assistant_turn),
            TranscriptItemRecord::from_tool_invocation(&write_tool),
            TranscriptItemRecord::assistant_reasoning(
                run_id,
                Uuid::new_v4(),
                5,
                "wrap up",
                TranscriptStreamState::Committed,
            ),
            TranscriptItemRecord::run_terminal(&fluent_code_app::session::model::RunRecord {
                id: run_id,
                status: fluent_code_app::session::model::RunStatus::Completed,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 1,
                terminal_sequence: Some(6),
                terminal_stop_reason: Some(
                    fluent_code_app::session::model::RunTerminalStopReason::Completed,
                ),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }),
        ];
        session.upsert_run(
            run_id,
            fluent_code_app::session::model::RunStatus::Completed,
        );

        let projection = projection_with_session("session-interleaved", session, None, None, None);
        let entries = projection.conversation_entries();

        assert_eq!(entries.len(), 6);
        assert!(matches!(
            &entries[0].kind,
            ConversationEntryKind::Turn(ConversationTurnEntryProjection {
                role: Role::User,
                content,
                is_streaming: false,
            }) if content == "first prompt"
        ));
        assert!(matches!(
            &entries[1].kind,
            ConversationEntryKind::Tool(ToolStatusProjection {
                tool_call_id,
                title,
                status,
            }) if tool_call_id == "tool-1" && title == "read" && status == "completed"
        ));
        assert!(matches!(
            &entries[2].kind,
            ConversationEntryKind::Turn(ConversationTurnEntryProjection {
                role: Role::Assistant,
                content,
                is_streaming: false,
            }) if content == "answer after tool"
        ));
        assert!(matches!(
            &entries[3].kind,
            ConversationEntryKind::Tool(ToolStatusProjection {
                tool_call_id,
                title,
                status,
            }) if tool_call_id == "tool-2" && title == "write" && status == "completed"
        ));
        assert!(matches!(
            &entries[4].kind,
            ConversationEntryKind::Reasoning(ConversationReasoningEntryProjection {
                content,
                is_streaming: false,
            }) if content == "wrap up"
        ));
        assert!(matches!(
            &entries[5].kind,
            ConversationEntryKind::RunMarker(RunMarkerProjection { label }) if label == "completed"
        ));

        assert_eq!(
            projection.transcript_rows(),
            vec![
                TranscriptRowProjection {
                    source: TranscriptSource::User,
                    content: "first prompt".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Agent,
                    content: "answer after tool".to_string(),
                },
                TranscriptRowProjection {
                    source: TranscriptSource::Thought,
                    content: "wrap up".to_string(),
                },
            ]
        );
        assert_eq!(
            projection.tool_statuses(),
            vec![
                ToolStatusProjection {
                    tool_call_id: "tool-1".to_string(),
                    title: "read".to_string(),
                    status: "completed".to_string(),
                },
                ToolStatusProjection {
                    tool_call_id: "tool-2".to_string(),
                    title: "write".to_string(),
                    status: "completed".to_string(),
                },
            ]
        );
    }

    #[test]
    fn conversation_entries_preserve_existing_grouped_tool_batches() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("grouped tools");
        let user_turn = make_turn(run_id, Role::User, "inspect files", 1);
        let mut read_a = make_tool_invocation(
            run_id,
            Some(user_turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            2,
        );
        read_a.tool_call_id = "tool-1".to_string();
        let mut read_b = make_tool_invocation(
            run_id,
            Some(user_turn.id),
            "read",
            json!({"path": "src/lib.rs"}),
            3,
        );
        read_b.tool_call_id = "tool-2".to_string();

        session.turns.push(user_turn.clone());
        session
            .tool_invocations
            .extend([read_a.clone(), read_b.clone()]);
        session.transcript_items = vec![
            TranscriptItemRecord::from_turn(&user_turn),
            TranscriptItemRecord::from_tool_invocation(&read_a),
            TranscriptItemRecord::from_tool_invocation(&read_b),
        ];

        let projection = projection_with_session("session-grouped", session, None, None, None);
        let entries = projection.conversation_entries();

        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0].kind,
            ConversationEntryKind::Turn(ConversationTurnEntryProjection {
                role: Role::User,
                ..
            })
        ));
        assert!(matches!(
            &entries[1].kind,
            ConversationEntryKind::ToolGroup(ToolGroupEntryProjection { items })
                if items.iter().map(|item| item.tool_call_id.as_str()).collect::<Vec<_>>()
                    == vec!["tool-1", "tool-2"]
        ));
        assert_eq!(
            projection.tool_statuses(),
            vec![
                ToolStatusProjection {
                    tool_call_id: "tool-1".to_string(),
                    title: "read".to_string(),
                    status: "completed".to_string(),
                },
                ToolStatusProjection {
                    tool_call_id: "tool-2".to_string(),
                    title: "read".to_string(),
                    status: "completed".to_string(),
                },
            ]
        );
    }

    #[test]
    fn apply_session_notification_does_not_merge_across_prompt_turn_boundaries() {
        let mut projection = TuiProjectionState::default();

        projection.mark_prompt_started();
        projection.apply_session_notification(agent_message_chunk("first turn"));
        projection.mark_prompt_finished(acp::StopReason::EndTurn);

        projection.mark_prompt_started();
        projection.apply_session_notification(agent_message_chunk("second turn"));

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
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

    #[tokio::test]
    async fn projection_loop_batches_notifications_without_starving_input() {
        assert_projection_loop_batches_notifications_without_starving_input()
            .await
            .expect("projection iteration to batch burst notifications without starving input");
    }

    #[tokio::test]
    async fn projection_wake_does_not_miss_activity_with_release_acquire_ordering() {
        assert_projection_wake_does_not_miss_activity_with_release_acquire_ordering()
            .await
            .expect(
                "stale activity should still be observed after the stored wake permit is consumed",
            );
    }

    #[tokio::test]
    async fn projection_wait_for_activity_still_blocks_without_new_sequence() {
        assert_projection_wait_for_activity_still_blocks_without_new_sequence()
            .await
            .expect("wait_for_activity should remain blocked until a newer sequence is published");
    }

    #[tokio::test]
    async fn projection_loop_flushes_terminal_update_before_queued_quit_under_burst_activity() {
        assert_projection_loop_flushes_terminal_update_before_queued_quit_under_burst_activity()
            .await
            .expect("projection iteration to flush the final prompt snapshot before queued quit under burst activity");
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

        projection.ensure_conversation_cache_fresh();
        assert_eq!(
            projection.transcript_rows(),
            vec![TranscriptRowProjection {
                source: TranscriptSource::Agent,
                content: "<image>".to_string(),
            }]
        );
    }

    #[test]
    fn render_contract_distinguishes_committed_history_from_active_cell() {
        super::assert_render_contract_distinguishes_committed_history_from_active_cell();
    }

    #[test]
    fn conversation_projection_cache_preserves_history_output() {
        super::assert_conversation_projection_cache_preserves_history_output();
    }

    #[test]
    fn conversation_projection_cache_invalidates_on_transcript_change() {
        super::assert_conversation_projection_cache_invalidates_on_transcript_change();
    }

    #[test]
    fn startup_restore_with_projection_cache_matches_uncached_output() {
        super::assert_startup_restore_with_projection_cache_matches_uncached_output();
    }

    #[test]
    fn session_browser_lines_preserve_backend_order_and_mark_current_session() {
        let mut projection = TuiProjectionState::default();
        projection.session.session_id = Some("session-current".to_string());
        projection.apply_session_list(vec![
            SessionBrowserEntryProjection {
                session_id: "session-current".to_string(),
                title: Some("Newest session".to_string()),
                updated_at: Some("2026-04-05T12:00:00Z".to_string()),
            },
            SessionBrowserEntryProjection {
                session_id: "session-middle".to_string(),
                title: Some("Middle session".to_string()),
                updated_at: Some("2026-04-05T11:00:00Z".to_string()),
            },
            SessionBrowserEntryProjection {
                session_id: "session-oldest".to_string(),
                title: Some("Oldest session".to_string()),
                updated_at: Some("2026-04-05T10:00:00Z".to_string()),
            },
        ]);

        let rendered = session_browser_lines(&projection)
            .into_iter()
            .map(line_to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "› Newest session · session".to_string(),
                "  Middle session · session".to_string(),
                "  Oldest session · session".to_string(),
            ]
        );
    }

    #[test]
    fn projection_key_action_maps_ctrl_j_and_ctrl_k_to_session_switching() {
        let projection = TuiProjectionState::default();

        assert_eq!(
            projection_key_action(&projection, KeyCode::Char('k'), KeyModifiers::CONTROL),
            ProjectionAction::PreviousSession
        );
        assert_eq!(
            projection_key_action(&projection, KeyCode::Char('j'), KeyModifiers::CONTROL),
            ProjectionAction::NextSession
        );
    }

    pub(super) fn agent_message_chunk(text: &str) -> acp::SessionNotification {
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

    pub(super) fn agent_thought_chunk(text: &str) -> acp::SessionNotification {
        session_notification(
            "session-1",
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        )
    }

    pub(super) fn user_message_chunk(text: &str) -> acp::SessionNotification {
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

    fn tool_call_with_metadata(
        session_id: impl Into<acp::SessionId>,
        invocation: &ToolInvocationRecord,
    ) -> acp::SessionNotification {
        let mut meta = acp::Meta::new();
        meta.insert(
            ACP_META_TOOL_INVOCATION_KEY.to_string(),
            serde_json::to_value(invocation).expect("tool invocation metadata should serialize"),
        );
        session_notification(
            session_id,
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    invocation.tool_call_id.clone(),
                    invocation.tool_name.clone(),
                )
                .meta(meta),
            ),
        )
    }

    fn session_info_with_transcript_item(
        session_id: impl Into<acp::SessionId>,
        item: TranscriptItemRecord,
    ) -> acp::SessionNotification {
        let mut meta = acp::Meta::new();
        meta.insert(
            ACP_META_TRANSCRIPT_ITEM_KEY.to_string(),
            serde_json::to_value(item).expect("transcript item metadata should serialize"),
        );
        session_notification(
            session_id,
            acp::SessionUpdate::SessionInfoUpdate(acp::SessionInfoUpdate::new().meta(meta)),
        )
    }

    fn load_session_response_with_meta(
        replay_fidelity: Option<&str>,
        prompt_state: Option<Option<&str>>,
    ) -> acp::LoadSessionResponse {
        let mut meta = acp::Meta::new();
        if let Some(replay_fidelity) = replay_fidelity {
            meta.insert(
                ACP_META_REPLAY_FIDELITY_KEY.to_string(),
                Value::String(replay_fidelity.to_string()),
            );
        }
        if let Some(prompt_state) = prompt_state {
            meta.insert(
                ACP_META_LATEST_PROMPT_STATE_KEY.to_string(),
                prompt_state.map_or(Value::Null, |value| Value::String(value.to_string())),
            );
        }
        acp::LoadSessionResponse::new().meta(meta)
    }

    fn session_notification(
        session_id: impl Into<acp::SessionId>,
        update: acp::SessionUpdate,
    ) -> acp::SessionNotification {
        acp::SessionNotification::new(session_id, update)
    }

    pub(super) fn line_to_string(line: Line<'static>) -> String {
        line.spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>()
    }
}
