use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use fluent_code_app::agent::AgentRegistry;
use fluent_code_app::app::permissions::{PermissionReply, can_remember_reply, remember_reply};
use fluent_code_app::app::{AppState, AppStatus, Msg};
use fluent_code_app::bootstrap::{AppBootstrap, BootstrapContext};
use fluent_code_app::config::Config;
use fluent_code_app::host::SharedAppHost;
use fluent_code_app::logging::{config_source_for_log, path_for_log};
use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry};
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::{
    ForegroundPhase, ReplaySequence, RunId, RunTerminalStopReason, Session, SessionId,
    TaskDelegationStatus, ToolApprovalState, ToolExecutionState, TranscriptItemId,
};
use fluent_code_app::session::store::FsSessionStore;
use fluent_code_app::{FluentCodeError, Result};
use futures::io::{AsyncRead, AsyncWrite};
#[cfg(test)]
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::{RawValue, to_raw_value};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::info;

#[cfg(test)]
use tracing::debug;
use uuid::Uuid;

use crate::mapping::{
    ProjectionEventPhase, PromptTurnEvent, PromptTurnProjection, SessionUpdateMapper,
    TerminalStopProjection,
};
use crate::protocol::{
    AgentMessageChunk, AgentThoughtChunk, AuthenticateRequest, AuthenticateResponse,
    CloseSessionRequest, CloseSessionResponse, ContentBlock, InitializeRequest, InitializeResponse,
    JsonRpcErrorResponse, JsonRpcNotification, JsonRpcProtocol, JsonRpcResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, Method,
    NewSessionRequest, NewSessionResponse, ParsedRequest, PromptTurnState, ProtocolError,
    ProtocolState, ReplayFidelity, ResumeSessionRequest, ResumeSessionResponse, ServerInfo,
    SessionCancelRequest, SessionInfoEntry, SessionNotification, SessionPromptRequest,
    SessionPromptResponse, SessionUpdate, ToolCallUpdate, UserMessageChunk,
};
use crate::transport::StdioTransport;

const JSONRPC_PARSE_ERROR: i32 = -32700;
const JSONRPC_INVALID_REQUEST: i32 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;
const JSONRPC_INVALID_PARAMS: i32 = -32602;
const JSONRPC_INTERNAL_ERROR: i32 = -32603;
const ACP_AUTH_REQUIRED: i32 = -32000;
const ACP_RESOURCE_NOT_FOUND: i32 = -32002;
const ACP_ACTIVE_PROMPT: i32 = -32003;
const ACP_TEST_PROBES_ENV_VAR: &str = "FLUENT_CODE_ACP_ENABLE_TEST_PROBES";
const ACP_META_LATEST_PROMPT_STATE_KEY: &str = "fluentCodeLatestPromptState";
const ACP_META_REPLAY_FIDELITY_KEY: &str = "fluentCodeReplayFidelity";
const SESSION_UPDATE_METHOD: &str = "session/update";
const SESSION_REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const CANCELLED_TOOL_MESSAGE: &str =
    "Tool execution was cancelled because the prompt turn was cancelled.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<McpTransportConfig>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
    },
    Sse {
        url: String,
    },
}

fn parse_mcp_servers(raw_servers: &[Value]) -> Vec<McpServerConfig> {
    raw_servers
        .iter()
        .filter_map(|value| serde_json::from_value::<McpServerConfig>(value.clone()).ok())
        .collect()
}

pub async fn run() -> Result<()> {
    let (bootstrap, _logging) = AppBootstrap::load()?.into_parts();
    let config = &bootstrap.config;

    info!(
        config_source = %config_source_for_log(config.config_path.as_deref()),
        data_dir = %path_for_log(&config.data_dir),
        provider = %config.model.provider,
        model = %config.model.model,
        file_logging = config.logging.file.enabled,
        file_log_path = %path_for_log(&config.logging.file.path),
        file_log_level = %config.logging.file.level,
        stderr_logging = config.logging.stderr.enabled,
        stderr_log_level = %config.logging.stderr.level,
        "application startup configuration loaded"
    );

    AcpServer::from_dependencies(AcpServerDependencies::from_bootstrap(bootstrap))
        .run()
        .await
}

type ManagedAppHost = SharedAppHost;

#[derive(Clone)]
pub struct AcpServerDependencies {
    pub config: Config,
    pub store: FsSessionStore,
    pub agent_registry: Arc<AgentRegistry>,
    pub runtime: Runtime,
    pub tool_registry: Arc<ToolRegistry>,
    pub plugin_load_snapshot: PluginLoadSnapshot,
}

impl AcpServerDependencies {
    pub fn from_bootstrap(bootstrap: BootstrapContext) -> Self {
        let BootstrapContext {
            config,
            store,
            agent_registry,
            runtime,
            tool_registry,
            plugin_load_snapshot,
        } = bootstrap;

        Self {
            config,
            store,
            agent_registry,
            runtime,
            tool_registry,
            plugin_load_snapshot,
        }
    }

    pub fn from_config(config: Config) -> Result<Self> {
        Ok(Self::from_bootstrap(BootstrapContext::from_config(config)?))
    }
}

#[derive(Clone)]
pub struct AcpServer {
    dependencies: AcpServerDependencies,
    transport: StdioTransport,
    mapper: SessionUpdateMapper,
}

struct AcpConnectionState {
    protocol: JsonRpcProtocol,
    authenticated: bool,
    sessions: HashMap<SessionId, ManagedSessionHandle>,
    next_request_id: u64,
}

#[allow(dead_code)]
struct ManagedSession {
    cwd: PathBuf,
    mcp_servers: Vec<McpServerConfig>,
    host: ManagedAppHost,
    live_prompt_turn: Option<LivePromptTurnState>,
    pending_prompt_request_id: Option<u64>,
    buffered_prompt_completion: Option<BufferedPromptCompletion>,
}

struct LivePromptTurnState {
    root_run_id: RunId,
    emission_state: PromptTurnEmissionState,
}

enum BufferedPromptCompletion {
    Response(acp::PromptResponse),
    Error { code: i32, message: String },
}

#[cfg(test)]
pub(crate) enum ReaderEvent {
    Frame(String),
    Eof,
}

#[derive(Clone, Copy)]
struct CancelTarget {
    root_run_id: RunId,
    foreground_phase: ForegroundPhase,
    batch_anchor_turn_id: Option<uuid::Uuid>,
    active_run_is_child: bool,
}

#[derive(Debug)]
struct RpcResponseError {
    code: i32,
    message: String,
}

impl RpcResponseError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: JSONRPC_INVALID_PARAMS,
            message: message.into(),
        }
    }

    fn auth_required(message: impl Into<String>) -> Self {
        Self {
            code: ACP_AUTH_REQUIRED,
            message: message.into(),
        }
    }

    fn resource_not_found(message: impl Into<String>) -> Self {
        Self {
            code: ACP_RESOURCE_NOT_FOUND,
            message: message.into(),
        }
    }

    fn active_prompt(message: impl Into<String>) -> Self {
        Self {
            code: ACP_ACTIVE_PROMPT,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: JSONRPC_INTERNAL_ERROR,
            message: message.into(),
        }
    }
}

#[derive(Debug, Default)]
struct PromptTurnEmissionState {
    watermark: Option<ReplaySequence>,
    open_transcript_item_ids: HashSet<TranscriptItemId>,
    emitted_text_lengths: HashMap<(u64, u8), usize>,
    emitted_tool_calls: HashSet<String>,
    emitted_permission_requests: HashSet<String>,
    latest_tool_updates: HashMap<String, ToolCallUpdate>,
}

struct PromptTurnResult {
    frames: Vec<OutboundFrame>,
    stop_reason: SessionPromptResponse,
}

struct LivePromptPollResult {
    frames: Vec<OutboundFrame>,
    prompt_turn_complete: bool,
}

#[derive(Clone, Debug)]
struct OutboundFrame {
    json: Box<str>,
}

impl OutboundFrame {
    fn new(json: String) -> std::result::Result<Self, RpcResponseError> {
        if json.contains(['\n', '\r']) {
            return Err(RpcResponseError::internal(
                "serialized JSON-RPC frame must remain single-line",
            ));
        }

        Ok(Self {
            json: json.into_boxed_str(),
        })
    }

    fn into_value(self) -> std::result::Result<Value, RpcResponseError> {
        serde_json::from_str(&self.json)
            .map_err(|error| RpcResponseError::internal(error.to_string()))
    }

    fn to_value(&self) -> std::result::Result<Value, RpcResponseError> {
        serde_json::from_str(&self.json)
            .map_err(|error| RpcResponseError::internal(error.to_string()))
    }
}

type ManagedSessionHandle = Arc<Mutex<ManagedSession>>;

struct OfficialAgentConnection {
    server: AcpServer,
    state: Mutex<AcpConnectionState>,
}

#[derive(Clone)]
struct OfficialAgentRuntime {
    connection: Arc<OfficialAgentConnection>,
    outbound_sender: mpsc::UnboundedSender<OutboundClientCall>,
    test_probes_enabled: bool,
}

tokio::task_local! {
    static OFFICIAL_AGENT_RUNTIME: OfficialAgentRuntime;
}

#[derive(Clone)]
struct OfficialAgentAdapter {
    connection: Arc<OfficialAgentConnection>,
    outbound_sender: mpsc::UnboundedSender<OutboundClientCall>,
}

struct OfficialAgentTestProbeAdapter {
    adapter: OfficialAgentAdapter,
}

enum OutboundClientCall {
    SessionNotification {
        request: acp::SessionNotification,
        ack: oneshot::Sender<acp::Result<()>>,
    },
    RequestPermission {
        request: acp::RequestPermissionRequest,
        ack: oneshot::Sender<acp::Result<acp::RequestPermissionResponse>>,
    },
    ReadTextFile {
        request: acp::ReadTextFileRequest,
        ack: oneshot::Sender<acp::Result<acp::ReadTextFileResponse>>,
    },
    WriteTextFile {
        request: acp::WriteTextFileRequest,
        ack: oneshot::Sender<acp::Result<acp::WriteTextFileResponse>>,
    },
    CreateTerminal {
        request: acp::CreateTerminalRequest,
        ack: oneshot::Sender<acp::Result<acp::CreateTerminalResponse>>,
    },
    WaitForTerminalExit {
        request: acp::WaitForTerminalExitRequest,
        ack: oneshot::Sender<acp::Result<acp::WaitForTerminalExitResponse>>,
    },
    TerminalOutput {
        request: acp::TerminalOutputRequest,
        ack: oneshot::Sender<acp::Result<acp::TerminalOutputResponse>>,
    },
    ReleaseTerminal {
        request: acp::ReleaseTerminalRequest,
        ack: oneshot::Sender<acp::Result<acp::ReleaseTerminalResponse>>,
    },
}

enum FrameRouteResult {
    Dispatched,
    PermissionFollowUp,
    NotRouted,
}

#[derive(Debug, Deserialize)]
struct FilesystemWriteProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: PathBuf,
    content: String,
}

#[derive(Debug, Deserialize)]
struct FilesystemReadProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: PathBuf,
    line: Option<u32>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct FilesystemReadProbeResponse {
    content: String,
}

#[derive(Debug, Deserialize)]
struct TerminalCommandProbeRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(rename = "outputByteLimit")]
    output_byte_limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct TerminalCommandProbeResponse {
    output: String,
    truncated: bool,
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
}

macro_rules! send_outbound_call {
    ($self:expr, $variant:ident, $request:expr) => {{
        let (ack_sender, ack_receiver) = oneshot::channel();
        $self
            .outbound_sender
            .send(OutboundClientCall::$variant {
                request: $request,
                ack: ack_sender,
            })
            .map_err(|_| acp::Error::internal_error())?;
        ack_receiver
            .await
            .map_err(|_| acp::Error::internal_error())?
    }};
}

impl OfficialAgentAdapter {
    fn for_probe_connection(
        server: AcpServer,
        outbound_sender: mpsc::UnboundedSender<OutboundClientCall>,
    ) -> Self {
        Self {
            connection: Arc::new(OfficialAgentConnection::new(server)),
            outbound_sender,
        }
    }

    fn from_runtime(runtime: &OfficialAgentRuntime) -> Self {
        Self {
            connection: Arc::clone(&runtime.connection),
            outbound_sender: runtime.outbound_sender.clone(),
        }
    }

    async fn delegate_request<TParams, TResponse>(
        &self,
        method: Method,
        params: TParams,
    ) -> acp::Result<TResponse>
    where
        TParams: Serialize,
        TResponse: serde::de::DeserializeOwned,
    {
        let outbound_frames = self.connection.delegate_request(method, params).await?;
        self.process_outbound_frames(outbound_frames).await
    }

    async fn try_route_notification_frame(
        &self,
        frame: OutboundFrame,
        outbound_frames: &mut VecDeque<OutboundFrame>,
    ) -> acp::Result<(FrameRouteResult, Option<Value>)> {
        let frame = frame.into_value().map_err(map_rpc_response_error)?;
        let mut frame_object = match frame {
            Value::Object(frame_object) => frame_object,
            frame => return Ok((FrameRouteResult::NotRouted, Some(frame))),
        };

        if frame_object.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE_METHOD) {
            let notification = serde_json::from_value::<acp::SessionNotification>(
                frame_object
                    .remove("params")
                    .ok_or_else(acp::Error::internal_error)?,
            )
            .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
            self.send_session_notification(notification).await?;
            return Ok((FrameRouteResult::Dispatched, None));
        }

        if frame_object.get("method").and_then(Value::as_str)
            == Some(SESSION_REQUEST_PERMISSION_METHOD)
        {
            let request = serde_json::from_value::<acp::RequestPermissionRequest>(
                frame_object
                    .remove("params")
                    .ok_or_else(acp::Error::internal_error)?,
            )
            .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
            let client_response = self.request_permission(request.clone()).await?;
            let follow_up_frames = self
                .process_permission_response(&request, client_response)
                .await?;
            prepend_outbound_frames(outbound_frames, follow_up_frames);
            return Ok((FrameRouteResult::PermissionFollowUp, None));
        }

        Ok((
            FrameRouteResult::NotRouted,
            Some(Value::Object(frame_object)),
        ))
    }

    async fn process_outbound_frames<TResponse>(
        &self,
        outbound_frames: Vec<OutboundFrame>,
    ) -> acp::Result<TResponse>
    where
        TResponse: serde::de::DeserializeOwned,
    {
        let mut response = None;
        let mut outbound_frames = VecDeque::from(outbound_frames);

        while let Some(outbound_frame) = outbound_frames.pop_front() {
            let (frame_route_result, routed_frame) = self
                .try_route_notification_frame(outbound_frame, &mut outbound_frames)
                .await?;
            match frame_route_result {
                FrameRouteResult::Dispatched | FrameRouteResult::PermissionFollowUp => continue,
                FrameRouteResult::NotRouted => {}
            }

            let outbound_frame = routed_frame.ok_or_else(acp::Error::internal_error)?;

            if let Some(error) = outbound_frame.get("error") {
                let code = error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or_else(acp::Error::internal_error)? as i32;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(acp::Error::internal_error)?;
                return Err(acp::Error::new(code, message));
            }

            if let Some(result) = outbound_frame.get("result") {
                response = Some(
                    serde_json::from_value::<TResponse>(result.clone()).map_err(|error| {
                        acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string())
                    })?,
                );
            }
        }

        response.ok_or_else(|| {
            acp::Error::new(
                JSONRPC_INTERNAL_ERROR,
                "ACP method did not produce a response result",
            )
        })
    }

    async fn send_session_notification(
        &self,
        notification: acp::SessionNotification,
    ) -> acp::Result<()> {
        send_outbound_call!(self, SessionNotification, notification)
    }

    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        send_outbound_call!(self, RequestPermission, request)
    }

    async fn read_text_file(
        &self,
        request: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        send_outbound_call!(self, ReadTextFile, request)
    }

    async fn write_text_file(
        &self,
        request: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        send_outbound_call!(self, WriteTextFile, request)
    }

    async fn create_terminal(
        &self,
        request: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        send_outbound_call!(self, CreateTerminal, request)
    }

    async fn wait_for_terminal_exit(
        &self,
        request: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        send_outbound_call!(self, WaitForTerminalExit, request)
    }

    async fn terminal_output(
        &self,
        request: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        send_outbound_call!(self, TerminalOutput, request)
    }

    async fn release_terminal(
        &self,
        request: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        send_outbound_call!(self, ReleaseTerminal, request)
    }

    async fn handle_ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        match args.method.as_ref() {
            "fluent_code/test/write_text_file" => {
                let request =
                    serde_json::from_str::<FilesystemWriteProbeRequest>(args.params.get())
                        .map_err(|error| {
                            acp::Error::invalid_params().data(serde_json::json!({
                        "message": format!("invalid ACP test filesystem write request: {error}")
                    }))
                        })?;
                self.write_text_file(acp::WriteTextFileRequest::new(
                    request.session_id,
                    request.path,
                    request.content,
                ))
                .await?;
                Ok(acp::ExtResponse::new(RawValue::NULL.to_owned().into()))
            }
            "fluent_code/test/read_text_file" => {
                let request = serde_json::from_str::<FilesystemReadProbeRequest>(args.params.get())
                    .map_err(|error| {
                        acp::Error::invalid_params().data(serde_json::json!({
                            "message": format!("invalid ACP test filesystem read request: {error}")
                        }))
                    })?;
                let response = self
                    .read_text_file(
                        acp::ReadTextFileRequest::new(request.session_id, request.path)
                            .line(request.line)
                            .limit(request.limit),
                    )
                    .await?;
                let result = to_raw_value(&FilesystemReadProbeResponse {
                    content: response.content,
                })
                .map_err(|error| acp::Error::internal_error().data(serde_json::json!({
                    "message": format!("failed to encode ACP test filesystem read response: {error}")
                })))?;
                Ok(acp::ExtResponse::new(result.into()))
            }
            "fluent_code/test/run_terminal_command" => {
                let request =
                    serde_json::from_str::<TerminalCommandProbeRequest>(args.params.get())
                        .map_err(|error| {
                            acp::Error::invalid_params().data(serde_json::json!({
                                "message": format!("invalid ACP test terminal request: {error}")
                            }))
                        })?;
                let created = self
                    .create_terminal(
                        acp::CreateTerminalRequest::new(
                            request.session_id.clone(),
                            request.command,
                        )
                        .args(request.args)
                        .cwd(request.cwd)
                        .output_byte_limit(request.output_byte_limit.map(u64::from)),
                    )
                    .await?;
                let exit = self
                    .wait_for_terminal_exit(acp::WaitForTerminalExitRequest::new(
                        request.session_id.clone(),
                        created.terminal_id.clone(),
                    ))
                    .await?;
                let output = self
                    .terminal_output(acp::TerminalOutputRequest::new(
                        request.session_id.clone(),
                        created.terminal_id.clone(),
                    ))
                    .await?;
                self.release_terminal(acp::ReleaseTerminalRequest::new(
                    request.session_id,
                    created.terminal_id,
                ))
                .await?;
                let result = to_raw_value(&TerminalCommandProbeResponse {
                    output: output.output,
                    truncated: output.truncated,
                    exit_code: exit.exit_status.exit_code.map(|code| code as i32),
                })
                .map_err(|error| {
                    acp::Error::internal_error().data(serde_json::json!({
                        "message": format!("failed to encode ACP test terminal response: {error}")
                    }))
                })?;
                Ok(acp::ExtResponse::new(result.into()))
            }
            _ => Ok(acp::ExtResponse::new(RawValue::NULL.to_owned().into())),
        }
    }

    async fn process_permission_response(
        &self,
        request: &acp::RequestPermissionRequest,
        response: acp::RequestPermissionResponse,
    ) -> acp::Result<Vec<OutboundFrame>> {
        self.connection
            .process_permission_response(request, response)
            .await
    }

    async fn handle_live_prompt_request(
        &self,
        args: acp::PromptRequest,
    ) -> acp::Result<acp::PromptResponse> {
        let (session_id, prompt_request_id, managed_session, outbound_frames) =
            self.connection.start_live_prompt_request(args).await?;

        if let Some(response) = self
            .process_prompt_outbound_frames(prompt_request_id, outbound_frames)
            .await?
        {
            return Ok(response);
        }

        loop {
            let woke_for_runtime_activity = {
                let mut managed_session = managed_session.lock().await;
                if let Some(buffered_completion) = managed_session.buffered_prompt_completion.take()
                {
                    return buffered_completion.into_result();
                }
                if managed_session.live_prompt_turn.is_none() {
                    if let Some(prompt_completion) =
                        prompt_completion_from_terminal_state(managed_session.host.state())
                    {
                        return prompt_completion;
                    }

                    return Err(acp::Error::new(
                        JSONRPC_INTERNAL_ERROR,
                        format!(
                            "session `{session_id}` completed the live prompt turn without producing a prompt response"
                        ),
                    ));
                }

                managed_session.host.wait_for_runtime_activity().await
            };

            let poll_result = {
                let mut managed_session = managed_session.lock().await;
                if let Some(buffered_completion) = managed_session.buffered_prompt_completion.take()
                {
                    return buffered_completion.into_result();
                }
                if managed_session.live_prompt_turn.is_none() {
                    if let Some(prompt_completion) =
                        prompt_completion_from_terminal_state(managed_session.host.state())
                    {
                        return prompt_completion;
                    }

                    return Err(acp::Error::new(
                        JSONRPC_INTERNAL_ERROR,
                        format!(
                            "session `{session_id}` completed the live prompt turn without producing a prompt response"
                        ),
                    ));
                }

                self.connection
                    .poll_live_prompt_turn(&session_id, &mut managed_session)
                    .await?
            };

            if let Some(response) = self
                .process_prompt_outbound_frames(prompt_request_id, poll_result.frames)
                .await?
            {
                return Ok(response);
            }

            if poll_result.prompt_turn_complete {
                return Err(acp::Error::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!(
                        "session `{session_id}` completed the live prompt turn without producing a prompt response"
                    ),
                ));
            }

            if !woke_for_runtime_activity {
                return Err(acp::Error::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!(
                        "session `{session_id}` stopped receiving runtime activity before producing a prompt response"
                    ),
                ));
            }
        }
    }

    async fn process_prompt_outbound_frames(
        &self,
        prompt_request_id: u64,
        outbound_frames: Vec<OutboundFrame>,
    ) -> acp::Result<Option<acp::PromptResponse>> {
        let mut prompt_response = None;
        let mut outbound_frames = VecDeque::from(outbound_frames);

        while let Some(outbound_frame) = outbound_frames.pop_front() {
            let (frame_route_result, routed_frame) = self
                .try_route_notification_frame(outbound_frame, &mut outbound_frames)
                .await?;
            match frame_route_result {
                FrameRouteResult::Dispatched | FrameRouteResult::PermissionFollowUp => continue,
                FrameRouteResult::NotRouted => {}
            }

            let outbound_frame = routed_frame.ok_or_else(acp::Error::internal_error)?;

            if let Some(error) = outbound_frame.get("error") {
                let frame_id = outbound_frame
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(acp::Error::internal_error)?;
                let code = error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or_else(acp::Error::internal_error)? as i32;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(acp::Error::internal_error)?;
                if frame_id == prompt_request_id {
                    return Err(acp::Error::new(code, message));
                }

                return Err(acp::Error::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!("unexpected ACP error while processing live prompt output: {message}"),
                ));
            }

            if let Some(result) = outbound_frame.get("result") {
                let frame_id = outbound_frame
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(acp::Error::internal_error)?;
                if frame_id == prompt_request_id {
                    prompt_response = Some(
                        serde_json::from_value::<acp::PromptResponse>(result.clone()).map_err(
                            |error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()),
                        )?,
                    );
                }
            }
        }

        Ok(prompt_response)
    }

    async fn process_cancel_outbound_frames(
        &self,
        session_id: &SessionId,
        cancel_request_id: u64,
        outbound_frames: Vec<OutboundFrame>,
    ) -> acp::Result<()> {
        let mut buffered_prompt_completion = None;

        for outbound_frame in outbound_frames {
            let outbound_frame = outbound_frame
                .into_value()
                .map_err(map_rpc_response_error)?;
            match outbound_frame.get("method").and_then(Value::as_str) {
                Some(SESSION_UPDATE_METHOD) => {
                    let notification = serde_json::from_value::<acp::SessionNotification>(
                        outbound_frame
                            .get("params")
                            .cloned()
                            .ok_or_else(acp::Error::internal_error)?,
                    )
                    .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
                    self.send_session_notification(notification).await?;
                }
                Some(SESSION_REQUEST_PERMISSION_METHOD) => {
                    let request = serde_json::from_value::<acp::RequestPermissionRequest>(
                        outbound_frame
                            .get("params")
                            .cloned()
                            .ok_or_else(acp::Error::internal_error)?,
                    )
                    .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
                    let _ = self.request_permission(request).await?;
                }
                _ => {
                    if let Some(error) = outbound_frame.get("error") {
                        let frame_id = outbound_frame
                            .get("id")
                            .and_then(Value::as_u64)
                            .ok_or_else(acp::Error::internal_error)?;
                        let code = error
                            .get("code")
                            .and_then(Value::as_i64)
                            .ok_or_else(acp::Error::internal_error)?
                            as i32;
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .ok_or_else(acp::Error::internal_error)?;
                        if frame_id == cancel_request_id {
                            return Err(acp::Error::new(code, message));
                        }
                        buffered_prompt_completion = Some(BufferedPromptCompletion::Error {
                            code,
                            message: message.to_string(),
                        });
                        continue;
                    }

                    if let Some(result) = outbound_frame.get("result") {
                        let frame_id = outbound_frame
                            .get("id")
                            .and_then(Value::as_u64)
                            .ok_or_else(acp::Error::internal_error)?;
                        if frame_id == cancel_request_id {
                            continue;
                        }

                        let prompt_response =
                            serde_json::from_value::<acp::PromptResponse>(result.clone()).map_err(
                                |error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()),
                            )?;
                        buffered_prompt_completion =
                            Some(BufferedPromptCompletion::Response(prompt_response));
                    }
                }
            }
        }

        if let Some(buffered_prompt_completion) = buffered_prompt_completion {
            self.connection
                .store_buffered_prompt_completion(session_id, buffered_prompt_completion)
                .await?;
        }

        Ok(())
    }
}

async fn cancel_via_adapter(
    adapter: &OfficialAgentAdapter,
    args: acp::CancelNotification,
) -> acp::Result<()> {
    let (session_id, cancel_request_id, outbound_frames) =
        adapter.connection.prepare_cancel_request(&args).await?;

    if let Some(buffered_prompt_completion) =
        buffered_prompt_completion_from_outbound_frames(cancel_request_id, &outbound_frames)?
    {
        adapter
            .connection
            .store_buffered_prompt_completion(&session_id, buffered_prompt_completion)
            .await?;
    }

    adapter
        .process_cancel_outbound_frames(&session_id, cancel_request_id, outbound_frames)
        .await
}

fn prepend_outbound_frames(queue: &mut VecDeque<OutboundFrame>, frames: Vec<OutboundFrame>) {
    for frame in frames.into_iter().rev() {
        queue.push_front(frame);
    }
}

impl OfficialAgentConnection {
    fn new(server: AcpServer) -> Self {
        Self {
            server,
            state: Mutex::new(AcpConnectionState::default()),
        }
    }

    async fn delegate_request<TParams>(
        &self,
        method: Method,
        params: TParams,
    ) -> acp::Result<Vec<OutboundFrame>>
    where
        TParams: Serialize,
    {
        let params = serde_json::to_value(params)
            .map_err(|error| acp::Error::new(JSONRPC_INVALID_PARAMS, error.to_string()))?;
        let mut connection = self.state.lock().await;
        let request_id = next_official_request_id(&mut connection);
        ensure_official_request_order(connection.protocol.state(), method)
            .map_err(map_protocol_error_to_acp_error)?;
        self.server
            .handle_request(
                &mut connection,
                ParsedRequest {
                    jsonrpc: crate::protocol::JSONRPC_VERSION.to_string(),
                    id: request_id,
                    method,
                    params,
                },
            )
            .await
            .map_err(map_rpc_response_error)
    }

    async fn managed_session_handle(
        &self,
        session_id: &SessionId,
        session_id_string: &str,
    ) -> acp::Result<ManagedSessionHandle> {
        let connection = self.state.lock().await;
        connection
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| active_session_not_found_error(session_id_string))
    }

    async fn prepare_official_session_request(
        &self,
        method: Method,
        session_id: &SessionId,
        session_id_string: &str,
    ) -> acp::Result<(u64, ManagedSessionHandle)> {
        let mut connection = self.state.lock().await;
        let request_id = next_official_request_id(&mut connection);
        ensure_official_request_order(connection.protocol.state(), method)
            .map_err(map_protocol_error_to_acp_error)?;
        self.server
            .ensure_authenticated(&connection)
            .map_err(map_rpc_response_error)?;
        let managed_session = connection
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| active_session_not_found_error(session_id_string))?;
        Ok((request_id, managed_session))
    }

    async fn process_permission_response(
        &self,
        request: &acp::RequestPermissionRequest,
        response: acp::RequestPermissionResponse,
    ) -> acp::Result<Vec<OutboundFrame>> {
        let session_id =
            parse_session_id(&request.session_id.to_string()).map_err(map_rpc_response_error)?;
        let managed_session = self
            .managed_session_handle(&session_id, &request.session_id.to_string())
            .await?;
        let mut managed_session = managed_session.lock().await;

        self.server
            .apply_permission_response(
                &request.session_id.to_string(),
                &mut managed_session,
                request,
                response,
            )
            .await
            .map_err(map_rpc_response_error)
    }

    async fn start_live_prompt_request(
        &self,
        args: acp::PromptRequest,
    ) -> acp::Result<(String, u64, ManagedSessionHandle, Vec<OutboundFrame>)> {
        let session_id = args.session_id.to_string();
        let managed_session_id = parse_session_id(&session_id).map_err(map_rpc_response_error)?;
        let prompt =
            official_prompt_text_from_blocks(&args.prompt).map_err(map_rpc_response_error)?;
        let (prompt_request_id, managed_session) = self
            .prepare_official_session_request(
                Method::SessionPrompt,
                &managed_session_id,
                &session_id,
            )
            .await?;
        let outbound_frames = {
            let mut managed_session = managed_session.lock().await;
            self.server
                .start_live_prompt_turn_for_session(
                    &session_id,
                    &mut managed_session,
                    prompt,
                    prompt_request_id,
                )
                .await
                .map_err(map_rpc_response_error)?
        };

        Ok((
            session_id,
            prompt_request_id,
            managed_session,
            outbound_frames,
        ))
    }

    async fn store_buffered_prompt_completion(
        &self,
        session_id: &SessionId,
        buffered_prompt_completion: BufferedPromptCompletion,
    ) -> acp::Result<()> {
        let managed_session = self
            .managed_session_handle(session_id, &session_id.to_string())
            .await?;
        let mut managed_session = managed_session.lock().await;
        managed_session.buffered_prompt_completion = Some(buffered_prompt_completion);
        Ok(())
    }

    async fn poll_live_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
    ) -> acp::Result<LivePromptPollResult> {
        self.server
            .poll_live_prompt_turn(session_id, managed_session)
            .await
            .map_err(map_rpc_response_error)
    }

    async fn prepare_cancel_request(
        &self,
        args: &acp::CancelNotification,
    ) -> acp::Result<(SessionId, u64, Vec<OutboundFrame>)> {
        let session_id =
            parse_session_id(&args.session_id.to_string()).map_err(map_rpc_response_error)?;
        let (cancel_request_id, managed_session) = self
            .prepare_official_session_request(
                Method::SessionCancel,
                &session_id,
                &args.session_id.to_string(),
            )
            .await?;
        let outbound_frames = {
            let mut managed_session = managed_session.lock().await;
            self.server
                .cancel_prompt_turn_for_session(
                    &args.session_id.to_string(),
                    &mut managed_session,
                    cancel_request_id,
                )
                .await
                .map_err(map_rpc_response_error)?
        };

        Ok((session_id, cancel_request_id, outbound_frames))
    }
}

impl OfficialAgentRuntime {
    fn new(
        server: AcpServer,
        outbound_sender: mpsc::UnboundedSender<OutboundClientCall>,
        test_probes_enabled: bool,
    ) -> Self {
        Self {
            connection: Arc::new(OfficialAgentConnection::new(server)),
            outbound_sender,
            test_probes_enabled,
        }
    }

    fn adapter(&self) -> OfficialAgentAdapter {
        OfficialAgentAdapter::from_runtime(self)
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for AcpServer {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::Initialize, args)
            .await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::Authenticate, args)
            .await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::SessionNew, args)
            .await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::SessionLoad, args)
            .await
    }

    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> acp::Result<acp::ResumeSessionResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::SessionResume, args)
            .await
    }

    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> acp::Result<acp::CloseSessionResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::SessionClose, args)
            .await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> acp::Result<acp::ListSessionsResponse> {
        self.official_agent_adapter()?
            .delegate_request(Method::SessionList, args)
            .await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        self.official_agent_adapter()?
            .handle_live_prompt_request(args)
            .await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        cancel_via_adapter(&self.official_agent_adapter()?, args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        let runtime = self.official_agent_runtime()?;
        if runtime.test_probes_enabled {
            return runtime.adapter().handle_ext_method(args).await;
        }

        Err(acp::Error::method_not_found())
    }
}

impl OfficialAgentTestProbeAdapter {
    fn new(adapter: OfficialAgentAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for OfficialAgentTestProbeAdapter {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        self.adapter
            .delegate_request(Method::Initialize, args)
            .await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        self.adapter
            .delegate_request(Method::Authenticate, args)
            .await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        self.adapter
            .delegate_request(Method::SessionNew, args)
            .await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        self.adapter
            .delegate_request(Method::SessionLoad, args)
            .await
    }

    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> acp::Result<acp::ResumeSessionResponse> {
        self.adapter
            .delegate_request(Method::SessionResume, args)
            .await
    }

    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> acp::Result<acp::CloseSessionResponse> {
        self.adapter
            .delegate_request(Method::SessionClose, args)
            .await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> acp::Result<acp::ListSessionsResponse> {
        self.adapter
            .delegate_request(Method::SessionList, args)
            .await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        self.adapter.handle_live_prompt_request(args).await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        cancel_via_adapter(&self.adapter, args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        self.adapter.handle_ext_method(args).await
    }
}

impl PromptTurnEmissionState {
    fn project_frames(
        &mut self,
        session_id: &str,
        projection: &PromptTurnProjection,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let mut outbound_frames = Vec::new();

        for event in &projection.events {
            match &event.event {
                PromptTurnEvent::SessionUpdate(update) => {
                    if let Some(update) =
                        self.live_session_update(event.sequence, event.phase, update)
                    {
                        outbound_frames.push(session_update_frame(session_id, update)?);
                    }
                }
                PromptTurnEvent::PermissionRequest(request) => {
                    if self
                        .emitted_permission_requests
                        .insert(request.tool_call.tool_call_id.clone())
                    {
                        outbound_frames.push(permission_request_frame(request)?);
                    }
                }
            }
        }

        if let Some(max_sequence) = projection.max_sequence {
            self.watermark = Some(
                self.watermark
                    .map_or(max_sequence, |current| current.max(max_sequence)),
            );
        }
        self.open_transcript_item_ids = projection
            .open_transcript_item_ids
            .iter()
            .copied()
            .collect();

        Ok(outbound_frames)
    }

    fn live_session_update(
        &mut self,
        sequence: u64,
        phase: ProjectionEventPhase,
        update: &SessionUpdate,
    ) -> Option<SessionUpdate> {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => self
                .live_text_delta(sequence, phase, &chunk.content)
                .map(|delta| {
                    SessionUpdate::UserMessageChunk(UserMessageChunk {
                        content: ContentBlock::text(delta),
                    })
                }),
            SessionUpdate::AgentThoughtChunk(chunk) => self
                .live_text_delta(sequence, phase, &chunk.content)
                .map(|delta| {
                    SessionUpdate::AgentThoughtChunk(AgentThoughtChunk {
                        content: ContentBlock::text(delta),
                    })
                }),
            SessionUpdate::AgentMessageChunk(chunk) => self
                .live_text_delta(sequence, phase, &chunk.content)
                .map(|delta| {
                    SessionUpdate::AgentMessageChunk(AgentMessageChunk {
                        content: ContentBlock::text(delta),
                    })
                }),
            SessionUpdate::ToolCall(tool_call) => self
                .emitted_tool_calls
                .insert(tool_call.tool_call_id.clone())
                .then(|| SessionUpdate::ToolCall(tool_call.clone())),
            SessionUpdate::ToolCallUpdate(tool_call_update) => {
                let tool_call_id = tool_call_update.tool_call_id.clone();
                if self.latest_tool_updates.get(&tool_call_id) == Some(tool_call_update) {
                    None
                } else {
                    self.latest_tool_updates
                        .insert(tool_call_id, tool_call_update.clone());
                    Some(SessionUpdate::ToolCallUpdate(tool_call_update.clone()))
                }
            }
            SessionUpdate::SessionInfoUpdate(session_info_update) => {
                (session_info_update.title.is_some() || session_info_update.updated_at.is_some())
                    .then(|| SessionUpdate::SessionInfoUpdate(session_info_update.clone()))
            }
        }
    }

    fn live_text_delta(
        &mut self,
        sequence: u64,
        phase: ProjectionEventPhase,
        content: &ContentBlock,
    ) -> Option<String> {
        let text = match content {
            ContentBlock::Text(text) => &text.text,
        };

        let emitted_length = self
            .emitted_text_lengths
            .entry((sequence, projection_phase_key(phase)))
            .or_default();
        if text.len() <= *emitted_length {
            return None;
        }

        let delta = text[*emitted_length..].to_string();
        *emitted_length = text.len();
        Some(delta)
    }
}

impl AcpServer {
    pub fn build(config: Config) -> Result<Self> {
        Ok(Self::from_dependencies(AcpServerDependencies::from_config(
            config,
        )?))
    }

    pub fn from_dependencies(dependencies: AcpServerDependencies) -> Self {
        let mapper = SessionUpdateMapper::from_acp_config(&dependencies.config.acp);

        Self {
            dependencies,
            transport: StdioTransport::new(),
            mapper,
        }
    }

    #[cfg(test)]
    fn dependencies(&self) -> &AcpServerDependencies {
        &self.dependencies
    }

    #[cfg(test)]
    fn transport(&self) -> StdioTransport {
        self.transport
    }

    fn server_info(&self) -> ServerInfo {
        self.mapper.server_info()
    }

    fn build_host(&self) -> Result<ManagedAppHost> {
        ManagedAppHost::load_or_create(
            self.dependencies.store.clone(),
            self.dependencies.runtime.clone(),
            Arc::clone(&self.dependencies.agent_registry),
            Arc::clone(&self.dependencies.tool_registry),
            self.dependencies.plugin_load_snapshot.clone(),
        )
    }

    fn build_host_for_session(&self, session: Session) -> ManagedAppHost {
        ManagedAppHost::new(
            session,
            self.dependencies.store.clone(),
            self.dependencies.runtime.clone(),
            Arc::clone(&self.dependencies.agent_registry),
            Arc::clone(&self.dependencies.tool_registry),
            self.dependencies.plugin_load_snapshot.clone(),
        )
    }

    fn load_host(&self, session_id: &SessionId) -> Result<ManagedAppHost> {
        ManagedAppHost::load(
            session_id,
            self.dependencies.store.clone(),
            self.dependencies.runtime.clone(),
            Arc::clone(&self.dependencies.agent_registry),
            Arc::clone(&self.dependencies.tool_registry),
            self.dependencies.plugin_load_snapshot.clone(),
        )
    }

    pub async fn run(self) -> Result<()> {
        let server_info = self.server_info();
        let store_root = self.dependencies.config.data_dir.clone();
        let plugin_count = self.dependencies.plugin_load_snapshot.plugin_count();
        let warning_count = self.dependencies.plugin_load_snapshot.warning_count();
        let mut host = self.build_host()?;
        host.recover_startup().await?;

        info!(
            transport = self.transport.kind(),
            server = %server_info.name,
            version = %server_info.version,
            protocol_version = self.dependencies.config.acp.protocol_version,
            auth_method_count = self.dependencies.config.acp.auth_methods.len(),
            session_store = %display_path(&store_root),
            session_id = %host.state().session.id,
            active_run_id = ?host.state().active_run_id,
            status = ?host.state().status,
            provider = %self.dependencies.config.model.provider,
            model = %self.dependencies.config.model.model,
            plugin_count,
            plugin_warning_count = warning_count,
            "acp server initialized with lifecycle-capable headless host"
        );

        let frames_processed = self.serve_stdio_via_official_sdk().await?;
        info!(
            frames_processed,
            "acp stdio server stopped after stdin closed"
        );

        Ok(())
    }

    async fn serve_stdio_via_official_sdk(&self) -> Result<usize> {
        self.serve_agent_connection(
            tokio::io::stdin().compat(),
            tokio::io::stdout().compat_write(),
        )
        .await
    }

    async fn serve_agent_connection<R, W>(&self, reader: R, writer: W) -> Result<usize>
    where
        R: AsyncRead + Unpin + 'static,
        W: AsyncWrite + Unpin + 'static,
    {
        let local_set = tokio::task::LocalSet::new();
        let server = self.clone();

        local_set
            .run_until(async move {
                let (outbound_sender, outbound_receiver) = mpsc::unbounded_channel();
                if official_test_probes_enabled() {
                    let probe_adapter = OfficialAgentTestProbeAdapter::new(
                        OfficialAgentAdapter::for_probe_connection(server, outbound_sender),
                    );
                    run_probe_official_agent_connection(
                        probe_adapter,
                        reader,
                        writer,
                        outbound_receiver,
                    )
                    .await
                } else {
                    run_official_agent_server_connection(
                        server,
                        reader,
                        writer,
                        outbound_sender,
                        outbound_receiver,
                    )
                    .await
                }
            })
            .await
    }

    fn official_agent_runtime(&self) -> acp::Result<OfficialAgentRuntime> {
        OFFICIAL_AGENT_RUNTIME.try_with(Clone::clone).map_err(|_| {
            acp::Error::new(
                JSONRPC_INTERNAL_ERROR,
                "official ACP server methods require an active per-connection runtime",
            )
        })
    }

    fn official_agent_adapter(&self) -> acp::Result<OfficialAgentAdapter> {
        Ok(self.official_agent_runtime()?.adapter())
    }

    fn initialize_response(&self, requested_protocol_version: u16) -> InitializeResponse {
        self.mapper.initialize_response(requested_protocol_version)
    }

    fn ensure_supported_protocol_version(
        &self,
        requested_protocol_version: u16,
    ) -> std::result::Result<(), RpcResponseError> {
        let supported_protocol_version = self.dependencies.config.acp.protocol_version;
        if requested_protocol_version == supported_protocol_version {
            return Ok(());
        }

        Err(RpcResponseError::invalid_params(format!(
            "unsupported ACP protocol version `{requested_protocol_version}`; expected `{supported_protocol_version}`"
        )))
    }

    #[cfg(test)]
    pub(crate) async fn serve_live_frames<W: Write>(
        &self,
        frame_receiver: &mut mpsc::UnboundedReceiver<ReaderEvent>,
        writer: &mut W,
    ) -> Result<usize> {
        let mut connection = AcpConnectionState::default();
        let mut frames_processed = 0;
        let mut reader_closed = false;

        while !reader_closed || has_live_prompt_turns(&connection) {
            if self
                .flush_live_prompt_turn_updates(&mut connection, writer)
                .await?
            {
                continue;
            }

            let active_live_prompt_turns = active_live_prompt_turn_handles(&connection);
            tokio::select! {
                maybe_event = frame_receiver.recv(), if !reader_closed => {
                    let Some(event) = maybe_event else {
                        reader_closed = true;
                        continue;
                    };

                    match event {
                        ReaderEvent::Frame(frame) => {
                            frames_processed += 1;
                            debug!(frame_len = frame.len(), "received ACP stdio frame");

                            let outbound_frames = match self.handle_live_frame(&mut connection, &frame).await {
                                Ok(outbound_frames) => outbound_frames,
                                Err(error_response) => vec![serialize_value(error_response)
                                    .map_err(|error| FluentCodeError::Provider(error.message))?],
                            };
                            self.write_outbound_frames(writer, outbound_frames.clone())?;
                            clear_written_prompt_responses(
                                &mut connection,
                                &frame_ids_from_responses(&outbound_frames),
                            );

                            if has_live_prompt_turns(&connection) {
                                self.flush_live_prompt_turn_updates(&mut connection, writer)
                                    .await?;
                            }
                        }
                        ReaderEvent::Eof => {
                            reader_closed = true;
                        }
                    }
                }
                woke_for_runtime_activity = wait_for_live_prompt_turn_activity(active_live_prompt_turns), if !active_live_prompt_turns.is_empty() => {
                    let wrote_frames = self
                        .flush_live_prompt_turn_updates(&mut connection, writer)
                        .await?;
                    if !woke_for_runtime_activity && !wrote_frames && has_live_prompt_turns(&connection) {
                        return Err(FluentCodeError::Provider(
                            "live ACP prompt turns stopped receiving runtime activity before completing".to_string(),
                        ));
                    }
                }
            }
        }

        Ok(frames_processed)
    }

    #[cfg(test)]
    pub(crate) async fn serve_jsonl_script<R: BufRead, W: Write>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<usize> {
        let mut connection = AcpConnectionState::default();
        let mut frames_processed = 0;

        while let Some(frame) = self.transport.read_frame(reader).map_err(|error| {
            FluentCodeError::Provider(format!("ACP stdio transport error: {error}"))
        })? {
            frames_processed += 1;
            debug!(frame_len = frame.len(), "received ACP stdio frame");

            let outbound_frames = match self.handle_frame(&mut connection, &frame).await {
                Ok(outbound_frames) => outbound_frames,
                Err(error_response) => vec![
                    serialize_value(error_response)
                        .map_err(|error| FluentCodeError::Provider(error.message))?,
                ],
            };
            self.write_outbound_frames(writer, outbound_frames)?;
        }

        Ok(frames_processed)
    }

    #[cfg(test)]
    async fn serve_frames<R: BufRead, W: Write>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<usize> {
        let mut connection = AcpConnectionState::default();
        let mut frames_processed = 0;

        while let Some(frame) = self.transport.read_frame(reader).map_err(|error| {
            FluentCodeError::Provider(format!("ACP stdio transport error: {error}"))
        })? {
            frames_processed += 1;
            debug!(frame_len = frame.len(), "received ACP stdio frame");

            let outbound_frames = match self.handle_frame(&mut connection, &frame).await {
                Ok(outbound_frames) => outbound_frames,
                Err(error_response) => vec![
                    serialize_value(error_response)
                        .map_err(|error| FluentCodeError::Provider(error.message))?,
                ],
            };
            self.write_outbound_frames(writer, outbound_frames)?;
        }

        Ok(frames_processed)
    }

    #[cfg(test)]
    fn write_outbound_frames<W: Write>(
        &self,
        writer: &mut W,
        outbound_frames: Vec<OutboundFrame>,
    ) -> Result<()> {
        for outbound_frame in outbound_frames {
            writer
                .write_all(outbound_frame.json.as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .and_then(|()| writer.flush())
                .map_err(|error| {
                    FluentCodeError::Provider(format!("ACP stdio transport error: {error}"))
                })?;
        }

        Ok(())
    }

    #[cfg(test)]
    async fn handle_frame(
        &self,
        connection: &mut AcpConnectionState,
        frame: &str,
    ) -> std::result::Result<Vec<OutboundFrame>, JsonRpcErrorResponse> {
        let request_id = request_id_from_frame(frame);
        let request = self
            .parse_request(connection, frame)
            .map_err(|error| protocol_error_response(request_id.clone(), error))?;

        self.handle_request(connection, request)
            .await
            .map_err(|error| JsonRpcErrorResponse::new(request_id, error.code, error.message))
    }

    #[cfg(test)]
    async fn handle_live_frame(
        &self,
        connection: &mut AcpConnectionState,
        frame: &str,
    ) -> std::result::Result<Vec<OutboundFrame>, JsonRpcErrorResponse> {
        let request_id = request_id_from_frame(frame);
        let request = self
            .parse_request(connection, frame)
            .map_err(|error| protocol_error_response(request_id.clone(), error))?;

        let result = match request.method {
            Method::SessionPrompt => self.handle_live_session_prompt(connection, request).await,
            _ => self.handle_request(connection, request).await,
        };

        result.map_err(|error| JsonRpcErrorResponse::new(request_id, error.code, error.message))
    }

    #[cfg(test)]
    fn parse_request(
        &self,
        connection: &mut AcpConnectionState,
        frame: &str,
    ) -> std::result::Result<ParsedRequest, ProtocolError> {
        connection.protocol.parse_request(frame)
    }

    async fn handle_request(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        match request.method {
            Method::Initialize => self.handle_initialize(connection, request),
            Method::Authenticate => self.handle_authenticate(connection, request),
            Method::SessionNew => self.handle_session_new(connection, request),
            Method::SessionLoad => self.handle_session_load(connection, request).await,
            Method::SessionResume => self.handle_session_resume(connection, request).await,
            Method::SessionClose => self.handle_session_close(connection, request),
            Method::SessionList => self.handle_session_list(connection, request),
            Method::SessionPrompt => self.handle_session_prompt(connection, request).await,
            Method::SessionCancel => self.handle_session_cancel(connection, request).await,
        }
    }

    fn handle_initialize(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let initialize_request = decode_params::<InitializeRequest>(request.params)?;
        self.ensure_supported_protocol_version(initialize_request.protocol_version)?;
        let response = self.initialize_response(initialize_request.protocol_version);
        connection
            .protocol
            .mark_initialized()
            .map_err(|error| RpcResponseError::invalid_params(error.to_string()))?;

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id, response,
        ))?])
    }

    fn handle_authenticate(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let authenticate_request = decode_params::<AuthenticateRequest>(request.params)?;
        let configured_auth_methods = &self.dependencies.config.acp.auth_methods;

        if configured_auth_methods.is_empty() {
            return Err(RpcResponseError::invalid_params(
                "authenticate is unavailable because initialize did not advertise any auth methods",
            ));
        }

        if !configured_auth_methods
            .iter()
            .any(|method| method.id == authenticate_request.method_id)
        {
            return Err(RpcResponseError::invalid_params(format!(
                "unsupported auth method `{}`",
                authenticate_request.method_id
            )));
        }

        connection.authenticated = true;

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id,
            AuthenticateResponse::default(),
        ))?])
    }

    fn handle_session_new(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let new_session_request = decode_params::<NewSessionRequest>(request.params)?;
        let cwd = validate_absolute_cwd(&new_session_request.cwd)?;
        let mcp_servers = parse_mcp_servers(&new_session_request.mcp_servers);
        if !mcp_servers.is_empty() {
            info!(
                mcp_server_count = mcp_servers.len(),
                mcp_server_names = %mcp_servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
                "session created with MCP server configurations"
            );
        }
        let session = self
            .dependencies
            .store
            .create_new_session()
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;
        let session_id = session.id;
        let session_id_string = session_id.to_string();
        let host = self.build_host_for_session(session);
        connection.sessions.insert(
            session_id,
            Arc::new(Mutex::new(ManagedSession {
                cwd,
                mcp_servers,
                host,
                live_prompt_turn: None,
                pending_prompt_request_id: None,
                buffered_prompt_completion: None,
            })),
        );

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id,
            NewSessionResponse {
                session_id: session_id_string,
                config_options: self.mapper.session_config_options(),
            },
        ))?])
    }

    async fn handle_session_load(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let load_session_request = decode_params::<LoadSessionRequest>(request.params)?;
        let session_id = parse_session_id(&load_session_request.session_id)?;
        let cwd = validate_absolute_cwd(&load_session_request.cwd)?;
        let mut host = self
            .load_host(&session_id)
            .map_err(map_load_session_error)?;
        host.recover_startup()
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        let session_id_string = host.state().session.id.to_string();
        let mut outbound_frames = self.session_load_replay_frames(&session_id_string, &host)?;
        let latest_prompt_state = latest_prompt_state(host.state());
        let live_prompt_turn = self
            .seed_live_prompt_turn_state(&session_id_string, &host)
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;
        outbound_frames.push(serialize_value(JsonRpcResponse::new(
            request.id,
            LoadSessionResponse {
                config_options: self.mapper.session_config_options(),
                latest_prompt_state,
                replay_fidelity: replay_fidelity(&host.state().session),
                meta: Some(load_session_meta(
                    latest_prompt_state,
                    replay_fidelity(&host.state().session),
                )),
            },
        ))?);

        connection.sessions.insert(
            host.state().session.id,
            Arc::new(Mutex::new(ManagedSession {
                cwd: cwd,
                mcp_servers: parse_mcp_servers(&load_session_request.mcp_servers),
                host,
                live_prompt_turn,
                pending_prompt_request_id: None,
                buffered_prompt_completion: None,
            })),
        );

        Ok(outbound_frames)
    }

    async fn handle_session_resume(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let resume_request = decode_params::<ResumeSessionRequest>(request.params)?;
        let session_id = parse_session_id(&resume_request.session_id)?;
        let cwd = validate_absolute_cwd(&resume_request.cwd)?;
        let mut host = self
            .load_host(&session_id)
            .map_err(map_load_session_error)?;
        host.recover_startup()
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        let live_prompt_turn = self
            .seed_live_prompt_turn_state(&resume_request.session_id, &host)
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        connection.sessions.insert(
            host.state().session.id,
            Arc::new(Mutex::new(ManagedSession {
                cwd: cwd,
                mcp_servers: parse_mcp_servers(&resume_request.mcp_servers),
                host,
                live_prompt_turn,
                pending_prompt_request_id: None,
                buffered_prompt_completion: None,
            })),
        );

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id,
            ResumeSessionResponse {
                config_options: self.mapper.session_config_options(),
                meta: None,
            },
        ))?])
    }

    fn handle_session_close(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let close_request = decode_params::<CloseSessionRequest>(request.params)?;
        let session_id = parse_session_id(&close_request.session_id)?;

        connection.sessions.remove(&session_id).ok_or_else(|| {
            RpcResponseError::resource_not_found(format!(
                "session `{session_id}` is not active on this connection"
            ))
        })?;

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id,
            CloseSessionResponse { meta: None },
        ))?])
    }

    fn handle_session_list(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let _list_request = decode_params::<ListSessionsRequest>(request.params)?;

        let summaries = self
            .dependencies
            .store
            .list_sessions()
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        let session_entries = summaries
            .into_iter()
            .map(|summary| SessionInfoEntry {
                session_id: summary.session_id,
                title: summary.title,
                updated_at: summary.updated_at,
            })
            .collect();

        Ok(vec![serialize_value(JsonRpcResponse::new(
            request.id,
            ListSessionsResponse {
                sessions: session_entries,
                next_cursor: None,
            },
        ))?])
    }

    async fn handle_session_prompt(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let session_prompt_request = decode_params::<SessionPromptRequest>(request.params)?;
        let session_id = parse_session_id(&session_prompt_request.session_id)?;
        let prompt = prompt_text_from_blocks(&session_prompt_request.prompt)?;
        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                RpcResponseError::resource_not_found(format!(
                    "session `{session_id}` is not active on this connection"
                ))
            })?;
        let mut managed_session = managed_session.lock().await;

        if managed_session.host.state().active_run_id.is_some() {
            return Err(RpcResponseError::active_prompt(format!(
                "session `{session_id}` already has an active prompt turn"
            )));
        }

        let mut prompt_result = self
            .run_prompt_turn(
                &session_prompt_request.session_id,
                &mut managed_session.host,
                prompt,
            )
            .await?;
        prompt_result
            .frames
            .push(serialize_value(JsonRpcResponse::new(
                request.id,
                prompt_result.stop_reason,
            ))?);
        Ok(prompt_result.frames)
    }

    #[cfg(test)]
    async fn handle_live_session_prompt(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let request_id = request.id;
        let session_prompt_request = decode_params::<SessionPromptRequest>(request.params)?;
        let session_id = parse_session_id(&session_prompt_request.session_id)?;
        let prompt = prompt_text_from_blocks(&session_prompt_request.prompt)?;
        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                RpcResponseError::resource_not_found(format!(
                    "session `{session_id}` is not active on this connection"
                ))
            })?;
        let mut managed_session = managed_session.lock().await;

        if managed_session.host.state().active_run_id.is_some() {
            return Err(RpcResponseError::active_prompt(format!(
                "session `{session_id}` already has an active prompt turn"
            )));
        }

        self.start_live_prompt_turn(
            &session_prompt_request.session_id,
            &mut managed_session,
            prompt,
            request_id,
        )
        .await
    }

    async fn handle_session_cancel(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let session_cancel_request = decode_params::<SessionCancelRequest>(request.params)?;
        let session_id = parse_session_id(&session_cancel_request.session_id)?;
        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                RpcResponseError::resource_not_found(format!(
                    "session `{session_id}` is not active on this connection"
                ))
            })?;
        let mut managed_session = managed_session.lock().await;

        self.ensure_live_prompt_turn_seeded(
            &session_cancel_request.session_id,
            &mut managed_session,
        )?;
        let prompt_request_id = managed_session.pending_prompt_request_id;
        let (mut outbound_frames, mut prompt_completion_frames) = split_prompt_completion_frames(
            prompt_request_id,
            self.drain_live_prompt_turn_updates(
                &session_cancel_request.session_id,
                &mut managed_session,
            )
            .await?,
        );
        let cancel_target = cancel_target(managed_session.host.state()).ok_or_else(|| {
            RpcResponseError::invalid_params(format!(
                "session `{session_id}` does not have an active prompt turn to cancel"
            ))
        })?;

        let (cancel_frames, cancel_prompt_completion_frames) = split_prompt_completion_frames(
            prompt_request_id,
            self.cancel_prompt_turn(
                &session_cancel_request.session_id,
                &mut managed_session,
                cancel_target,
            )
            .await?,
        );
        outbound_frames.extend(cancel_frames);
        outbound_frames.push(serialize_value(JsonRpcResponse::new(
            request.id,
            SessionPromptResponse {
                stop_reason: crate::protocol::StopReason::Cancelled,
            },
        ))?);
        prompt_completion_frames.extend(cancel_prompt_completion_frames);
        outbound_frames.extend(prompt_completion_frames);
        Ok(outbound_frames)
    }

    async fn start_live_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        prompt: String,
        request_id: u64,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        managed_session
            .host
            .submit_prompt(prompt)
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;
        managed_session.pending_prompt_request_id = Some(request_id);
        managed_session.live_prompt_turn = self
            .seed_live_prompt_turn_state(session_id, &managed_session.host)
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        self.poll_live_prompt_turn(session_id, managed_session)
            .await
            .map(|result| result.frames)
    }

    async fn start_live_prompt_turn_for_session(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        prompt: String,
        request_id: u64,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        if managed_session.host.state().active_run_id.is_some() {
            return Err(RpcResponseError::active_prompt(format!(
                "session `{session_id}` already has an active prompt turn"
            )));
        }

        self.start_live_prompt_turn(session_id, managed_session, prompt, request_id)
            .await
    }

    fn seed_live_prompt_turn_state(
        &self,
        session_id: &str,
        host: &ManagedAppHost,
    ) -> Result<Option<LivePromptTurnState>> {
        let Some(root_run_id) = current_prompt_turn_root_run_id(host.state()) else {
            return Ok(None);
        };
        let projection = self.mapper.project_prompt_turn(host.state(), root_run_id).ok_or_else(|| {
            FluentCodeError::Provider(format!(
                "session `{session_id}` lost active prompt turn projection for run `{root_run_id}`"
            ))
        })?;
        let mut emission_state = PromptTurnEmissionState::default();
        emission_state
            .project_frames(session_id, &projection)
            .map_err(|error| FluentCodeError::Provider(error.message.clone()))?;

        Ok(Some(LivePromptTurnState {
            root_run_id,
            emission_state,
        }))
    }

    async fn drain_live_prompt_turn_updates(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.poll_live_prompt_turn(session_id, managed_session)
            .await
            .map(|result| result.frames)
    }

    async fn cancel_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        cancel_target: CancelTarget,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        managed_session
            .host
            .handle_message(Msg::CancelActiveRun)
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        if cancel_target.active_run_is_child
            && managed_session.host.state().active_run_id == Some(cancel_target.root_run_id)
        {
            managed_session
                .host
                .handle_message(Msg::CancelActiveRun)
                .await
                .map_err(|error| RpcResponseError::internal(error.to_string()))?;
        }

        if terminalize_cancelled_prompt_turn(
            managed_session.host.state_mut(),
            cancel_target.root_run_id,
            cancel_target.foreground_phase,
            cancel_target.batch_anchor_turn_id,
        ) {
            managed_session.host.persist_now().map_err(|error| {
                RpcResponseError::internal(format!(
                    "failed to persist cancelled prompt turn for session `{session_id}`: {error}"
                ))
            })?;
        }

        self.poll_live_prompt_turn(session_id, managed_session)
            .await
            .map(|result| result.frames)
    }

    async fn cancel_prompt_turn_for_session(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        request_id: u64,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        self.ensure_live_prompt_turn_seeded(session_id, managed_session)?;
        let prompt_request_id = managed_session.pending_prompt_request_id;
        let (mut outbound_frames, mut prompt_completion_frames) = split_prompt_completion_frames(
            prompt_request_id,
            self.drain_live_prompt_turn_updates(session_id, managed_session)
                .await?,
        );
        outbound_frames.push(serialize_value(JsonRpcResponse::new(
            request_id,
            SessionPromptResponse {
                stop_reason: crate::protocol::StopReason::Cancelled,
            },
        ))?);
        let cancel_target = cancel_target(managed_session.host.state()).ok_or_else(|| {
            RpcResponseError::invalid_params(format!(
                "session `{session_id}` does not have an active prompt turn to cancel"
            ))
        })?;

        let (cancel_frames, cancel_prompt_completion_frames) = split_prompt_completion_frames(
            prompt_request_id,
            self.cancel_prompt_turn(session_id, managed_session, cancel_target)
                .await?,
        );
        outbound_frames.extend(cancel_frames);
        prompt_completion_frames.extend(cancel_prompt_completion_frames);
        outbound_frames.extend(prompt_completion_frames);
        Ok(outbound_frames)
    }

    fn ensure_live_prompt_turn_seeded(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
    ) -> std::result::Result<(), RpcResponseError> {
        if managed_session.live_prompt_turn.is_some() {
            return Ok(());
        }

        managed_session.live_prompt_turn = self
            .seed_live_prompt_turn_state(session_id, &managed_session.host)
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;
        Ok(())
    }

    async fn apply_permission_response(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        request: &acp::RequestPermissionRequest,
        response: acp::RequestPermissionResponse,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        match response.outcome {
            acp::RequestPermissionOutcome::Cancelled => {
                let cancel_target =
                    cancel_target(managed_session.host.state()).ok_or_else(|| {
                        RpcResponseError::invalid_params(format!(
                            "session `{session_id}` does not have an active prompt turn to cancel"
                        ))
                    })?;
                self.cancel_prompt_turn(session_id, managed_session, cancel_target)
                    .await
            }
            acp::RequestPermissionOutcome::Selected(selected) => {
                self.apply_selected_permission_option(
                    managed_session,
                    request,
                    &selected.option_id,
                )
                .await?;
                self.poll_live_prompt_turn(session_id, managed_session)
                    .await
                    .map(|result| result.frames)
            }
            _ => Err(RpcResponseError::invalid_params(
                "unsupported ACP permission response outcome",
            )),
        }
    }

    async fn apply_selected_permission_option(
        &self,
        managed_session: &mut ManagedSession,
        request: &acp::RequestPermissionRequest,
        option_id: &acp::PermissionOptionId,
    ) -> std::result::Result<(), RpcResponseError> {
        let option_id = option_id.to_string();
        if option_id == "reject_always" {
            remember_rejected_permission_rule(managed_session.host.state_mut(), request)?;
        }

        let reply = permission_reply_for_option_id(&option_id)?;
        managed_session
            .host
            .handle_message(Msg::ReplyToPendingTool(reply))
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))
    }

    #[cfg(test)]
    async fn poll_live_prompt_turns(
        &self,
        connection: &mut AcpConnectionState,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let mut outbound_frames = Vec::new();

        for managed_session in connection.sessions.values() {
            let mut managed_session = managed_session.lock().await;
            let session_id = managed_session.host.state().session.id.to_string();
            if managed_session.live_prompt_turn.is_none() {
                outbound_frames.extend(self.flush_pending_prompt_response(&mut managed_session)?);
                continue;
            }

            outbound_frames.extend(
                self.poll_live_prompt_turn(&session_id, &mut managed_session)
                    .await?
                    .frames,
            );
        }

        Ok(outbound_frames)
    }

    #[cfg(test)]
    async fn flush_live_prompt_turn_updates<W: Write>(
        &self,
        connection: &mut AcpConnectionState,
        writer: &mut W,
    ) -> Result<bool> {
        if !has_live_prompt_turns(connection) {
            return Ok(false);
        }

        let outbound_frames = self
            .poll_live_prompt_turns(connection)
            .await
            .map_err(|error| FluentCodeError::Provider(error.message))?;
        if outbound_frames.is_empty() {
            return Ok(false);
        }

        self.write_outbound_frames(writer, outbound_frames.clone())?;
        clear_written_prompt_responses(connection, &frame_ids_from_responses(&outbound_frames));
        Ok(true)
    }

    #[cfg(test)]
    fn flush_pending_prompt_response(
        &self,
        managed_session: &mut ManagedSession,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let Some(request_id) = managed_session.pending_prompt_request_id else {
            return Ok(Vec::new());
        };

        let Some(run_id) = latest_root_run_id(managed_session.host.state()) else {
            return Ok(Vec::new());
        };
        let Some(run) = managed_session.host.state().session.find_run(run_id) else {
            return Ok(Vec::new());
        };

        let Some(reason) = run
            .terminal_stop_reason
            .or_else(|| run.status.default_terminal_stop_reason())
        else {
            return Ok(Vec::new());
        };

        match reason {
            RunTerminalStopReason::Completed => Ok(vec![serialize_value(JsonRpcResponse::new(
                request_id,
                SessionPromptResponse {
                    stop_reason: crate::protocol::StopReason::EndTurn,
                },
            ))?]),
            RunTerminalStopReason::Cancelled => Ok(vec![serialize_value(JsonRpcResponse::new(
                request_id,
                SessionPromptResponse {
                    stop_reason: crate::protocol::StopReason::Cancelled,
                },
            ))?]),
            RunTerminalStopReason::Failed | RunTerminalStopReason::Interrupted => {
                let error =
                    prompt_turn_terminal_error(managed_session.host.state(), run_id, reason);
                Ok(vec![serialize_value(JsonRpcErrorResponse::new(
                    Value::from(request_id),
                    error.code,
                    error.message,
                ))?])
            }
        }
    }

    async fn poll_live_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
    ) -> std::result::Result<LivePromptPollResult, RpcResponseError> {
        managed_session
            .host
            .drain_runtime_messages()
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        let Some(mut live_prompt_turn) = managed_session.live_prompt_turn.take() else {
            return Ok(LivePromptPollResult {
                frames: Vec::new(),
                prompt_turn_complete: true,
            });
        };
        let projection = self
            .mapper
            .project_live_prompt_turn(
                managed_session.host.state(),
                live_prompt_turn.root_run_id,
                live_prompt_turn.emission_state.watermark,
                &live_prompt_turn.emission_state.open_transcript_item_ids,
            )
            .ok_or_else(|| {
                RpcResponseError::internal(format!(
                    "session `{session_id}` lost active prompt turn projection for run `{}`",
                    live_prompt_turn.root_run_id,
                ))
            })?;
        let mut outbound_frames = live_prompt_turn
            .emission_state
            .project_frames(session_id, &projection)?;

        match prompt_turn_response(
            managed_session.host.state(),
            live_prompt_turn.root_run_id,
            &projection,
        ) {
            Ok(Some(stop_reason)) => {
                if let Some(request_id) = managed_session.pending_prompt_request_id {
                    outbound_frames.push(serialize_value(JsonRpcResponse::new(
                        request_id,
                        stop_reason,
                    ))?);
                }
                managed_session.pending_prompt_request_id = None;
            }
            Ok(None) => {
                managed_session.live_prompt_turn = Some(live_prompt_turn);
            }
            Err(error) => {
                if let Some(request_id) = managed_session.pending_prompt_request_id {
                    outbound_frames.push(serialize_value(JsonRpcErrorResponse::new(
                        Value::from(request_id),
                        error.code,
                        error.message,
                    ))?);
                }
                managed_session.pending_prompt_request_id = None;
            }
        }

        Ok(LivePromptPollResult {
            frames: outbound_frames,
            prompt_turn_complete: managed_session.live_prompt_turn.is_none(),
        })
    }

    fn ensure_authenticated(
        &self,
        connection: &AcpConnectionState,
    ) -> std::result::Result<(), RpcResponseError> {
        if self.dependencies.config.acp.auth_methods.is_empty() || connection.authenticated {
            return Ok(());
        }

        Err(RpcResponseError::auth_required(
            "authentication is required before creating or loading sessions",
        ))
    }

    fn session_load_replay_frames(
        &self,
        session_id: &str,
        host: &ManagedAppHost,
    ) -> std::result::Result<Vec<OutboundFrame>, RpcResponseError> {
        let mut outbound_frames = Vec::new();

        for projection in self.mapper.project_prompt_turns(host.state()) {
            for event in projection.events {
                match event.event {
                    PromptTurnEvent::SessionUpdate(update) => {
                        outbound_frames.push(session_update_frame(session_id, update)?);
                    }
                    PromptTurnEvent::PermissionRequest(request) => {
                        outbound_frames.push(permission_request_frame(&request)?);
                    }
                }
            }
        }

        Ok(outbound_frames)
    }

    async fn run_prompt_turn(
        &self,
        session_id: &str,
        host: &mut ManagedAppHost,
        prompt: String,
    ) -> std::result::Result<PromptTurnResult, RpcResponseError> {
        host.submit_prompt(prompt)
            .await
            .map_err(|error| RpcResponseError::internal(error.to_string()))?;

        let run_id = host.state().active_run_id.ok_or_else(|| {
            RpcResponseError::internal(
                "prompt submission did not leave the session with an active run",
            )
        })?;
        let mut emission_state = PromptTurnEmissionState::default();
        let mut outbound_frames = Vec::new();

        let emit_prompt_turn_projection =
            |host: &ManagedAppHost,
             emission_state: &mut PromptTurnEmissionState,
             outbound_frames: &mut Vec<OutboundFrame>|
             -> std::result::Result<Option<SessionPromptResponse>, RpcResponseError> {
                let projection = self
                    .mapper
                    .project_prompt_turn(host.state(), run_id)
                    .ok_or_else(|| {
                        RpcResponseError::internal(format!(
                            "session `{session_id}` lost prompt turn projection for run `{run_id}`"
                        ))
                    })?;
                outbound_frames.extend(emission_state.project_frames(session_id, &projection)?);

                prompt_turn_response(host.state(), run_id, &projection)
            };

        loop {
            host.drain_runtime_messages()
                .await
                .map_err(|error| RpcResponseError::internal(error.to_string()))?;

            if let Some(stop_reason) =
                emit_prompt_turn_projection(host, &mut emission_state, &mut outbound_frames)?
            {
                return Ok(PromptTurnResult {
                    frames: outbound_frames,
                    stop_reason,
                });
            }

            let woke_for_runtime_activity = host.wait_for_runtime_activity().await;

            host.drain_runtime_messages()
                .await
                .map_err(|error| RpcResponseError::internal(error.to_string()))?;

            if let Some(stop_reason) =
                emit_prompt_turn_projection(host, &mut emission_state, &mut outbound_frames)?
            {
                return Ok(PromptTurnResult {
                    frames: outbound_frames,
                    stop_reason,
                });
            }

            if !woke_for_runtime_activity {
                return Err(RpcResponseError::internal(format!(
                    "session `{session_id}` stopped receiving runtime activity before completing prompt turn run `{run_id}`"
                )));
            }
        }
    }
}

fn latest_prompt_state(state: &AppState) -> Option<PromptTurnState> {
    let latest_root_run = state
        .session
        .runs
        .iter()
        .filter(|run| run.parent_run_id.is_none())
        .max_by_key(|run| run.latest_replay_sequence())?;

    if let Some(reason) = latest_root_run
        .terminal_stop_reason
        .or_else(|| latest_root_run.status.default_terminal_stop_reason())
    {
        return Some(match reason {
            RunTerminalStopReason::Completed => PromptTurnState::Completed,
            RunTerminalStopReason::Cancelled => PromptTurnState::Cancelled,
            RunTerminalStopReason::Failed => PromptTurnState::Failed,
            RunTerminalStopReason::Interrupted => PromptTurnState::Interrupted,
        });
    }

    if current_prompt_turn_root_run_id(state) == Some(latest_root_run.id) {
        return Some(match state.status {
            AppStatus::Generating => PromptTurnState::Running,
            AppStatus::AwaitingToolApproval => PromptTurnState::AwaitingToolApproval,
            AppStatus::RunningTool => PromptTurnState::RunningTool,
            AppStatus::Idle | AppStatus::Error(_) => PromptTurnState::Running,
        });
    }

    None
}

fn replay_fidelity(session: &Session) -> ReplayFidelity {
    match session.transcript_fidelity {
        fluent_code_app::session::model::TranscriptFidelity::Exact => ReplayFidelity::Exact,
        fluent_code_app::session::model::TranscriptFidelity::Approximate => {
            ReplayFidelity::Approximate
        }
    }
}

fn load_session_meta(
    latest_prompt_state: Option<PromptTurnState>,
    replay_fidelity: ReplayFidelity,
) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::new();
    meta.insert(
        ACP_META_LATEST_PROMPT_STATE_KEY.to_string(),
        serde_json::to_value(latest_prompt_state)
            .expect("load-session prompt state metadata should serialize"),
    );
    meta.insert(
        ACP_META_REPLAY_FIDELITY_KEY.to_string(),
        serde_json::to_value(replay_fidelity)
            .expect("load-session replay fidelity metadata should serialize"),
    );
    meta
}

#[cfg(test)]
fn frame_ids_from_responses(frames: &[OutboundFrame]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|frame| frame.to_value().ok())
        .filter_map(|frame| frame.get("id").and_then(Value::as_u64))
        .collect()
}

#[cfg(test)]
fn clear_written_prompt_responses(connection: &mut AcpConnectionState, written_ids: &[u64]) {
    if written_ids.is_empty() {
        return;
    }

    for managed_session in connection.sessions.values() {
        let mut managed_session = managed_session
            .try_lock()
            .expect("test harness should not contend on managed sessions");
        if managed_session
            .pending_prompt_request_id
            .is_some_and(|request_id| written_ids.contains(&request_id))
        {
            managed_session.pending_prompt_request_id = None;
        }
    }
}

#[cfg(test)]
fn has_live_prompt_turns(connection: &AcpConnectionState) -> bool {
    connection.sessions.values().any(|managed_session| {
        let managed_session = managed_session
            .try_lock()
            .expect("test harness should not contend on managed sessions");
        managed_session.live_prompt_turn.is_some()
            || managed_session.pending_prompt_request_id.is_some()
    })
}

#[cfg(test)]
fn active_live_prompt_turn_handles(connection: &AcpConnectionState) -> Vec<ManagedSessionHandle> {
    connection
        .sessions
        .values()
        .filter_map(|managed_session| {
            let has_live_prompt_turn = managed_session
                .try_lock()
                .expect("test harness should not contend on managed sessions")
                .live_prompt_turn
                .is_some();
            has_live_prompt_turn.then(|| Arc::clone(managed_session))
        })
        .collect()
}

#[cfg(test)]
async fn wait_for_live_prompt_turn_activity(managed_sessions: Vec<ManagedSessionHandle>) -> bool {
    let mut waits = FuturesUnordered::new();

    for managed_session in managed_sessions {
        waits.push(async move {
            let mut managed_session = managed_session.lock().await;
            managed_session.host.wait_for_runtime_activity().await
        });
    }

    while let Some(woke_for_runtime_activity) = waits.next().await {
        if woke_for_runtime_activity {
            return true;
        }
    }

    false
}

fn cancel_target(state: &AppState) -> Option<CancelTarget> {
    let owner = state.session.foreground_owner.as_ref()?;
    let root_run_id = root_run_id_for(state, owner.run_id)?;
    let active_run = state.session.find_run(owner.run_id)?;

    Some(CancelTarget {
        root_run_id,
        foreground_phase: owner.phase,
        batch_anchor_turn_id: owner.batch_anchor_turn_id,
        active_run_is_child: active_run.parent_run_id.is_some(),
    })
}

impl Default for AcpConnectionState {
    fn default() -> Self {
        Self {
            protocol: JsonRpcProtocol::default(),
            authenticated: false,
            sessions: HashMap::new(),
            next_request_id: 1,
        }
    }
}

fn active_session_not_found_error(session_id: &str) -> acp::Error {
    acp::Error::new(
        ACP_RESOURCE_NOT_FOUND,
        format!("session `{session_id}` is not active on this connection"),
    )
}

fn official_prompt_text_from_blocks(
    prompt: &[acp::ContentBlock],
) -> std::result::Result<String, RpcResponseError> {
    join_prompt_text(prompt.iter().map(|block| match block {
        acp::ContentBlock::Text(text) => Ok(text.text.trim().to_string()),
        _ => Err(RpcResponseError::invalid_params(
            "session prompt only supports text ACP content blocks",
        )),
    }))
}

async fn dispatch_outbound_call(connection: &acp::AgentSideConnection, call: OutboundClientCall) {
    match call {
        OutboundClientCall::SessionNotification { request, ack } => {
            let _ = ack.send(connection.session_notification(request).await);
        }
        OutboundClientCall::RequestPermission { request, ack } => {
            let _ = ack.send(connection.request_permission(request).await);
        }
        OutboundClientCall::ReadTextFile { request, ack } => {
            let _ = ack.send(connection.read_text_file(request).await);
        }
        OutboundClientCall::WriteTextFile { request, ack } => {
            let _ = ack.send(connection.write_text_file(request).await);
        }
        OutboundClientCall::CreateTerminal { request, ack } => {
            let _ = ack.send(connection.create_terminal(request).await);
        }
        OutboundClientCall::WaitForTerminalExit { request, ack } => {
            let _ = ack.send(connection.wait_for_terminal_exit(request).await);
        }
        OutboundClientCall::TerminalOutput { request, ack } => {
            let _ = ack.send(connection.terminal_output(request).await);
        }
        OutboundClientCall::ReleaseTerminal { request, ack } => {
            let _ = ack.send(connection.release_terminal(request).await);
        }
    }
}

async fn run_probe_official_agent_connection<R, W, A>(
    probe_adapter: A,
    reader: R,
    writer: W,
    mut outbound_receiver: mpsc::UnboundedReceiver<OutboundClientCall>,
) -> Result<usize>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
    A: acp::Agent + 'static,
{
    let (connection, io_task) =
        acp::AgentSideConnection::new(probe_adapter, writer, reader, |future| {
            tokio::task::spawn_local(future);
        });

    tokio::task::spawn_local(async move {
        while let Some(call) = outbound_receiver.recv().await {
            dispatch_outbound_call(&connection, call).await;
        }
    });

    io_task
        .await
        .map_err(|error| FluentCodeError::Provider(format!("ACP connection error: {error}")))
        .map(|_| 0)
}

async fn run_official_agent_server_connection<R, W>(
    server: AcpServer,
    reader: R,
    writer: W,
    outbound_sender: mpsc::UnboundedSender<OutboundClientCall>,
    mut outbound_receiver: mpsc::UnboundedReceiver<OutboundClientCall>,
) -> Result<usize>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let runtime = OfficialAgentRuntime::new(server.clone(), outbound_sender, false);
    let spawn_runtime = runtime.clone();
    let (connection, io_task) = OFFICIAL_AGENT_RUNTIME
        .scope(runtime.clone(), async move {
            acp::AgentSideConnection::new(server, writer, reader, move |future| {
                let runtime = spawn_runtime.clone();
                tokio::task::spawn_local(OFFICIAL_AGENT_RUNTIME.scope(runtime, future));
            })
        })
        .await;

    tokio::task::spawn_local(async move {
        while let Some(call) = outbound_receiver.recv().await {
            dispatch_outbound_call(&connection, call).await;
        }
    });

    OFFICIAL_AGENT_RUNTIME
        .scope(runtime, async move {
            io_task
                .await
                .map_err(|error| {
                    FluentCodeError::Provider(format!("ACP connection error: {error}"))
                })
                .map(|_| 0)
        })
        .await
}

impl BufferedPromptCompletion {
    fn into_result(self) -> acp::Result<acp::PromptResponse> {
        match self {
            Self::Response(response) => Ok(response),
            Self::Error { code, message } => Err(acp::Error::new(code, message)),
        }
    }
}

fn next_official_request_id(connection: &mut AcpConnectionState) -> u64 {
    let request_id = connection.next_request_id;
    connection.next_request_id = connection.next_request_id.checked_add(1).unwrap_or(1);
    request_id
}

fn official_test_probes_enabled() -> bool {
    std::env::var(ACP_TEST_PROBES_ENV_VAR)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn buffered_prompt_completion_from_outbound_frames(
    cancel_request_id: u64,
    outbound_frames: &[OutboundFrame],
) -> acp::Result<Option<BufferedPromptCompletion>> {
    let mut buffered_prompt_completion = None;

    for outbound_frame in outbound_frames {
        let outbound_frame = outbound_frame.to_value().map_err(map_rpc_response_error)?;
        if outbound_frame.get("method").is_some() {
            continue;
        }

        if let Some(error) = outbound_frame.get("error") {
            let frame_id = outbound_frame
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(acp::Error::internal_error)?;
            if frame_id == cancel_request_id {
                continue;
            }

            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or_else(acp::Error::internal_error)? as i32;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(acp::Error::internal_error)?;
            buffered_prompt_completion = Some(BufferedPromptCompletion::Error {
                code,
                message: message.to_string(),
            });
            continue;
        }

        if let Some(result) = outbound_frame.get("result") {
            let frame_id = outbound_frame
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(acp::Error::internal_error)?;
            if frame_id == cancel_request_id {
                continue;
            }

            let prompt_response = serde_json::from_value::<acp::PromptResponse>(result.clone())
                .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
            buffered_prompt_completion = Some(BufferedPromptCompletion::Response(prompt_response));
        }
    }

    Ok(buffered_prompt_completion)
}

fn split_prompt_completion_frames(
    prompt_request_id: Option<u64>,
    outbound_frames: Vec<OutboundFrame>,
) -> (Vec<OutboundFrame>, Vec<OutboundFrame>) {
    let Some(prompt_request_id) = prompt_request_id else {
        return (outbound_frames, Vec::new());
    };

    let mut leading_frames = Vec::new();
    let mut prompt_completion_frames = Vec::new();

    for frame in outbound_frames {
        let frame_id = frame
            .to_value()
            .ok()
            .and_then(|frame| frame.get("id").and_then(Value::as_u64));
        if frame_id == Some(prompt_request_id) {
            prompt_completion_frames.push(frame);
        } else {
            leading_frames.push(frame);
        }
    }

    (leading_frames, prompt_completion_frames)
}

fn current_prompt_turn_root_run_id(state: &AppState) -> Option<RunId> {
    root_run_id_for(state, state.active_run_id?)
}

fn latest_root_run_id(state: &AppState) -> Option<RunId> {
    state
        .session
        .runs
        .iter()
        .filter(|run| run.parent_run_id.is_none())
        .max_by_key(|run| run.latest_replay_sequence())
        .map(|run| run.id)
}

fn prompt_completion_from_terminal_state(
    state: &AppState,
) -> Option<acp::Result<acp::PromptResponse>> {
    let run_id = latest_root_run_id(state)?;
    let run = state.session.find_run(run_id)?;
    let reason = run
        .terminal_stop_reason
        .or_else(|| run.status.default_terminal_stop_reason())?;

    Some(match reason {
        RunTerminalStopReason::Completed => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
        RunTerminalStopReason::Cancelled => {
            Ok(acp::PromptResponse::new(acp::StopReason::Cancelled))
        }
        RunTerminalStopReason::Failed | RunTerminalStopReason::Interrupted => Err(
            map_rpc_response_error(prompt_turn_terminal_error(state, run_id, reason)),
        ),
    })
}

fn root_run_id_for(state: &AppState, mut run_id: RunId) -> Option<RunId> {
    loop {
        let run = state.session.find_run(run_id)?;
        match run.parent_run_id {
            Some(parent_run_id) => run_id = parent_run_id,
            None => return Some(run.id),
        }
    }
}

fn terminalize_cancelled_prompt_turn(
    state: &mut AppState,
    run_id: RunId,
    phase: ForegroundPhase,
    batch_anchor_turn_id: Option<uuid::Uuid>,
) -> bool {
    if !matches!(
        phase,
        ForegroundPhase::AwaitingToolApproval | ForegroundPhase::RunningTool
    ) {
        return false;
    }

    let cancelled_at = state.session.updated_at;
    let mut changed = false;

    for invocation in state
        .session
        .tool_invocations
        .iter_mut()
        .filter(|invocation| {
            invocation.run_id == run_id && invocation.preceding_turn_id == batch_anchor_turn_id
        })
    {
        match invocation.approval_state {
            ToolApprovalState::Pending => {
                invocation.approval_state = ToolApprovalState::Denied;
                invocation.execution_state = ToolExecutionState::Skipped;
                invocation.result = None;
                invocation.error = Some(CANCELLED_TOOL_MESSAGE.to_string());
                invocation.completed_at = Some(cancelled_at);
                changed = true;
            }
            ToolApprovalState::Approved
                if matches!(
                    invocation.execution_state,
                    ToolExecutionState::NotStarted | ToolExecutionState::Running
                ) =>
            {
                invocation.execution_state = ToolExecutionState::Failed;
                invocation.result = None;
                invocation.error = Some(CANCELLED_TOOL_MESSAGE.to_string());
                invocation.completed_at = Some(cancelled_at);
                if let Some(delegation) = invocation.delegation.as_mut()
                    && delegation.status == TaskDelegationStatus::Running
                {
                    delegation.status = TaskDelegationStatus::Cancelled;
                }
                changed = true;
            }
            ToolApprovalState::Denied | ToolApprovalState::Approved => {}
        }
    }

    if changed {
        state.session.updated_at = cancelled_at;
    }

    changed
}

fn permission_reply_for_option_id(
    option_id: &str,
) -> std::result::Result<PermissionReply, RpcResponseError> {
    match option_id {
        "allow_once" => Ok(PermissionReply::Once),
        "allow_always" => Ok(PermissionReply::Always),
        "reject_once" | "reject_always" => Ok(PermissionReply::Deny),
        _ => Err(RpcResponseError::invalid_params(format!(
            "unsupported ACP permission option `{option_id}`"
        ))),
    }
}

fn remember_rejected_permission_rule(
    state: &mut AppState,
    request: &acp::RequestPermissionRequest,
) -> std::result::Result<(), RpcResponseError> {
    let pending_invocation = state
        .session
        .tool_invocations
        .iter()
        .rev()
        .find(|invocation| {
            invocation.approval_state == ToolApprovalState::Pending
                && invocation.tool_call_id == request.tool_call.tool_call_id.to_string()
        })
        .ok_or_else(|| {
            RpcResponseError::invalid_params(format!(
                "session `{}` does not have a pending tool invocation for `{}`",
                request.session_id, request.tool_call.tool_call_id
            ))
        })?;

    let Some(policy) = state
        .tool_registry
        .tool_policy(&pending_invocation.tool_name)
    else {
        return Ok(());
    };

    if can_remember_reply(&policy, PermissionReply::Deny) {
        remember_reply(&mut state.session, &policy, PermissionReply::Deny);
    }

    Ok(())
}

fn decode_params<T: serde::de::DeserializeOwned>(
    params: Value,
) -> std::result::Result<T, RpcResponseError> {
    serde_json::from_value(params)
        .map_err(|error| RpcResponseError::invalid_params(error.to_string()))
}

fn serialize_value<T: Serialize>(value: T) -> std::result::Result<OutboundFrame, RpcResponseError> {
    OutboundFrame::new(
        serde_json::to_string(&value)
            .map_err(|error| RpcResponseError::internal(error.to_string()))?,
    )
}

fn session_update_frame(
    session_id: &str,
    update: SessionUpdate,
) -> std::result::Result<OutboundFrame, RpcResponseError> {
    serialize_value(JsonRpcNotification::new(
        SESSION_UPDATE_METHOD,
        SessionNotification {
            session_id: session_id.to_string(),
            update,
        },
    ))
}

fn permission_request_frame(
    request: &crate::protocol::RequestPermissionRequest,
) -> std::result::Result<OutboundFrame, RpcResponseError> {
    serialize_value(JsonRpcNotification::new(
        SESSION_REQUEST_PERMISSION_METHOD,
        request.clone(),
    ))
}

fn projection_phase_key(phase: ProjectionEventPhase) -> u8 {
    match phase {
        ProjectionEventPhase::UserMessage => 0,
        ProjectionEventPhase::AgentThought => 1,
        ProjectionEventPhase::AgentMessage => 2,
        ProjectionEventPhase::SessionInfoUpdate => 3,
        ProjectionEventPhase::ToolCallCreate => 4,
        ProjectionEventPhase::ToolCallPatch => 5,
        ProjectionEventPhase::PermissionRequest => 6,
    }
}

fn join_prompt_text(
    blocks: impl Iterator<Item = std::result::Result<String, RpcResponseError>>,
) -> std::result::Result<String, RpcResponseError> {
    let prompt = blocks
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    if prompt.is_empty() {
        return Err(RpcResponseError::invalid_params(
            "session prompt must include at least one non-empty text block",
        ));
    }

    Ok(prompt)
}

fn prompt_text_from_blocks(
    prompt: &[ContentBlock],
) -> std::result::Result<String, RpcResponseError> {
    join_prompt_text(prompt.iter().map(|block| match block {
        ContentBlock::Text(text) => Ok(text.text.trim().to_string()),
    }))
}

fn prompt_turn_response(
    state: &AppState,
    run_id: RunId,
    projection: &PromptTurnProjection,
) -> std::result::Result<Option<SessionPromptResponse>, RpcResponseError> {
    match projection.terminal_stop {
        Some(TerminalStopProjection::PromptResponse(stop_reason)) => {
            Ok(Some(SessionPromptResponse { stop_reason }))
        }
        Some(TerminalStopProjection::SessionState(reason)) => {
            Err(prompt_turn_terminal_error(state, run_id, reason))
        }
        None => Ok(None),
    }
}

fn prompt_turn_terminal_error(
    state: &AppState,
    run_id: RunId,
    reason: RunTerminalStopReason,
) -> RpcResponseError {
    let status_message = match &state.status {
        AppStatus::Error(message) => Some(message.as_str()),
        _ => None,
    };
    let message = match reason {
        RunTerminalStopReason::Failed => status_message
            .map(str::to_owned)
            .unwrap_or_else(|| format!("prompt turn `{run_id}` failed")),
        RunTerminalStopReason::Interrupted => {
            status_message.map(str::to_owned).unwrap_or_else(|| {
                format!("prompt turn `{run_id}` was interrupted before it could finish")
            })
        }
        RunTerminalStopReason::Completed | RunTerminalStopReason::Cancelled => {
            format!("prompt turn `{run_id}` ended in unexpected terminal state `{reason:?}`")
        }
    };

    RpcResponseError::internal(message)
}

fn map_rpc_response_error(error: RpcResponseError) -> acp::Error {
    acp::Error::new(error.code, error.message)
}

fn ensure_official_request_order(
    protocol_state: ProtocolState,
    method: Method,
) -> std::result::Result<(), ProtocolError> {
    match (protocol_state, method) {
        (ProtocolState::Uninitialized, Method::Initialize) => Ok(()),
        (ProtocolState::Uninitialized, method) => Err(ProtocolError::InitializeRequired {
            method: method.as_str().to_string(),
        }),
        (ProtocolState::Initialized, Method::Initialize) => {
            Err(ProtocolError::InitializeOutOfOrder)
        }
        (ProtocolState::Initialized, _) => Ok(()),
    }
}

fn map_protocol_error_to_acp_error(error: ProtocolError) -> acp::Error {
    let response = protocol_error_response(Value::Null, error);
    acp::Error::new(response.error.code, response.error.message)
}

fn protocol_error_response(id: Value, error: ProtocolError) -> JsonRpcErrorResponse {
    let (code, message) = match error {
        ProtocolError::MalformedJson(parse_error) => (JSONRPC_PARSE_ERROR, parse_error.to_string()),
        ProtocolError::UnsupportedMethod(method) => (
            JSONRPC_METHOD_NOT_FOUND,
            format!("unsupported JSON-RPC method `{method}`"),
        ),
        ProtocolError::UnsupportedJsonRpcVersion(version) => (
            JSONRPC_INVALID_REQUEST,
            format!("unsupported JSON-RPC version `{version}`"),
        ),
        ProtocolError::InitializeRequired { method } => (
            JSONRPC_INVALID_REQUEST,
            format!("initialize must be the first request, got `{method}`"),
        ),
        ProtocolError::InitializeOutOfOrder => (
            JSONRPC_INVALID_REQUEST,
            "initialize request is out of order".to_string(),
        ),
    };

    JsonRpcErrorResponse::new(id, code, message)
}

#[cfg(test)]
fn request_id_from_frame(frame: &str) -> Value {
    serde_json::from_str::<Value>(frame)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(Value::Null)
}

fn validate_absolute_cwd(cwd: &str) -> std::result::Result<PathBuf, RpcResponseError> {
    let path = PathBuf::from(cwd);
    if path.is_absolute() {
        return Ok(path);
    }

    Err(RpcResponseError::invalid_params(
        "session cwd must be an absolute path",
    ))
}

fn parse_session_id(session_id: &str) -> std::result::Result<SessionId, RpcResponseError> {
    Uuid::parse_str(session_id).map_err(|error| {
        RpcResponseError::invalid_params(format!("invalid session id `{session_id}`: {error}"))
    })
}

fn map_load_session_error(error: FluentCodeError) -> RpcResponseError {
    match error {
        FluentCodeError::Session(message) if message.contains("session metadata not found") => {
            RpcResponseError::resource_not_found(message)
        }
        other => RpcResponseError::internal(other.to_string()),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use agent_client_protocol::{self as acp, Agent as _};
    use chrono::Utc;
    use fluent_code_app::agent::AgentRegistry;
    use fluent_code_app::app::{AppStatus, Msg};
    use fluent_code_app::config::{
        AcpAuthMethodConfig, AcpConfig, AcpSessionDefaultsConfig, Config, LoggingConfig,
        LoggingFileConfig, LoggingStderrConfig, ModelConfig, PluginConfig,
    };
    use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry};
    use fluent_code_app::runtime::Runtime;
    use fluent_code_app::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, RunTerminalStopReason,
        Session, TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolPermissionAction, ToolPermissionRule, ToolPermissionSubject,
        ToolSource, TranscriptItemRecord, TranscriptPermissionState, TranscriptStreamState, Turn,
    };
    use fluent_code_app::session::store::{FsSessionStore, SessionStore};
    use fluent_code_provider::{MockProvider, ProviderClient, ProviderToolCall};
    use serde_json::Value;
    use tokio::io::duplex;
    use tokio::sync::{Mutex, Notify, mpsc};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    use uuid::Uuid;

    use super::{
        ACP_META_LATEST_PROMPT_STATE_KEY, ACP_META_REPLAY_FIDELITY_KEY, AcpConnectionState,
        AcpServer, AcpServerDependencies, CANCELLED_TOOL_MESSAGE, JSONRPC_INTERNAL_ERROR,
        LivePromptTurnState, ManagedAppHost, ManagedSession, OutboundFrame,
        PromptTurnEmissionState, ReaderEvent, wait_for_live_prompt_turn_activity,
    };
    use crate::dev_harness::ScriptedJsonlHarness;
    use crate::protocol::{Method, ParsedRequest};

    #[test]
    fn build_server_scaffold_uses_stdio_transport_and_server_metadata() {
        let temp_dir = unique_temp_dir("fluent-code-acp-build");
        fs::create_dir_all(&temp_dir).unwrap();

        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();

        assert_eq!(server.transport().kind(), "stdio");
        assert_eq!(server.server_info().name, "fluent-code");
        assert_eq!(server.dependencies().config.data_dir, temp_dir);
    }

    #[test]
    fn build_server_scaffold_uses_configured_acp_defaults() {
        let temp_dir = unique_temp_dir("fluent-code-acp-configured-build");
        fs::create_dir_all(&temp_dir).unwrap();
        let mut config = test_config(temp_dir.clone());
        config.acp = AcpConfig {
            protocol_version: 1,
            auth_methods: vec![AcpAuthMethodConfig {
                id: "api_key".to_string(),
                name: "API key".to_string(),
                description: Some("Provide a bearer token.".to_string()),
            }],
            session_defaults: AcpSessionDefaultsConfig {
                system_prompt: "ACP prompt".to_string(),
                reasoning_effort: Some("medium".to_string()),
            },
        };

        let server = AcpServer::build(config).unwrap();
        let response = server.initialize_response(1);

        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.auth_methods.len(), 1);
        assert_eq!(response.auth_methods[0].id, "api_key");

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn initialize_rejects_unsupported_protocol_version_without_initializing_connection() {
        let temp_dir = unique_temp_dir("fluent-code-acp-unsupported-protocol");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut reader = Cursor::new(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":999}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
"#,
        );
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        assert_eq!(responses[0]["error"]["code"], -32602);
        assert_eq!(
            responses[0]["error"]["message"],
            "unsupported ACP protocol version `999`; expected `1`"
        );
        assert_eq!(responses[1]["error"]["code"], -32600);
        assert_eq!(
            responses[1]["error"]["message"],
            "initialize must be the first request, got `session/new`"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn initialize_and_session_new_create_durable_session_with_config_negotiation() {
        let temp_dir = unique_temp_dir("fluent-code-acp-session-new");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut reader = Cursor::new(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"terminal":true}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
"#,
        );
        let mut output = Vec::new();

        let frames_processed = server.serve_frames(&mut reader, &mut output).await.unwrap();
        let responses = output_frames(output);

        assert_eq!(frames_processed, 2);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["protocolVersion"], 1);
        assert_eq!(
            responses[0]["result"]["agentCapabilities"]["loadSession"],
            true
        );
        assert!(responses[0]["result"].get("authMethods").is_none());

        let session_id = responses[1]["result"]["sessionId"]
            .as_str()
            .expect("session id in session/new response");
        assert_eq!(
            responses[1]["result"]["configOptions"][0]["id"],
            "system_prompt"
        );
        assert_eq!(
            responses[1]["result"]["configOptions"][1]["id"],
            "reasoning_effort"
        );
        assert!(responses[1]["result"].get("modes").is_none());

        let store = FsSessionStore::new(temp_dir.clone());
        let persisted = store
            .load(&Uuid::parse_str(session_id).expect("persisted session id should parse"))
            .unwrap();
        assert_eq!(persisted.id.to_string(), session_id);

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn configured_auth_methods_are_advertised_and_gate_session_setup() {
        let temp_dir = unique_temp_dir("fluent-code-acp-auth-flow");
        fs::create_dir_all(&temp_dir).unwrap();
        let mut config = test_config(temp_dir.clone());
        config.acp.auth_methods = vec![AcpAuthMethodConfig {
            id: "api_key".to_string(),
            name: "API key".to_string(),
            description: Some("Provide a bearer token.".to_string()),
        }];
        let server = AcpServer::build(config).unwrap();
        let mut reader = Cursor::new(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
{"jsonrpc":"2.0","id":3,"method":"authenticate","params":{"methodId":"api_key"}}
{"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}
"#,
        );
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        assert_eq!(responses[0]["result"]["authMethods"][0]["id"], "api_key");
        assert_eq!(responses[1]["error"]["code"], -32000);
        assert_eq!(responses[2]["result"], serde_json::json!({}));
        assert!(responses[3]["result"]["sessionId"].is_string());

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_new_rejects_relative_cwd_without_persisting_session() {
        let temp_dir = unique_temp_dir("fluent-code-acp-relative-cwd");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut reader = Cursor::new(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"relative/path","mcpServers":[]}}
"#,
        );
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert_eq!(
            responses[1]["error"]["message"],
            "session cwd must be an absolute path"
        );
        assert!(!temp_dir.join("sessions").exists());

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn scripted_jsonl_harness_captures_stdout_and_persists_evidence() {
        let temp_dir = unique_temp_dir("fluent-code-acp-jsonl-harness");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let harness = ScriptedJsonlHarness::new();
        let script = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\",\"mcpServers\":[]}}\n"
        );

        let capture = harness.run_script(&server, script).await.unwrap();
        let evidence_path = temp_dir.join("session-new.jsonl");
        capture.write_stdout(&evidence_path).unwrap();

        assert_eq!(capture.frames_processed, 2);
        assert_eq!(capture.stdout_frames().len(), 2);
        assert_eq!(
            capture.stdout_frames()[1]["result"]["configOptions"][0]["id"],
            "system_prompt"
        );
        assert_eq!(
            fs::read_to_string(&evidence_path).unwrap(),
            capture.stdout_text()
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_load_replays_persisted_history_in_durable_sequence_order() {
        let temp_dir = unique_temp_dir("fluent-code-acp-session-load");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = persisted_replay_session(&store);
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let load_request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n"
            ),
            session.id
        );
        let mut reader = Cursor::new(load_request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        assert_eq!(responses.len(), 6);
        assert_eq!(responses[1]["method"], "session/update");
        assert_eq!(
            responses[1]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            responses[2]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            responses[3]["params"]["update"]["sessionUpdate"],
            "tool_call"
        );
        assert_eq!(
            responses[4]["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(
            responses[4]["params"]["update"]["rawOutput"]["result"],
            "ordered output"
        );
        assert_eq!(
            responses[5]["result"]["configOptions"][0]["id"],
            "system_prompt"
        );
        assert_eq!(responses[5]["result"]["replayFidelity"], "approximate");
        assert!(responses[5]["result"].get("modes").is_none());

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_load_replays_one_permission_request_for_a_pending_tool_batch() {
        let temp_dir = unique_temp_dir("fluent-code-acp-session-load-permission");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = pending_permission_batch_session(&store);
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let load_request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n"
            ),
            session.id
        );
        let mut reader = Cursor::new(load_request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let tool_calls = responses
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("tool_call"))
            .collect::<Vec<_>>();
        let tool_call_indices = responses
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| {
                (session_update_kind(frame) == Some("tool_call")).then_some(index)
            })
            .collect::<Vec<_>>();
        let permission_requests = responses
            .iter()
            .filter(|frame| frame["method"] == "session/request_permission")
            .collect::<Vec<_>>();
        let permission_request_index = responses
            .iter()
            .position(|frame| frame["method"] == "session/request_permission")
            .expect("single permission request frame");

        assert_eq!(tool_calls.len(), 2);
        assert_eq!(permission_requests.len(), 1);
        assert_eq!(
            responses.last().unwrap()["result"]["replayFidelity"],
            "approximate"
        );
        assert_eq!(
            permission_requests[0]["params"]["toolCall"]["toolCallId"],
            "glob-call-2"
        );
        assert_eq!(
            permission_requests[0]["params"]["toolCall"]["locations"][0]["path"],
            "/tmp/project"
        );
        assert!(
            tool_call_indices
                .iter()
                .all(|index| *index < permission_request_index),
            "expected tool_call replays before the pending permission request"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn replay_preserves_permission_and_tool_boundaries_from_canonical_items() {
        let temp_dir = unique_temp_dir("fluent-code-acp-canonical-replay");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = canonical_exact_replay_session(&store);
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let load_request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n"
            ),
            session.id
        );
        let mut reader = Cursor::new(load_request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let replay_events = responses
            .iter()
            .filter_map(|frame| match frame.get("method").and_then(Value::as_str) {
                Some("session/update") => Some(
                    frame["params"]["update"]["sessionUpdate"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                ),
                Some("session/request_permission") => Some(format!(
                    "request_permission:{}",
                    frame["params"]["toolCall"]["toolCallId"].as_str().unwrap()
                )),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            replay_events,
            vec![
                "user_message_chunk",
                "agent_thought_chunk",
                "agent_message_chunk",
                "tool_call",
                "request_permission:glob-call-1",
                "user_message_chunk",
                "agent_message_chunk",
                "tool_call",
                "tool_call_update",
            ]
        );
        assert_eq!(
            responses.last().unwrap()["result"]["replayFidelity"],
            "exact"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_cancel_cancels_generating_prompt_and_ignores_stale_assistant_updates() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-cancel-generating");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut connection = AcpConnectionState::default();
        let managed_session = managed_session_for(
            server.build_host_for_session(active_generating_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id;
        let run_id = managed_session.host.state().session.runs[0].id;
        connection
            .sessions
            .insert(session_id, Arc::new(Mutex::new(managed_session)));

        let responses = server
            .handle_request(
                &mut connection,
                ParsedRequest {
                    jsonrpc: "2.0".to_string(),
                    id: 1,
                    method: Method::SessionCancel,
                    params: serde_json::json!({ "sessionId": session_id.to_string() }),
                },
            )
            .await
            .unwrap();

        let cancel_frames = response_values(responses);
        assert_eq!(
            cancel_frames.last().unwrap()["result"]["stopReason"],
            "cancelled"
        );

        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .expect("managed session");
        let mut managed_session = managed_session.lock().await;
        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "stale output".to_string(),
            })
            .unwrap();
        managed_session.host.drain_runtime_messages().await.unwrap();

        assert!(matches!(
            managed_session.host.state().status,
            AppStatus::Idle
        ));
        assert!(managed_session.host.state().active_run_id.is_none());
        assert!(
            managed_session
                .host
                .state()
                .session
                .turns
                .iter()
                .all(|turn| !(turn.run_id == run_id
                    && matches!(turn.role, Role::Assistant)
                    && turn.content.contains("stale output")))
        );
        assert_eq!(
            managed_session
                .host
                .state()
                .session
                .find_run(run_id)
                .expect("cancelled run")
                .terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_cancel_during_approval_terminalizes_pending_tools_and_reloads_as_cancelled() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-cancel-approval");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = pending_permission_batch_session(&store);
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}\n"
            ),
            session.id, session.id,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let load_response = responses
            .iter()
            .find(|frame| frame["id"] == 2)
            .expect("load response frame");
        assert_eq!(
            load_response["result"]["latestPromptState"],
            "awaiting_tool_approval"
        );
        let cancelled_updates = responses
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("tool_call_update"))
            .filter(|frame| {
                frame["params"]["update"]["rawOutput"]["error"] == CANCELLED_TOOL_MESSAGE
            })
            .collect::<Vec<_>>();
        assert_eq!(cancelled_updates.len(), 2);
        let cancel_response = responses
            .iter()
            .find(|frame| frame["id"] == 3)
            .expect("cancel response frame");
        assert_eq!(cancel_response["result"]["stopReason"], "cancelled");

        let persisted = store.load(&session.id).unwrap();
        assert!(persisted.tool_invocations.iter().all(|invocation| {
            invocation.approval_state == ToolApprovalState::Denied
                && invocation.execution_state == ToolExecutionState::Skipped
                && invocation.error.as_deref() == Some(CANCELLED_TOOL_MESSAGE)
        }));
        let cancelled_run = persisted
            .runs
            .iter()
            .find(|run| run.parent_run_id.is_none())
            .expect("root run persisted");
        assert_eq!(cancelled_run.status, RunStatus::Cancelled);
        assert_eq!(
            cancelled_run.terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        let reload_request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n"
            ),
            session.id,
        );
        let mut reload_reader = Cursor::new(reload_request.into_bytes());
        let mut reload_output = Vec::new();

        server
            .serve_frames(&mut reload_reader, &mut reload_output)
            .await
            .unwrap();

        let reload_frames = output_frames(reload_output);
        assert_eq!(
            reload_frames.last().unwrap()["result"]["latestPromptState"],
            "cancelled"
        );
        assert_eq!(
            reload_frames
                .iter()
                .filter(|frame| frame["method"] == "session/request_permission")
                .count(),
            0
        );
        assert_eq!(
            reload_frames
                .iter()
                .filter(|frame| session_update_kind(frame) == Some("tool_call_update"))
                .filter(|frame| frame["params"]["update"]["rawOutput"]["error"]
                    == CANCELLED_TOOL_MESSAGE)
                .count(),
            2
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_cancel_during_running_tool_terminalizes_tool_and_ignores_stale_result() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-cancel-running-tool");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut connection = AcpConnectionState::default();
        let managed_session = managed_session_for(
            server.build_host_for_session(running_tool_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id;
        let run_id = managed_session.host.state().session.runs[0].id;
        let invocation_id = managed_session.host.state().session.tool_invocations[0].id;
        connection
            .sessions
            .insert(session_id, Arc::new(Mutex::new(managed_session)));

        let responses = server
            .handle_request(
                &mut connection,
                ParsedRequest {
                    jsonrpc: "2.0".to_string(),
                    id: 1,
                    method: Method::SessionCancel,
                    params: serde_json::json!({ "sessionId": session_id.to_string() }),
                },
            )
            .await
            .unwrap();

        let cancel_frames = response_values(responses);
        let failed_tool_update = cancel_frames
            .iter()
            .find(|frame| {
                session_update_kind(frame) == Some("tool_call_update")
                    && frame["params"]["update"]["rawOutput"]["error"] == CANCELLED_TOOL_MESSAGE
            })
            .expect("failed tool update emitted before cancel response");
        assert_eq!(failed_tool_update["params"]["update"]["status"], "failed");
        let cancel_response = cancel_frames
            .iter()
            .find(|frame| frame["id"] == 1)
            .expect("cancel response frame");
        assert_eq!(cancel_response["result"]["stopReason"], "cancelled");

        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .expect("managed session");
        let mut managed_session = managed_session.lock().await;
        managed_session
            .host
            .runtime_sender()
            .send(Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Ok("late result".to_string()),
            })
            .unwrap();
        managed_session.host.drain_runtime_messages().await.unwrap();

        let invocation = &managed_session.host.state().session.tool_invocations[0];
        assert_eq!(invocation.execution_state, ToolExecutionState::Failed);
        assert_eq!(invocation.error.as_deref(), Some(CANCELLED_TOOL_MESSAGE));
        assert_eq!(invocation.result, None);
        assert_eq!(
            managed_session
                .host
                .state()
                .session
                .find_run(run_id)
                .expect("cancelled run")
                .terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn session_cancel_during_delegated_child_cancels_root_prompt_turn() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-cancel-delegated-child");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut connection = AcpConnectionState::default();
        let managed_session = managed_session_for(
            server.build_host_for_session(active_delegated_child_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id;
        connection
            .sessions
            .insert(session_id, Arc::new(Mutex::new(managed_session)));

        let responses = server
            .handle_request(
                &mut connection,
                ParsedRequest {
                    jsonrpc: "2.0".to_string(),
                    id: 1,
                    method: Method::SessionCancel,
                    params: serde_json::json!({ "sessionId": session_id.to_string() }),
                },
            )
            .await
            .unwrap();

        let cancel_frames = response_values(responses);
        let resumed_tool_update = cancel_frames
            .iter()
            .find(|frame| {
                session_update_kind(frame) == Some("tool_call_update")
                    && frame["params"]["update"]["rawOutput"]["result"]
                        == "Subagent cancelled by user."
            })
            .expect("delegated child cancellation should patch root task invocation");
        assert_eq!(
            resumed_tool_update["params"]["update"]["status"],
            "completed"
        );
        let cancel_response = cancel_frames
            .iter()
            .find(|frame| frame["id"] == 1)
            .expect("cancel response frame");
        assert_eq!(cancel_response["result"]["stopReason"], "cancelled");

        let managed_session = connection
            .sessions
            .get(&session_id)
            .cloned()
            .expect("managed session");
        let managed_session = managed_session.lock().await;
        let root_run = managed_session
            .host
            .state()
            .session
            .runs
            .iter()
            .find(|run| run.parent_run_id.is_none())
            .expect("root run");
        let child_run = managed_session
            .host
            .state()
            .session
            .runs
            .iter()
            .find(|run| run.parent_run_id.is_some())
            .expect("child run");
        assert_eq!(root_run.status, RunStatus::Cancelled);
        assert_eq!(
            root_run.terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );
        assert_eq!(child_run.status, RunStatus::Cancelled);
        assert_eq!(
            managed_session.host.state().session.tool_invocations[0]
                .result
                .as_deref(),
            Some("Subagent cancelled by user.")
        );
        assert_eq!(
            managed_session.host.state().session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Cancelled)
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn poll_live_prompt_turn_streams_foreground_delegated_child_updates_before_completion() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-delegated-child");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_delegated_child_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let child_run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("delegated child owns the foreground")
            .run_id;

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantReasoningChunk {
                run_id: child_run_id,
                delta: "child reasoning".to_string(),
            })
            .unwrap();
        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "child answer".to_string(),
            })
            .unwrap();
        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantToolCall {
                run_id: child_run_id,
                tool_call: ProviderToolCall {
                    id: "read-call-2".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path":"/tmp/child.txt"}),
                },
            })
            .unwrap();

        let poll_result = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let session_updates = session_update_frames(&poll_result.frames);
        let poll_frames = response_values(poll_result.frames.clone());
        let tool_call_frame_index = poll_result
            .frames
            .iter()
            .zip(poll_frames.iter())
            .position(|(_, frame)| session_update_kind(frame) == Some("tool_call"))
            .expect("delegated child tool call frame");
        let permission_request_frame_index = poll_result
            .frames
            .iter()
            .zip(poll_frames.iter())
            .position(|(_, frame)| frame["method"] == "session/request_permission")
            .expect("delegated child permission request frame");

        assert!(!poll_result.prompt_turn_complete);
        assert_eq!(session_updates.len(), 3);
        assert_eq!(
            session_update_kind(&session_updates[0]),
            Some("agent_thought_chunk")
        );
        assert_eq!(
            session_updates[0]["params"]["update"]["content"]["text"],
            "child reasoning"
        );
        assert_eq!(
            session_update_kind(&session_updates[1]),
            Some("agent_message_chunk")
        );
        assert_eq!(
            session_updates[1]["params"]["update"]["content"]["text"],
            "child answer"
        );
        assert_eq!(session_update_kind(&session_updates[2]), Some("tool_call"));
        assert_eq!(
            session_updates[2]["params"]["update"]["toolCallId"],
            "read-call-2"
        );
        assert!(tool_call_frame_index < permission_request_frame_index);

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn poll_live_prompt_turn_resumes_foreground_delegated_child_agent_chunks_without_duplicate_text()
     {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-delegated-child-resume");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_delegated_child_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let child_run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("delegated child owns the foreground")
            .run_id;

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "delegated partial ".to_string(),
            })
            .unwrap();

        let first_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let first_updates = session_update_frames(&first_poll.frames);

        assert!(!first_poll.prompt_turn_complete);
        assert_eq!(
            collect_agent_message_chunk_texts(&first_updates),
            vec!["delegated partial "]
        );

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "resumed output".to_string(),
            })
            .unwrap();

        let second_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let second_updates = session_update_frames(&second_poll.frames);

        assert!(!second_poll.prompt_turn_complete);
        assert_eq!(
            collect_agent_message_chunk_texts(&second_updates),
            vec!["resumed output"]
        );
        assert_eq!(
            managed_session
                .host
                .state()
                .session
                .turns
                .iter()
                .find(|turn| turn.run_id == child_run_id && matches!(turn.role, Role::Assistant))
                .expect("delegated child assistant turn")
                .content,
            "delegated partial resumed output"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn live_projection_watermark_matches_full_projection_for_monotonic_updates() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-watermark-parity");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_generating_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("active generating session owns the foreground")
            .run_id;

        let baseline_replay = server
            .session_load_replay_frames(&session_id, &managed_session.host)
            .unwrap();

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantReasoningChunk {
                run_id,
                delta: "thinking ".to_string(),
            })
            .unwrap();
        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "hello ".to_string(),
            })
            .unwrap();

        let first_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantReasoningChunk {
                run_id,
                delta: "more".to_string(),
            })
            .unwrap();
        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "world".to_string(),
            })
            .unwrap();

        let second_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let final_replay = server
            .session_load_replay_frames(&session_id, &managed_session.host)
            .unwrap();
        let mut cumulative_live = baseline_replay.clone();
        cumulative_live.extend(first_poll.frames.clone());
        cumulative_live.extend(second_poll.frames.clone());

        assert!(!first_poll.prompt_turn_complete);
        assert!(!second_poll.prompt_turn_complete);
        assert_eq!(
            collect_chunk_texts_by_kind(&first_poll.frames, "agent_thought_chunk"),
            vec!["thinking ".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&first_poll.frames, "agent_message_chunk"),
            vec!["hello ".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&second_poll.frames, "agent_thought_chunk"),
            vec!["more".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&second_poll.frames, "agent_message_chunk"),
            vec!["world".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&cumulative_live, "user_message_chunk").concat(),
            collect_chunk_texts_by_kind(&final_replay, "user_message_chunk").concat()
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&cumulative_live, "agent_thought_chunk").concat(),
            collect_chunk_texts_by_kind(&final_replay, "agent_thought_chunk").concat()
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&cumulative_live, "agent_message_chunk").concat(),
            collect_chunk_texts_by_kind(&final_replay, "agent_message_chunk").concat()
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    async fn live_projection_falls_back_to_full_projection_on_stream_reopen() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-stream-reopen");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_generating_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("active generating session owns the foreground")
            .run_id;

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "partial ".to_string(),
            })
            .unwrap();
        let initial_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "answer".to_string(),
            })
            .unwrap();
        let continued_frames = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let reopened_frames = server
            .session_load_replay_frames(&session_id, &managed_session.host)
            .unwrap();

        assert_eq!(
            collect_chunk_texts_by_kind(&initial_poll.frames, "agent_message_chunk"),
            vec!["partial ".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&continued_frames.frames, "agent_message_chunk"),
            vec!["answer".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&reopened_frames, "user_message_chunk"),
            vec!["resume me".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&reopened_frames, "agent_message_chunk"),
            vec!["partial answer".to_string()]
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn live_projection_empty_poll_does_not_emit_duplicate_text() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-empty-poll");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_generating_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("active generating session owns the foreground")
            .run_id;

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "partial ".to_string(),
            })
            .unwrap();

        let first_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let empty_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();

        assert!(!first_poll.prompt_turn_complete);
        assert!(!empty_poll.prompt_turn_complete);
        assert_eq!(
            collect_chunk_texts_by_kind(&first_poll.frames, "agent_message_chunk"),
            vec!["partial ".to_string()]
        );
        assert!(empty_poll.frames.is_empty());

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn replay_projection_ignores_live_watermark_state() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-replay-watermark-fallback");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let mut managed_session = managed_session_for(
            server.build_host_for_session(active_generating_session()),
            &server,
        );
        let session_id = managed_session.host.state().session.id.to_string();
        let run_id = managed_session
            .host
            .state()
            .session
            .foreground_owner
            .as_ref()
            .expect("active generating session owns the foreground")
            .run_id;

        managed_session
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id,
                delta: "live partial ".to_string(),
            })
            .unwrap();

        let live_poll = server
            .poll_live_prompt_turn(&session_id, &mut managed_session)
            .await
            .unwrap();
        let replay_frames = server
            .session_load_replay_frames(&session_id, &managed_session.host)
            .unwrap();

        assert_eq!(
            collect_chunk_texts_by_kind(&live_poll.frames, "agent_message_chunk"),
            vec!["live partial ".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&replay_frames, "user_message_chunk"),
            vec!["resume me".to_string()]
        );
        assert_eq!(
            collect_chunk_texts_by_kind(&replay_frames, "agent_message_chunk"),
            vec!["live partial ".to_string()]
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_load_surfaces_interrupted_running_tool_state_explicitly() {
        let temp_dir = unique_temp_dir("fluent-code-acp-load-interrupted-running-tool");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = running_tool_session();
        let session_id = session.id;
        store.create(&session).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n"
            ),
            session_id,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let interrupted_update = responses
            .iter()
            .find(|frame| {
                session_update_kind(frame) == Some("tool_call_update")
                    && frame["params"]["update"]["rawOutput"]["error"]
                        == "Tool execution was interrupted during restart recovery."
            })
            .expect("interrupted tool failure should be replayed");
        assert_eq!(interrupted_update["params"]["update"]["status"], "failed");
        assert_eq!(
            responses.last().unwrap()["result"]["latestPromptState"],
            "interrupted"
        );
        assert_eq!(
            responses.last().unwrap()["result"]["_meta"][ACP_META_LATEST_PROMPT_STATE_KEY],
            "interrupted"
        );
        assert_eq!(
            responses.last().unwrap()["result"]["_meta"][ACP_META_REPLAY_FIDELITY_KEY],
            "approximate"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn live_stdio_session_cancel_interrupts_active_prompt_on_same_connection() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-cancel");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("live cancel session");
        let session_id = session.id;
        store.create(&session).unwrap();

        let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(50));
        let (frame_sender, mut frame_receiver) = mpsc::unbounded_channel();
        let mut output = NotifyingFrameCapture::default();
        let output_for_sender = output.clone();
        let sender = tokio::spawn(async move {
            frame_sender
                .send(ReaderEvent::Frame(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
                        .to_string(),
                ))
                .unwrap();
            frame_sender
                .send(ReaderEvent::Frame(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}",
                    session_id,
                )))
                .unwrap();
            frame_sender
                .send(ReaderEvent::Frame(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"interrupt this prompt\"}}]}}}}",
                    session_id,
                )))
                .unwrap();

            wait_for_capture_agent_chunk(&output_for_sender).await;

            frame_sender
                .send(ReaderEvent::Frame(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}",
                    session_id,
                )))
                .unwrap();

            frame_sender.send(ReaderEvent::Eof).unwrap();
        });

        let frames_processed = server
            .serve_live_frames(&mut frame_receiver, &mut output)
            .await
            .unwrap();
        sender.await.unwrap();

        let responses = output.output_frames();
        let cancel_response_index = responses
            .iter()
            .position(|frame| frame["id"] == 4)
            .expect("session/cancel response frame");
        let prompt_response_index = responses
            .iter()
            .position(|frame| frame["id"] == 3)
            .expect("session/prompt response frame");

        assert_eq!(frames_processed, 4);
        assert_eq!(
            responses[cancel_response_index]["result"]["stopReason"],
            "cancelled"
        );
        assert_eq!(
            responses[prompt_response_index]["result"]["stopReason"],
            "cancelled"
        );
        assert!(
            cancel_response_index < prompt_response_index,
            "cancel response should arrive before prompt completion response"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn live_session_new_cancel_keeps_prompt_request_resolvable() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-session-new-cancel");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(50));
        let mut connection = AcpConnectionState::default();

        server
            .handle_live_frame(
                &mut connection,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            )
            .await
            .unwrap();
        let new_session_frames = server
            .handle_live_frame(
                &mut connection,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\",\"mcpServers\":[]}}}}",
                    temp_dir.display(),
                ),
            )
            .await
            .unwrap();
        let new_session_frames = response_values(new_session_frames);
        let session_id = new_session_frames[0]["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        server
            .handle_live_frame(
                &mut connection,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"interrupt this prompt\"}}]}}}}",
                    session_id,
                ),
            )
            .await
            .unwrap();

        let cancel_frames = server
            .handle_live_frame(
                &mut connection,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}",
                    session_id,
                ),
            )
            .await
            .unwrap();
        let cancel_frames = response_values(cancel_frames);

        let cancel_ids = cancel_frames
            .iter()
            .filter_map(|frame| frame.get("id").and_then(Value::as_u64))
            .collect::<Vec<_>>();

        assert!(cancel_ids.contains(&4));
        assert!(
            cancel_ids.contains(&3)
                || connection.sessions.values().any(|managed_session| {
                    managed_session
                        .try_lock()
                        .expect("test harness should not contend on managed sessions")
                        .pending_prompt_request_id
                        == Some(3)
                }),
            "cancel path must either emit or retain the original prompt response",
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn live_prompt_wait_path_stays_idle_until_runtime_activity() {
        let temp_dir = unique_temp_dir("fluent-code-acp-live-wait-idle");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let session = active_generating_session();
        let root_run_id = session
            .foreground_owner
            .as_ref()
            .expect("active generating session should retain a foreground owner")
            .run_id;
        let managed_session = Arc::new(Mutex::new(ManagedSession {
            cwd: temp_dir.clone(),
            mcp_servers: Vec::new(),
            host: server.build_host_for_session(session),
            live_prompt_turn: Some(LivePromptTurnState {
                root_run_id,
                emission_state: PromptTurnEmissionState::default(),
            }),
            pending_prompt_request_id: Some(1),
            buffered_prompt_completion: None,
        }));

        let wait_task = tokio::spawn(wait_for_live_prompt_turn_activity(vec![Arc::clone(
            &managed_session,
        )]));
        tokio::task::yield_now().await;
        assert!(
            !wait_task.is_finished(),
            "expected the live ACP prompt wait path to remain idle until runtime activity arrives"
        );

        managed_session
            .lock()
            .await
            .host
            .runtime_sender()
            .send(Msg::AssistantChunk {
                run_id: root_run_id,
                delta: "wake".to_string(),
            })
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(100), wait_task)
                .await
                .expect("live ACP wait path should wake after runtime activity")
                .expect("live ACP wait task should join cleanly"),
            "expected runtime activity to wake the live ACP wait path"
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn official_sdk_test_probes_are_disabled_by_default() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-official-test-probes-disabled");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();

        tokio::task::LocalSet::new()
            .run_until(async move {
                let (server_stream, client_stream) = duplex(64 * 1024);
                let (server_reader, server_writer) = tokio::io::split(server_stream);
                let server_task = {
                    let server = server.clone();
                    tokio::task::spawn_local(async move {
                        server
                            .serve_agent_connection(
                                server_reader.compat(),
                                server_writer.compat_write(),
                            )
                            .await
                    })
                };

                let (client_reader, client_writer) = tokio::io::split(client_stream);
                let (connection, io_future) = acp::ClientSideConnection::new(
                    RecordingClient::default(),
                    client_writer.compat_write(),
                    client_reader.compat(),
                    |future| {
                        tokio::task::spawn_local(future);
                    },
                );
                let connection = Arc::new(connection);
                let io_task = tokio::task::spawn_local(io_future);

                connection
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
                    .await
                    .unwrap();

                let params = serde_json::value::to_raw_value(&serde_json::json!({})).unwrap();
                let error = connection
                    .ext_method(acp::ExtRequest::new(
                        "fluent_code/test/read_text_file".to_string(),
                        params.into(),
                    ))
                    .await
                    .expect_err("production official runtime should reject ACP test probes");
                assert_eq!(error.code, acp::ErrorCode::MethodNotFound);

                drop(connection);
                io_task.abort();
                let _ = io_task.await;
                server_task.await.unwrap().unwrap();
            })
            .await;

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn official_sdk_same_connection_cancel_unblocks_live_prompt_and_preserves_streaming() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-official-live-cancel");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("official live cancel session");
        let session_id = session.id.to_string();
        store.create(&session).unwrap();

        let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(50));
        let test_cwd = temp_dir.clone();
        let test_session_id = session_id.clone();

        tokio::task::LocalSet::new()
            .run_until(async move {
                let (server_stream, client_stream) = duplex(64 * 1024);
                let (server_reader, server_writer) = tokio::io::split(server_stream);
                let server_task = {
                    let server = server.clone();
                    tokio::task::spawn_local(async move {
                        server
                            .serve_agent_connection(
                                server_reader.compat(),
                                server_writer.compat_write(),
                            )
                            .await
                    })
                };

                let client = RecordingClient::default();
                let agent_chunk_count = Arc::clone(&client.agent_chunk_count);
                let chunk_notifications = client.clone();
                let (client_reader, client_writer) = tokio::io::split(client_stream);
                let (connection, io_future) = acp::ClientSideConnection::new(
                    client,
                    client_writer.compat_write(),
                    client_reader.compat(),
                    |future| {
                        tokio::task::spawn_local(future);
                    },
                );
                let connection = Arc::new(connection);
                let io_task = tokio::task::spawn_local(io_future);

                connection
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
                    .await
                    .unwrap();
                connection
                    .load_session(acp::LoadSessionRequest::new(
                        test_session_id.clone(),
                        test_cwd.clone(),
                    ))
                    .await
                    .unwrap();

                let prompt_connection = Arc::clone(&connection);
                let prompt_session_id = test_session_id.clone();
                let prompt_task = tokio::task::spawn_local(async move {
                    prompt_connection
                        .prompt(acp::PromptRequest::new(
                            prompt_session_id,
                            vec![acp::ContentBlock::Text(acp::TextContent::new(
                                "interrupt this prompt over the official SDK path",
                            ))],
                        ))
                        .await
                });

                wait_for_agent_chunk(&chunk_notifications).await;
                assert!(
                    !prompt_task.is_finished(),
                    "prompt should still be active after the first streamed agent chunk"
                );

                connection
                    .cancel(acp::CancelNotification::new(test_session_id))
                    .await
                    .unwrap();

                let prompt_response = prompt_task.await.unwrap().unwrap();
                assert_eq!(prompt_response.stop_reason, acp::StopReason::Cancelled);
                assert!(agent_chunk_count.load(Ordering::SeqCst) > 0);

                drop(connection);
                io_task.abort();
                let _ = io_task.await;
                server_task.await.unwrap().unwrap();
            })
            .await;

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn permission_notification_routing_preserves_follow_up_order() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-official-permission-order");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("official prompt permission ordering session");
        let session_id = session.id.to_string();
        store.create(&session).unwrap();

        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let test_cwd = temp_dir.clone();
        let test_session_id = session_id.clone();

        tokio::task::LocalSet::new()
            .run_until(async move {
                let (server_stream, client_stream) = duplex(64 * 1024);
                let (server_reader, server_writer) = tokio::io::split(server_stream);
                let server_task = {
                    let server = server.clone();
                    tokio::task::spawn_local(async move {
                        server
                            .serve_agent_connection(
                                server_reader.compat(),
                                server_writer.compat_write(),
                            )
                            .await
                    })
                };

                let client = PermissionOrderingClient::default();
                let client_events = client.clone();
                let (client_reader, client_writer) = tokio::io::split(client_stream);
                let (connection, io_future) = acp::ClientSideConnection::new(
                    client,
                    client_writer.compat_write(),
                    client_reader.compat(),
                    |future| {
                        tokio::task::spawn_local(future);
                    },
                );
                let connection = Arc::new(connection);
                let io_task = tokio::task::spawn_local(io_future);

                connection
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
                    .await
                    .unwrap();
                connection
                    .load_session(acp::LoadSessionRequest::new(
                        test_session_id.clone(),
                        test_cwd.clone(),
                    ))
                    .await
                    .unwrap();

                let prompt_response = connection
                    .prompt(acp::PromptRequest::new(
                        test_session_id,
                        vec![acp::ContentBlock::Text(acp::TextContent::new(
                            "use uppercase_text: hello world",
                        ))],
                    ))
                    .await
                    .unwrap();
                assert_eq!(prompt_response.stop_reason, acp::StopReason::EndTurn);
                let expected_resumed_response = "Mock assistant response after tool: HELLO WORLD";

                wait_for_client_event(
                    &client_events,
                    "resumed assistant output after permission approval",
                    |events| {
                        collect_agent_message_chunk_texts_from_events(events)
                            .join("")
                            .contains(expected_resumed_response)
                    },
                )
                .await;
                let events = client_events.snapshot_events();
                let permission_index = events
                    .iter()
                    .position(|event| event.starts_with("request_permission:"))
                    .expect("permission request event");
                let completed_tool_update_index = events
                    .iter()
                    .position(|event| event.ends_with(":completed:HELLO WORLD"))
                    .expect("completed tool update event");
                let resumed_chunk_index = events
                    .iter()
                    .enumerate()
                    .find_map(|(index, event)| {
                        (index > completed_tool_update_index
                            && event.starts_with("agent_message_chunk:"))
                        .then_some(index)
                    })
                    .expect("resumed assistant chunk event after tool completion");
                let resumed_chunk_text = collect_agent_message_chunk_texts_from_events(
                    &events[completed_tool_update_index + 1..],
                )
                .join("");

                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event.starts_with("request_permission:"))
                        .count(),
                    1,
                    "expected a single permission request before wake-driven resume"
                );
                assert!(
                    permission_index < completed_tool_update_index,
                    "expected permission approval to precede the completed tool patch\nevents: {events:?}"
                );
                assert!(
                    completed_tool_update_index < resumed_chunk_index,
                    "expected tool completion to precede resumed assistant output\nevents: {events:?}"
                );
                assert_eq!(
                    resumed_chunk_text, expected_resumed_response,
                    "expected resumed assistant chunks after approval to reconstruct the post-tool response\nevents: {events:?}"
                );

                drop(connection);
                io_task.abort();
                let _ = io_task.await;
                server_task.await.unwrap().unwrap();
            })
            .await;

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_prompt_streams_text_updates_until_terminal_stop_reason() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-session-prompt-text");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("text prompt session");
        store.create(&session).unwrap();

        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"inspect this prompt\"}}]}}}}\n"
            ),
            session.id, session.id,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let session_updates = session_update_frames(&responses);
        assert_eq!(
            responses.last().unwrap()["result"]["stopReason"],
            "end_turn"
        );
        assert!(!session_updates.is_empty());
        assert_eq!(
            session_update_kind(&session_updates[0]),
            Some("user_message_chunk")
        );
        assert!(
            session_updates[1..]
                .iter()
                .all(|frame| session_update_kind(frame) == Some("agent_message_chunk"))
        );
        assert_eq!(
            collect_agent_message_chunks(&session_updates),
            "Mock assistant response: inspect this prompt"
        );

        let persisted = store.load(&session.id).unwrap();
        let root_runs = persisted
            .runs
            .iter()
            .filter(|run| run.parent_run_id.is_none())
            .collect::<Vec<_>>();
        assert_eq!(root_runs.len(), 1);
        assert_eq!(root_runs[0].status, RunStatus::Completed);
        assert_eq!(
            root_runs[0].terminal_stop_reason,
            Some(RunTerminalStopReason::Completed)
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_prompt_flushes_pending_agent_delta_before_terminal_stop_reason() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-session-prompt-terminal-flush");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("terminal flush prompt session");
        store.create(&session).unwrap();

        let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(4));
        let prompt_text = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen";
        let expected_response = format!("Mock assistant response: {prompt_text}");
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n"
            ),
            session.id, session.id, prompt_text,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let prompt_response_index = responses
            .iter()
            .position(|frame| frame["id"] == 3)
            .expect("session/prompt response frame");
        let session_updates = session_update_frames(&responses);
        let agent_chunks = collect_agent_message_chunk_texts(&session_updates);
        let emitted_prefix_length = agent_chunks[..agent_chunks.len() - 1]
            .iter()
            .map(String::len)
            .sum::<usize>();

        assert_eq!(
            responses[prompt_response_index]["result"]["stopReason"],
            "end_turn"
        );
        assert!(agent_chunks.len() >= 2);
        assert_eq!(
            collect_agent_message_chunks(&session_updates),
            expected_response
        );
        assert_eq!(
            session_update_kind(&responses[prompt_response_index - 1]),
            Some("agent_message_chunk")
        );
        assert!(emitted_prefix_length > 0);
        assert!(emitted_prefix_length < expected_response.len());
        assert_eq!(
            agent_chunks.last().unwrap(),
            &expected_response[emitted_prefix_length..]
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_prompt_preserves_many_chunk_continuity_without_duplicate_text() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-session-prompt-chunk-continuity");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = Session::new("chunk continuity prompt session");
        store.create(&session).unwrap();

        let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(15));
        let prompt_text = "alpha  beta   gamma    delta     epsilon      zeta";
        let expected_response = format!("Mock assistant response: {prompt_text}");
        let expected_chunks = split_text_like_mock_provider(&expected_response);
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n"
            ),
            session.id, session.id, prompt_text,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let session_updates = session_update_frames(&responses);
        let agent_chunks = collect_agent_message_chunk_texts(&session_updates);

        assert_eq!(
            responses.last().unwrap()["result"]["stopReason"],
            "end_turn"
        );
        assert_eq!(agent_chunks, expected_chunks);
        assert_eq!(agent_chunks.concat(), expected_response);
        assert_eq!(
            collect_agent_message_chunks(&session_updates),
            expected_response
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_prompt_streams_tool_lifecycle_updates_in_projection_order() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-session-prompt-tool");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let mut session = Session::new("tool prompt session");
        session.remember_tool_permission_rule(ToolPermissionRule {
            subject: ToolPermissionSubject::from_tool("uppercase_text", &ToolSource::BuiltIn),
            action: ToolPermissionAction::Allow,
        });
        store.create(&session).unwrap();

        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"use uppercase_text: hello world\"}}]}}}}\n"
            ),
            session.id, session.id,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let session_updates = session_update_frames(&responses);
        let update_kinds = session_updates
            .iter()
            .filter_map(|frame| session_update_kind(frame))
            .collect::<Vec<_>>();

        assert_eq!(
            responses.last().unwrap()["result"]["stopReason"],
            "end_turn"
        );
        assert_eq!(update_kinds[0], "user_message_chunk");

        let tool_call_index = update_kinds
            .iter()
            .position(|kind| *kind == "tool_call")
            .expect("tool_call update should be emitted");
        let first_tool_update_index = update_kinds
            .iter()
            .position(|kind| *kind == "tool_call_update")
            .expect("tool_call_update should be emitted");
        let resumed_agent_chunks = session_updates[first_tool_update_index + 2..]
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("agent_message_chunk"))
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect::<Vec<_>>();
        assert!(tool_call_index > 0);
        assert!(first_tool_update_index > tool_call_index);
        assert_eq!(
            session_updates[first_tool_update_index]["params"]["update"]["status"],
            "in_progress"
        );
        assert_eq!(
            session_updates[first_tool_update_index + 1]["params"]["update"]["status"],
            "completed"
        );
        assert_eq!(
            session_updates[first_tool_update_index + 1]["params"]["update"]["rawOutput"]["result"],
            "HELLO WORLD"
        );
        assert!(
            update_kinds[first_tool_update_index + 2..]
                .iter()
                .all(|kind| *kind == "agent_message_chunk")
        );
        assert!(!resumed_agent_chunks.is_empty());
        assert_eq!(
            resumed_agent_chunks.concat(),
            "Mock assistant response after tool: HELLO WORLD"
        );
        assert!(
            collect_agent_message_chunks(&session_updates)
                .contains("Mock assistant response after tool: HELLO WORLD")
        );

        let persisted = store.load(&session.id).unwrap();
        let invocation = persisted
            .tool_invocations
            .last()
            .expect("tool invocation persisted after prompt turn");
        assert_eq!(invocation.tool_name, "uppercase_text");
        assert_eq!(invocation.execution_state, ToolExecutionState::Completed);
        assert_eq!(invocation.result.as_deref(), Some("HELLO WORLD"));

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn session_prompt_rejects_when_session_already_has_active_prompt_turn() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-session-prompt-active");
        fs::create_dir_all(&temp_dir).unwrap();
        let store = FsSessionStore::new(temp_dir.clone());
        let session = active_generating_session();
        let session_id = session.id;
        store.create(&session).unwrap();

        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
        let request = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"/tmp\",\"mcpServers\":[]}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"second prompt\"}}]}}}}\n"
            ),
            session_id, session_id,
        );
        let mut reader = Cursor::new(request.into_bytes());
        let mut output = Vec::new();

        server.serve_frames(&mut reader, &mut output).await.unwrap();

        let responses = output_frames(output);
        let error_response = responses
            .iter()
            .find(|frame| frame.get("error").is_some())
            .expect("active prompt rejection response");
        assert_eq!(error_response["error"]["code"], -32003);
        assert!(
            error_response["error"]["message"]
                .as_str()
                .expect("active prompt rejection message")
                .contains("already has an active prompt turn")
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn headless_host_completes_prompt_lifecycle_and_persists_session() {
        let _guard = test_lock().lock().await;
        let root = unique_temp_dir("fluent-code-acp-headless-prompt");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::with_chunk_delay(Duration::from_millis(10))),
            Arc::clone(&tool_registry),
        );
        let mut host = ManagedAppHost::new(
            Session::new("headless prompt lifecycle"),
            store.clone(),
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.submit_prompt("headless host prompt").await.unwrap();

        let completion_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < completion_deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            host.drain_runtime_messages().await.unwrap();

            if matches!(host.state().status, AppStatus::Idle) {
                break;
            }
        }

        assert!(matches!(host.state().status, AppStatus::Idle));
        assert!(host.state().active_run_id.is_none());

        let persisted = store.load(&host.state().session.id).unwrap();
        let assistant_turn = persisted
            .turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn after headless prompt lifecycle");
        assert_eq!(
            assistant_turn.content,
            "Mock assistant response: headless host prompt"
        );

        cleanup(root);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn headless_host_drains_queued_runtime_messages_in_order() {
        let _guard = test_lock().lock().await;
        let root = unique_temp_dir("fluent-code-acp-runtime-drain");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let run_id = Uuid::new_v4();
        let mut session = Session::new("queued runtime drain");
        session.upsert_run(run_id, RunStatus::InProgress);
        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "queued message".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });

        let mut host = ManagedAppHost::new(
            session,
            store.clone(),
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );
        host.state_mut()
            .set_foreground(run_id, ForegroundPhase::Generating, None);

        let runtime_sender = host.runtime_sender();
        runtime_sender
            .send(Msg::AssistantChunk {
                run_id,
                delta: "queued ".to_string(),
            })
            .unwrap();
        runtime_sender
            .send(Msg::AssistantChunk {
                run_id,
                delta: "message".to_string(),
            })
            .unwrap();
        runtime_sender.send(Msg::AssistantDone { run_id }).unwrap();

        host.drain_runtime_messages().await.unwrap();

        assert!(matches!(host.state().status, AppStatus::Idle));
        assert!(host.state().active_run_id.is_none());

        let persisted = store.load(&host.state().session.id).unwrap();
        let assistant_turn = persisted
            .turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn persisted after queued drain");
        assert_eq!(assistant_turn.content, "queued message");

        cleanup(root);
    }

    #[tokio::test]
    #[ignore = "slow (>10s)"]
    async fn headless_host_recovery_fails_closed_for_running_tool_owner() {
        let _guard = test_lock().lock().await;
        let root = unique_temp_dir("fluent-code-acp-running-tool-recovery");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let mut host = ManagedAppHost::new(
            running_tool_session(),
            store,
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.recover_startup().await.unwrap();

        assert!(
            matches!(host.state().status, AppStatus::Error(ref message) if message.contains("refuses to guess how to resume running tools"))
        );
        assert!(host.state().active_run_id.is_none());
        assert!(host.state().session.foreground_owner.is_none());
        assert_eq!(
            host.state().session.tool_invocations[0].execution_state,
            ToolExecutionState::Failed
        );
        assert_eq!(
            host.state().session.tool_invocations[0].error.as_deref(),
            Some("Tool execution was interrupted during restart recovery.")
        );

        cleanup(root);
    }

    fn running_tool_session() -> Session {
        let mut session = Session::new("running tool recovery");
        let run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        let run_created_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: run_created_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "read the file".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });
        let assistant_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "I will read the file".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_sequence_number,
            timestamp: Utc::now(),
        });
        let invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "read-call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: None,
            sequence_number: invocation_sequence_number,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::RunningTool,
            batch_anchor_turn_id: Some(assistant_turn_id),
        });
        session.rebuild_run_indexes();
        session
    }

    fn active_generating_session() -> Session {
        let mut session = Session::new("active generating prompt");
        let run_id = Uuid::new_v4();
        session.upsert_run(run_id, RunStatus::InProgress);
        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "resume me".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::Generating,
            batch_anchor_turn_id: None,
        });
        session
    }

    fn persisted_replay_session(store: &FsSessionStore) -> Session {
        let mut session = Session::new("ordered replay session");
        let run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        let shared_timestamp = Utc::now();
        session.upsert_run(run_id, RunStatus::InProgress);

        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "replay this session".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: shared_timestamp + chrono::Duration::seconds(10),
        });

        let assistant_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "ordered answer".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_sequence_number,
            timestamp: shared_timestamp - chrono::Duration::seconds(20),
        });

        let invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "read-call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "/tmp/notes.txt" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("ordered output".to_string()),
            error: None,
            delegation: None,
            sequence_number: invocation_sequence_number,
            requested_at: shared_timestamp - chrono::Duration::seconds(30),
            approved_at: Some(shared_timestamp - chrono::Duration::seconds(29)),
            completed_at: Some(shared_timestamp - chrono::Duration::seconds(28)),
        });

        session.upsert_run_with_stop_reason(run_id, RunStatus::Completed, None);
        store.create(&session).unwrap();
        session
    }

    fn canonical_exact_replay_session(store: &FsSessionStore) -> Session {
        let mut session = Session::new("canonical exact replay session");
        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let root_assistant_turn_id = Uuid::new_v4();
        let child_assistant_turn_id = Uuid::new_v4();
        let now = Utc::now();

        let root_user_turn = Turn {
            id: Uuid::new_v4(),
            run_id: root_run_id,
            role: Role::User,
            content: "inspect and delegate".to_string(),
            reasoning: String::new(),
            sequence_number: 41,
            timestamp: now,
        };
        let root_assistant_turn = Turn {
            id: root_assistant_turn_id,
            run_id: root_run_id,
            role: Role::Assistant,
            content: "I will inspect the repo.".to_string(),
            reasoning: "plan first".to_string(),
            sequence_number: 42,
            timestamp: now,
        };
        let child_user_turn = Turn {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::User,
            content: "inspect child state".to_string(),
            reasoning: String::new(),
            sequence_number: 43,
            timestamp: now,
        };
        let child_assistant_turn = Turn {
            id: child_assistant_turn_id,
            run_id: child_run_id,
            role: Role::Assistant,
            content: "Child summary".to_string(),
            reasoning: String::new(),
            sequence_number: 44,
            timestamp: now,
        };

        let mut pending_invocation = ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id: root_run_id,
            tool_call_id: "glob-call-1".to_string(),
            tool_name: "glob".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "pattern": "**/*.rs", "path": "/tmp/project" }),
            preceding_turn_id: Some(root_assistant_turn_id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            delegation: None,
            sequence_number: 60,
            requested_at: now,
            approved_at: None,
            completed_at: None,
        };
        let child_invocation = ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            tool_call_id: "read-call-2".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "/tmp/child.txt" }),
            preceding_turn_id: Some(child_assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("<path>/tmp/child.txt</path>\n1: child output".to_string()),
            error: None,
            delegation: None,
            sequence_number: 61,
            requested_at: now,
            approved_at: Some(now),
            completed_at: Some(now),
        };

        session.runs = vec![
            RunRecord {
                id: root_run_id,
                status: RunStatus::InProgress,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 80,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: child_run_id,
                status: RunStatus::Completed,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(pending_invocation.id),
                created_sequence: 90,
                terminal_sequence: Some(91),
                terminal_stop_reason: Some(RunTerminalStopReason::Completed),
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            root_user_turn.clone(),
            root_assistant_turn.clone(),
            child_user_turn.clone(),
            child_assistant_turn.clone(),
        ];
        session.tool_invocations = vec![pending_invocation.clone(), child_invocation.clone()];
        session.transcript_items = vec![
            TranscriptItemRecord::from_turn(&Turn {
                sequence_number: 1,
                ..root_user_turn
            }),
            TranscriptItemRecord::assistant_reasoning(
                root_run_id,
                root_assistant_turn_id,
                2,
                "plan first",
                TranscriptStreamState::Committed,
            ),
            TranscriptItemRecord::assistant_text(
                root_run_id,
                root_assistant_turn_id,
                3,
                "I will inspect the repo.",
                TranscriptStreamState::Committed,
            ),
            TranscriptItemRecord::from_tool_invocation(&ToolInvocationRecord {
                sequence_number: 4,
                ..pending_invocation.clone()
            }),
            TranscriptItemRecord::permission(
                &pending_invocation,
                5,
                TranscriptPermissionState::Pending,
                None,
            ),
            TranscriptItemRecord::from_turn(&Turn {
                sequence_number: 6,
                ..child_user_turn
            }),
            TranscriptItemRecord::from_turn(&Turn {
                sequence_number: 7,
                ..child_assistant_turn
            }),
            TranscriptItemRecord::from_tool_invocation(&ToolInvocationRecord {
                sequence_number: 8,
                ..child_invocation
            }),
        ];
        session.next_replay_sequence = 100;
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id: root_run_id,
            phase: ForegroundPhase::AwaitingToolApproval,
            batch_anchor_turn_id: Some(root_assistant_turn_id),
        });
        pending_invocation.sequence_number = 60;
        session.rebuild_run_indexes();
        store.create(&session).unwrap();
        session
    }

    fn pending_permission_batch_session(store: &FsSessionStore) -> Session {
        let mut session = Session::new("pending permission batch");
        let run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        session.upsert_run(run_id, RunStatus::InProgress);

        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "inspect the repository".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });

        let assistant_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "I should inspect a few paths".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_sequence_number,
            timestamp: Utc::now(),
        });

        let first_invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "read-call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "/tmp/first.txt" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            delegation: None,
            sequence_number: first_invocation_sequence_number,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });

        let second_invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "glob-call-2".to_string(),
            tool_name: "glob".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "pattern": "**/*.rs", "path": "/tmp/project" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            delegation: None,
            sequence_number: second_invocation_sequence_number,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });

        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::AwaitingToolApproval,
            batch_anchor_turn_id: Some(assistant_turn_id),
        });
        store.create(&session).unwrap();
        session
    }

    fn active_delegated_child_session() -> Session {
        let mut session = Session::new("active delegated child");
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let task_invocation_id = Uuid::new_v4();
        let user_turn_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();

        let parent_run_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: parent_run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: parent_run_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        let child_run_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: child_run_id,
            status: RunStatus::InProgress,
            parent_run_id: Some(parent_run_id),
            parent_tool_invocation_id: Some(task_invocation_id),
            created_sequence: child_run_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: user_turn_id,
            run_id: parent_run_id,
            role: Role::User,
            content: "delegate work".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });
        let assistant_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "I will delegate that task.".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_sequence_number,
            timestamp: Utc::now(),
        });
        let child_prompt_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::User,
            content: "Inspect cancellation flow".to_string(),
            reasoning: String::new(),
            sequence_number: child_prompt_sequence,
            timestamp: Utc::now(),
        });

        let invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: task_invocation_id,
            run_id: parent_run_id,
            tool_call_id: "task-call-1".to_string(),
            tool_name: "task".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({
                "agent": "explore",
                "prompt": "Inspect cancellation flow"
            }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: Some(TaskDelegationRecord {
                child_run_id: Some(child_run_id),
                agent_name: Some("explore".to_string()),
                prompt: Some("Inspect cancellation flow".to_string()),
                status: TaskDelegationStatus::Running,
            }),
            sequence_number: invocation_sequence_number,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });

        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id: child_run_id,
            phase: ForegroundPhase::Generating,
            batch_anchor_turn_id: None,
        });
        session.rebuild_run_indexes();
        session
    }

    fn test_config(data_dir: PathBuf) -> Config {
        let plugin_root = data_dir.join("plugins");
        fs::create_dir_all(plugin_root.join("project")).unwrap();
        fs::create_dir_all(plugin_root.join("global")).unwrap();

        Config {
            config_path: None,
            data_dir: data_dir.clone(),
            logging: LoggingConfig {
                file: LoggingFileConfig {
                    enabled: false,
                    path: data_dir.join("logs/fluent-code.log"),
                    level: "info".to_string(),
                },
                stderr: LoggingStderrConfig {
                    enabled: false,
                    level: "info".to_string(),
                },
            },
            model: ModelConfig {
                provider: "mock".to_string(),
                model: "gpt-4.1-mini".to_string(),
                reasoning_effort: None,
                system_prompt: "You are a helpful coding assistant.".to_string(),
            },
            agents: None,
            plugins: PluginConfig {
                enable_project_plugins: false,
                enable_global_plugins: false,
                project_dir: plugin_root.join("project"),
                global_dir: plugin_root.join("global"),
            },
            acp: AcpConfig {
                protocol_version: 1,
                auth_methods: Vec::new(),
                session_defaults: AcpSessionDefaultsConfig {
                    system_prompt: "You are a helpful coding assistant.".to_string(),
                    reasoning_effort: None,
                },
            },
            model_providers: HashMap::new(),
        }
    }

    fn test_server_with_chunk_delay(data_dir: PathBuf, chunk_delay: Duration) -> AcpServer {
        let config = test_config(data_dir.clone());
        let store = FsSessionStore::new(data_dir);
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::with_chunk_delay(chunk_delay)),
            Arc::clone(&tool_registry),
        );

        AcpServer::from_dependencies(AcpServerDependencies {
            config,
            store,
            agent_registry,
            runtime,
            tool_registry,
            plugin_load_snapshot: PluginLoadSnapshot::default(),
        })
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique_suffix}"))
    }

    fn cleanup(path: PathBuf) {
        if path.exists() {
            fs::remove_dir_all(path).unwrap();
        }
    }

    fn output_frames(output: Vec<u8>) -> Vec<Value> {
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn response_values(frames: Vec<OutboundFrame>) -> Vec<Value> {
        frames
            .into_iter()
            .map(|frame| {
                frame
                    .into_value()
                    .expect("outbound frame should parse as JSON")
            })
            .collect()
    }

    trait FrameValueExt {
        fn to_frame_value(&self) -> Value;
    }

    impl FrameValueExt for Value {
        fn to_frame_value(&self) -> Value {
            self.clone()
        }
    }

    impl FrameValueExt for OutboundFrame {
        fn to_frame_value(&self) -> Value {
            self.to_value()
                .expect("outbound frame should parse as JSON")
        }
    }

    fn managed_session_for(host: ManagedAppHost, server: &AcpServer) -> ManagedSession {
        let mut host = host;
        if let Some(owner) = host.state().session.foreground_owner.clone() {
            host.state_mut()
                .set_foreground(owner.run_id, owner.phase, owner.batch_anchor_turn_id);
        }
        let session_id = host.state().session.id.to_string();
        let live_prompt_turn = server
            .seed_live_prompt_turn_state(&session_id, &host)
            .unwrap();
        ManagedSession {
            cwd: PathBuf::from("/tmp"),
            mcp_servers: Vec::new(),
            host,
            live_prompt_turn,
            pending_prompt_request_id: None,
            buffered_prompt_completion: None,
        }
    }

    fn session_update_frames<F: FrameValueExt>(frames: &[F]) -> Vec<Value> {
        frames
            .iter()
            .map(FrameValueExt::to_frame_value)
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some("session/update"))
            .collect()
    }

    fn session_update_kind(frame: &Value) -> Option<&str> {
        frame["params"]["update"]["sessionUpdate"].as_str()
    }

    fn collect_agent_message_chunks(frames: &[Value]) -> String {
        frames
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("agent_message_chunk"))
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    fn collect_agent_message_chunk_texts(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("agent_message_chunk"))
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn collect_chunk_texts_by_kind<F: FrameValueExt>(frames: &[F], kind: &str) -> Vec<String> {
        frames
            .iter()
            .map(FrameValueExt::to_frame_value)
            .filter(|frame| session_update_kind(frame) == Some(kind))
            .filter_map(|frame| {
                frame["params"]["update"]["content"]["text"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }

    fn split_text_like_mock_provider(text: &str) -> Vec<String> {
        let mut chunks = text
            .split_inclusive(' ')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if chunks.is_empty() {
            chunks.push(text.to_string());
        }
        chunks
    }

    #[derive(Clone, Default)]
    struct RecordingClient {
        agent_chunk_count: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    }

    #[derive(Clone, Default)]
    struct NotifyingFrameCapture {
        state: Arc<std::sync::Mutex<NotifyingFrameCaptureState>>,
        agent_chunk_count: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    }

    #[derive(Default)]
    struct NotifyingFrameCaptureState {
        bytes: Vec<u8>,
        parsed_offset: usize,
    }

    impl NotifyingFrameCapture {
        fn output_frames(&self) -> Vec<Value> {
            output_frames(
                self.state
                    .lock()
                    .expect("notifying frame capture mutex poisoned")
                    .bytes
                    .clone(),
            )
        }
    }

    impl Write for NotifyingFrameCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut state = self
                .state
                .lock()
                .expect("notifying frame capture mutex poisoned");
            state.bytes.extend_from_slice(buf);

            while let Some(relative_newline_index) = state.bytes[state.parsed_offset..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let newline_index = state.parsed_offset + relative_newline_index;
                let line =
                    String::from_utf8_lossy(&state.bytes[state.parsed_offset..newline_index])
                        .into_owned();
                state.parsed_offset = newline_index + 1;

                if line.is_empty() {
                    continue;
                }

                if let Ok(frame) = serde_json::from_str::<Value>(&line)
                    && session_update_kind(&frame) == Some("agent_message_chunk")
                {
                    self.agent_chunk_count.fetch_add(1, Ordering::SeqCst);
                    self.notify.notify_waiters();
                }
            }

            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct PermissionOrderingClient {
        events: Arc<std::sync::Mutex<Vec<String>>>,
        notify: Arc<Notify>,
    }

    impl PermissionOrderingClient {
        fn record_event(&self, event: impl Into<String>) {
            let mut events = self
                .events
                .lock()
                .expect("permission-ordering client events mutex poisoned");
            events.push(event.into());
            self.notify.notify_waiters();
        }

        fn snapshot_events(&self) -> Vec<String> {
            self.events
                .lock()
                .expect("permission-ordering client events mutex poisoned")
                .clone()
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RecordingClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            if matches!(args.update, acp::SessionUpdate::AgentMessageChunk(_)) {
                self.agent_chunk_count.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_waiters();
            }
            Ok(())
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            Err(acp::Error::method_not_found())
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for PermissionOrderingClient {
        async fn request_permission(
            &self,
            args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            self.record_event(format!(
                "request_permission:{}",
                args.tool_call.tool_call_id
            ));
            let allow_once = args
                .options
                .iter()
                .find(|option| option.option_id.to_string() == "allow_once")
                .expect("permission request should expose allow_once");

            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    allow_once.option_id.to_string(),
                )),
            ))
        }

        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            let notification = serde_json::to_value(&args)
                .map_err(|error| acp::Error::new(JSONRPC_INTERNAL_ERROR, error.to_string()))?;
            let event = match notification["update"]["sessionUpdate"].as_str() {
                Some("tool_call") => format!(
                    "tool_call:{}",
                    notification["update"]["toolCallId"]
                        .as_str()
                        .unwrap_or_default()
                ),
                Some("tool_call_update") => format!(
                    "tool_call_update:{}:{}:{}",
                    notification["update"]["toolCallId"]
                        .as_str()
                        .unwrap_or_default(),
                    notification["update"]["status"]
                        .as_str()
                        .unwrap_or_default(),
                    notification["update"]["rawOutput"]["result"]
                        .as_str()
                        .unwrap_or_default(),
                ),
                Some("agent_message_chunk") => format!(
                    "agent_message_chunk:{}",
                    notification["update"]["content"]["text"]
                        .as_str()
                        .unwrap_or_default()
                ),
                Some(kind) => format!("session_update:{kind}"),
                None => "session_notification".to_string(),
            };
            self.record_event(event);
            Ok(())
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            Err(acp::Error::method_not_found())
        }
    }

    async fn wait_for_agent_chunk(client: &RecordingClient) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let notified = client.notify.notified();
                if client.agent_chunk_count.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("timed out waiting for an incrementally delivered agent chunk");
    }

    async fn wait_for_capture_agent_chunk(capture: &NotifyingFrameCapture) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let notified = capture.notify.notified();
                if capture.agent_chunk_count.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("timed out waiting for an incrementally delivered ACP stdio agent chunk");
    }

    async fn wait_for_client_event<F>(
        client: &PermissionOrderingClient,
        description: &str,
        predicate: F,
    ) where
        F: Fn(&[String]) -> bool,
    {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = client.notify.notified();
                let events = client.snapshot_events();
                if predicate(&events) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
    }

    fn collect_agent_message_chunk_texts_from_events(events: &[String]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.strip_prefix("agent_message_chunk:"))
            .map(str::to_owned)
            .collect()
    }

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
