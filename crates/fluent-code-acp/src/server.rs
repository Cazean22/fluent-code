use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fluent_code_app::agent::AgentRegistry;
use fluent_code_app::app::{AppState, AppStatus, Effect, Msg, recover_startup_foreground, update};
use fluent_code_app::config::Config;
use fluent_code_app::logging::{config_source_for_log, init_logging, path_for_log};
use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry, load_tool_registry};
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::{
    ForegroundPhase, RunId, RunTerminalStopReason, Session, SessionId, TaskDelegationStatus,
    ToolApprovalState, ToolExecutionState,
};
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use fluent_code_app::{FluentCodeError, Result};
use fluent_code_provider::ProviderClient;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::mapping::{
    ProjectionEventPhase, PromptTurnEvent, PromptTurnProjection, SessionUpdateMapper,
    TerminalStopProjection,
};
use crate::protocol::{
    AgentMessageChunk, AgentThoughtChunk, AuthenticateRequest, AuthenticateResponse, ContentBlock,
    InitializeRequest, InitializeResponse, JsonRpcErrorResponse, JsonRpcNotification,
    JsonRpcProtocol, JsonRpcResponse, LoadSessionRequest, LoadSessionResponse, Method,
    NewSessionRequest, NewSessionResponse, ParsedRequest, PromptTurnState, ProtocolError,
    ServerInfo, SessionCancelRequest, SessionNotification, SessionPromptRequest,
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
const SESSION_UPDATE_METHOD: &str = "session/update";
const SESSION_REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CANCELLED_TOOL_MESSAGE: &str =
    "Tool execution was cancelled because the prompt turn was cancelled.";

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let _logging = init_logging(&config)?;

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

    AcpServer::build(config)?.run().await
}

pub struct HeadlessAppHost {
    state: AppState,
    store: FsSessionStore,
    runtime: Runtime,
    runtime_sender: mpsc::UnboundedSender<Msg>,
    runtime_receiver: mpsc::UnboundedReceiver<Msg>,
}

impl HeadlessAppHost {
    pub fn new(
        session: Session,
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Self {
        let (runtime_sender, runtime_receiver) = mpsc::unbounded_channel();

        Self {
            state: AppState::new_with_plugin_state(
                session,
                agent_registry,
                tool_registry,
                plugin_load_snapshot,
            ),
            store,
            runtime,
            runtime_sender,
            runtime_receiver,
        }
    }

    pub fn load_or_create(
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Result<Self> {
        let session = store.load_or_create_latest()?;
        Ok(Self::new(
            session,
            store,
            runtime,
            agent_registry,
            tool_registry,
            plugin_load_snapshot,
        ))
    }

    pub fn load(
        session_id: &SessionId,
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Result<Self> {
        let session = store.load(session_id)?;
        Ok(Self::new(
            session,
            store,
            runtime,
            agent_registry,
            tool_registry,
            plugin_load_snapshot,
        ))
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn runtime_sender(&self) -> mpsc::UnboundedSender<Msg> {
        self.runtime_sender.clone()
    }

    pub async fn recover_startup(&mut self) -> Result<()> {
        let effects = recover_startup_foreground(&mut self.state);
        if effects.is_empty() {
            return Ok(());
        }

        self.apply_effects(effects, &mut VecDeque::new()).await
    }

    pub async fn submit_prompt(&mut self, prompt: impl Into<String>) -> Result<()> {
        self.handle_message(Msg::InputChanged(prompt.into()))
            .await?;
        self.handle_message(Msg::SubmitPrompt).await
    }

    pub async fn handle_message(&mut self, msg: Msg) -> Result<()> {
        if matches!(msg, Msg::NewSession) {
            self.create_and_swap_session()?;
            return Ok(());
        }

        debug!(
            session_id = %self.state.session.id,
            message = ?msg,
            "handling headless host message"
        );

        let mut pending_messages = VecDeque::from([msg]);

        while let Some(message) = pending_messages.pop_front() {
            let effects = update(&mut self.state, message);
            self.apply_effects(effects, &mut pending_messages).await?;
        }

        Ok(())
    }

    pub async fn drain_runtime_messages(&mut self) -> Result<()> {
        while let Ok(message) = self.runtime_receiver.try_recv() {
            debug!(
                session_id = %self.state.session.id,
                message = ?message,
                "draining queued runtime message into headless host state"
            );
            self.handle_message(message).await?;
        }

        Ok(())
    }

    fn create_and_swap_session(&mut self) -> Result<()> {
        let session = self.store.create_new_session()?;
        info!(session_id = %session.id, "swapped headless host to new session");
        self.state.replace_session(session);
        Ok(())
    }

    async fn apply_effects(
        &mut self,
        effects: Vec<Effect>,
        pending_messages: &mut VecDeque<Msg>,
    ) -> Result<()> {
        for effect in effects {
            self.apply_effect(
                effect,
                "forwarding async effect from headless host to runtime",
            )?;
        }

        while let Some(message) = pending_messages.pop_front() {
            let effects = update(&mut self.state, message);
            for effect in effects {
                self.apply_effect(
                    effect,
                    "forwarding queued async effect from headless host to runtime",
                )?;
            }
        }

        Ok(())
    }

    fn apply_effect(&mut self, effect: Effect, async_effect_log_context: &str) -> Result<()> {
        match effect {
            Effect::PersistSession => self.persist_session(),
            Effect::PersistSessionIfDue => self.persist_session_if_due(),
            Effect::StartAssistant { .. }
            | Effect::ExecuteTool { .. }
            | Effect::CancelAssistant { .. } => {
                debug!(
                    session_id = %self.state.session.id,
                    effect = ?effect,
                    "{async_effect_log_context}"
                );
                self.runtime
                    .spawn_effect(effect, self.runtime_sender.clone());
                Ok(())
            }
        }
    }

    fn persist_session(&mut self) -> Result<()> {
        debug!(session_id = %self.state.session.id, "persisting session snapshot from headless host");
        self.store.save(&self.state.session)?;
        Ok(())
    }

    fn persist_session_if_due(&mut self) -> Result<()> {
        if self.state.should_checkpoint_now() {
            debug!(session_id = %self.state.session.id, "persisting due session checkpoint from headless host");
            self.store.save(&self.state.session)?;
            self.state.mark_checkpoint_saved();
        }

        Ok(())
    }

    pub fn persist_now(&mut self) -> Result<()> {
        self.persist_session()
    }
}

pub struct AcpServerDependencies {
    pub config: Config,
    pub store: FsSessionStore,
    pub agent_registry: Arc<AgentRegistry>,
    pub runtime: Runtime,
    pub tool_registry: Arc<ToolRegistry>,
    pub plugin_load_snapshot: PluginLoadSnapshot,
}

impl AcpServerDependencies {
    pub fn from_config(config: Config) -> Result<Self> {
        let store = FsSessionStore::new(config.data_dir.clone());
        let agent_registry = Arc::new(AgentRegistry::from_configured(config.agents.as_deref())?);
        let provider = ProviderClient::new(
            &config.model.provider,
            config.model.model.clone(),
            config.model.system_prompt.clone(),
            config.model.reasoning_effort.clone(),
            config.selected_provider_config().cloned(),
        )?;
        let loaded_tool_registry = load_tool_registry(&config)?;
        let tool_registry = Arc::new(loaded_tool_registry.tool_registry);
        let runtime = Runtime::new_with_tool_registry(provider, Arc::clone(&tool_registry));

        Ok(Self {
            config,
            store,
            agent_registry,
            runtime,
            tool_registry,
            plugin_load_snapshot: loaded_tool_registry.plugin_load_snapshot,
        })
    }
}

pub struct AcpServer {
    dependencies: AcpServerDependencies,
    transport: StdioTransport,
    mapper: SessionUpdateMapper,
}

#[derive(Default)]
struct AcpConnectionState {
    protocol: JsonRpcProtocol,
    authenticated: bool,
    sessions: HashMap<SessionId, ManagedSession>,
}

struct ManagedSession {
    _cwd: PathBuf,
    _mcp_servers: Vec<Value>,
    host: HeadlessAppHost,
    live_prompt_turn: Option<LivePromptTurnState>,
    pending_prompt_request_id: Option<u64>,
}

struct LivePromptTurnState {
    root_run_id: RunId,
    emission_state: PromptTurnEmissionState,
}

enum ReaderEvent {
    Frame(String),
    Eof,
    Error(String),
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
    emitted_text_lengths: HashMap<(u64, u8), usize>,
    emitted_tool_calls: HashSet<String>,
    emitted_permission_requests: HashSet<String>,
    latest_tool_updates: HashMap<String, ToolCallUpdate>,
}

struct PromptTurnResult {
    frames: Vec<Value>,
    stop_reason: SessionPromptResponse,
}

struct LivePromptPollResult {
    frames: Vec<Value>,
}

impl PromptTurnEmissionState {
    fn project_frames(
        &mut self,
        session_id: &str,
        projection: &PromptTurnProjection,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
            SessionUpdate::SessionInfoUpdate(session_info_update) => Some(
                SessionUpdate::SessionInfoUpdate(session_info_update.clone()),
            ),
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

    pub fn dependencies(&self) -> &AcpServerDependencies {
        &self.dependencies
    }

    pub fn transport(&self) -> StdioTransport {
        self.transport
    }

    pub fn server_info(&self) -> ServerInfo {
        self.mapper.server_info()
    }

    pub fn build_host(&self) -> Result<HeadlessAppHost> {
        HeadlessAppHost::load_or_create(
            self.dependencies.store.clone(),
            self.dependencies.runtime.clone(),
            Arc::clone(&self.dependencies.agent_registry),
            Arc::clone(&self.dependencies.tool_registry),
            self.dependencies.plugin_load_snapshot.clone(),
        )
    }

    fn build_host_for_session(&self, session: Session) -> HeadlessAppHost {
        HeadlessAppHost::new(
            session,
            self.dependencies.store.clone(),
            self.dependencies.runtime.clone(),
            Arc::clone(&self.dependencies.agent_registry),
            Arc::clone(&self.dependencies.tool_registry),
            self.dependencies.plugin_load_snapshot.clone(),
        )
    }

    fn load_host(&self, session_id: &SessionId) -> Result<HeadlessAppHost> {
        HeadlessAppHost::load(
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

        let stdout = io::stdout();
        let mut writer = stdout.lock();
        let frames_processed = self.serve_stdio_until_eof(&mut writer).await?;
        info!(
            frames_processed,
            "acp stdio server stopped after stdin closed"
        );

        Ok(())
    }

    pub fn initialize_response(&self, requested_protocol_version: u16) -> InitializeResponse {
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

    async fn serve_stdio_until_eof<W: Write>(&self, writer: &mut W) -> Result<usize> {
        let (frame_sender, mut frame_receiver) = mpsc::unbounded_channel();
        let transport = self.transport;
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                match transport.read_frame(&mut reader) {
                    Ok(Some(frame)) => {
                        if frame_sender.send(ReaderEvent::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = frame_sender.send(ReaderEvent::Eof);
                        break;
                    }
                    Err(error) => {
                        let _ = frame_sender.send(ReaderEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
        });

        self.serve_live_frames(&mut frame_receiver, writer).await
    }

    async fn serve_live_frames<W: Write>(
        &self,
        frame_receiver: &mut mpsc::UnboundedReceiver<ReaderEvent>,
        writer: &mut W,
    ) -> Result<usize> {
        let mut connection = AcpConnectionState::default();
        let mut frames_processed = 0;
        let mut reader_closed = false;

        while !reader_closed || has_live_prompt_turns(&connection) {
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
                                Err(error_response) => vec![serde_json::to_value(error_response)?],
                            };
                            self.write_outbound_frames(writer, outbound_frames.clone())?;
                            clear_written_prompt_responses(
                                &mut connection,
                                &frame_ids_from_responses(&outbound_frames),
                            );

                            if has_live_prompt_turns(&connection) {
                                let outbound_frames = self
                                    .poll_live_prompt_turns(&mut connection)
                                    .await
                                    .map_err(|error| FluentCodeError::Provider(error.message))?;
                                self.write_outbound_frames(writer, outbound_frames.clone())?;
                                clear_written_prompt_responses(
                                    &mut connection,
                                    &frame_ids_from_responses(&outbound_frames),
                                );
                            }
                        }
                        ReaderEvent::Eof => {
                            reader_closed = true;
                        }
                        ReaderEvent::Error(error) => {
                            return Err(FluentCodeError::Provider(format!(
                                "ACP stdio transport error: {error}"
                            )));
                        }
                    }
                }
                _ = tokio::time::sleep(PROMPT_POLL_INTERVAL), if has_live_prompt_turns(&connection) => {
                    let outbound_frames = self
                        .poll_live_prompt_turns(&mut connection)
                        .await
                        .map_err(|error| FluentCodeError::Provider(error.message))?;
                    self.write_outbound_frames(writer, outbound_frames.clone())?;
                    clear_written_prompt_responses(
                        &mut connection,
                        &frame_ids_from_responses(&outbound_frames),
                    );
                }
            }
        }

        Ok(frames_processed)
    }

    pub async fn serve_jsonl_script<R: BufRead, W: Write>(
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
                Err(error_response) => vec![serde_json::to_value(error_response)?],
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
        self.serve_jsonl_script(reader, writer).await
    }

    fn write_outbound_frames<W: Write>(
        &self,
        writer: &mut W,
        outbound_frames: Vec<Value>,
    ) -> Result<()> {
        for outbound_frame in outbound_frames {
            self.transport
                .write_frame(writer, &outbound_frame)
                .map_err(|error| {
                    FluentCodeError::Provider(format!("ACP stdio transport error: {error}"))
                })?;
        }

        Ok(())
    }

    async fn handle_frame(
        &self,
        connection: &mut AcpConnectionState,
        frame: &str,
    ) -> std::result::Result<Vec<Value>, JsonRpcErrorResponse> {
        let request_id = request_id_from_frame(frame);
        let request = self
            .parse_request(connection, frame)
            .map_err(|error| protocol_error_response(request_id.clone(), error))?;

        self.handle_request(connection, request)
            .await
            .map_err(|error| JsonRpcErrorResponse::new(request_id, error.code, error.message))
    }

    async fn handle_live_frame(
        &self,
        connection: &mut AcpConnectionState,
        frame: &str,
    ) -> std::result::Result<Vec<Value>, JsonRpcErrorResponse> {
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
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        match request.method {
            Method::Initialize => self.handle_initialize(connection, request),
            Method::Authenticate => self.handle_authenticate(connection, request),
            Method::SessionNew => self.handle_session_new(connection, request),
            Method::SessionLoad => self.handle_session_load(connection, request).await,
            Method::SessionPrompt => self.handle_session_prompt(connection, request).await,
            Method::SessionCancel => self.handle_session_cancel(connection, request).await,
        }
    }

    fn handle_initialize(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let new_session_request = decode_params::<NewSessionRequest>(request.params)?;
        let cwd = validate_absolute_cwd(&new_session_request.cwd)?;
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
            ManagedSession {
                _cwd: cwd,
                _mcp_servers: new_session_request.mcp_servers,
                host,
                live_prompt_turn: None,
                pending_prompt_request_id: None,
            },
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
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
            },
        ))?);

        connection.sessions.insert(
            host.state().session.id,
            ManagedSession {
                _cwd: cwd,
                _mcp_servers: load_session_request.mcp_servers,
                host,
                live_prompt_turn,
                pending_prompt_request_id: None,
            },
        );

        Ok(outbound_frames)
    }

    async fn handle_session_prompt(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let session_prompt_request = decode_params::<SessionPromptRequest>(request.params)?;
        let session_id = parse_session_id(&session_prompt_request.session_id)?;
        let prompt = prompt_text_from_blocks(&session_prompt_request.prompt)?;
        let managed_session = connection.sessions.get_mut(&session_id).ok_or_else(|| {
            RpcResponseError::resource_not_found(format!(
                "session `{session_id}` is not active on this connection"
            ))
        })?;

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

    async fn handle_live_session_prompt(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let request_id = request.id;
        let session_prompt_request = decode_params::<SessionPromptRequest>(request.params)?;
        let session_id = parse_session_id(&session_prompt_request.session_id)?;
        let prompt = prompt_text_from_blocks(&session_prompt_request.prompt)?;
        let managed_session = connection.sessions.get_mut(&session_id).ok_or_else(|| {
            RpcResponseError::resource_not_found(format!(
                "session `{session_id}` is not active on this connection"
            ))
        })?;

        if managed_session.host.state().active_run_id.is_some() {
            return Err(RpcResponseError::active_prompt(format!(
                "session `{session_id}` already has an active prompt turn"
            )));
        }

        self.start_live_prompt_turn(
            &session_prompt_request.session_id,
            managed_session,
            prompt,
            request_id,
        )
        .await
    }

    async fn handle_session_cancel(
        &self,
        connection: &mut AcpConnectionState,
        request: ParsedRequest,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        self.ensure_authenticated(connection)?;

        let session_cancel_request = decode_params::<SessionCancelRequest>(request.params)?;
        let session_id = parse_session_id(&session_cancel_request.session_id)?;
        let managed_session = connection.sessions.get_mut(&session_id).ok_or_else(|| {
            RpcResponseError::resource_not_found(format!(
                "session `{session_id}` is not active on this connection"
            ))
        })?;

        let mut outbound_frames = self
            .drain_live_prompt_turn_updates(&session_cancel_request.session_id, managed_session)
            .await?;
        outbound_frames.push(serialize_value(JsonRpcResponse::new(
            request.id,
            SessionPromptResponse {
                stop_reason: crate::protocol::StopReason::Cancelled,
            },
        ))?);
        let cancel_target = cancel_target(managed_session.host.state()).ok_or_else(|| {
            RpcResponseError::invalid_params(format!(
                "session `{session_id}` does not have an active prompt turn to cancel"
            ))
        })?;

        outbound_frames.extend(
            self.cancel_prompt_turn(
                &session_cancel_request.session_id,
                managed_session,
                cancel_target,
            )
            .await?,
        );
        Ok(outbound_frames)
    }

    async fn start_live_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        prompt: String,
        request_id: u64,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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

    fn seed_live_prompt_turn_state(
        &self,
        session_id: &str,
        host: &HeadlessAppHost,
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
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        self.poll_live_prompt_turn(session_id, managed_session)
            .await
            .map(|result| result.frames)
    }

    async fn cancel_prompt_turn(
        &self,
        session_id: &str,
        managed_session: &mut ManagedSession,
        cancel_target: CancelTarget,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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

    async fn poll_live_prompt_turns(
        &self,
        connection: &mut AcpConnectionState,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
        let mut outbound_frames = Vec::new();

        for managed_session in connection.sessions.values_mut() {
            let session_id = managed_session.host.state().session.id.to_string();
            if managed_session.live_prompt_turn.is_none() {
                outbound_frames.extend(self.flush_pending_prompt_response(managed_session)?);
                continue;
            }

            outbound_frames.extend(
                self.poll_live_prompt_turn(&session_id, managed_session)
                    .await?
                    .frames,
            );
        }

        Ok(outbound_frames)
    }

    fn flush_pending_prompt_response(
        &self,
        managed_session: &mut ManagedSession,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
            return Ok(LivePromptPollResult { frames: Vec::new() });
        };
        let projection = self
            .mapper
            .project_prompt_turn(managed_session.host.state(), live_prompt_turn.root_run_id)
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
            }
        }

        Ok(LivePromptPollResult {
            frames: outbound_frames,
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
        host: &HeadlessAppHost,
    ) -> std::result::Result<Vec<Value>, RpcResponseError> {
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
        host: &mut HeadlessAppHost,
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

        loop {
            let projection = self
                .mapper
                .project_prompt_turn(host.state(), run_id)
                .ok_or_else(|| {
                    RpcResponseError::internal(format!(
                        "session `{session_id}` lost prompt turn projection for run `{run_id}`"
                    ))
                })?;
            outbound_frames.extend(emission_state.project_frames(session_id, &projection)?);

            if let Some(stop_reason) = prompt_turn_response(host.state(), run_id, &projection)? {
                return Ok(PromptTurnResult {
                    frames: outbound_frames,
                    stop_reason,
                });
            }

            tokio::time::sleep(PROMPT_POLL_INTERVAL).await;
            host.drain_runtime_messages()
                .await
                .map_err(|error| RpcResponseError::internal(error.to_string()))?;
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

fn frame_ids_from_responses(frames: &[Value]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|frame| frame.get("id").and_then(Value::as_u64))
        .collect()
}

fn clear_written_prompt_responses(connection: &mut AcpConnectionState, written_ids: &[u64]) {
    if written_ids.is_empty() {
        return;
    }

    for managed_session in connection.sessions.values_mut() {
        if managed_session
            .pending_prompt_request_id
            .is_some_and(|request_id| written_ids.contains(&request_id))
        {
            managed_session.pending_prompt_request_id = None;
        }
    }
}

fn has_live_prompt_turns(connection: &AcpConnectionState) -> bool {
    connection.sessions.values().any(|managed_session| {
        managed_session.live_prompt_turn.is_some()
            || managed_session.pending_prompt_request_id.is_some()
    })
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

fn decode_params<T: serde::de::DeserializeOwned>(
    params: Value,
) -> std::result::Result<T, RpcResponseError> {
    serde_json::from_value(params)
        .map_err(|error| RpcResponseError::invalid_params(error.to_string()))
}

fn serialize_value<T: Serialize>(value: T) -> std::result::Result<Value, RpcResponseError> {
    serde_json::to_value(value).map_err(|error| RpcResponseError::internal(error.to_string()))
}

fn session_update_frame(
    session_id: &str,
    update: SessionUpdate,
) -> std::result::Result<Value, RpcResponseError> {
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
) -> std::result::Result<Value, RpcResponseError> {
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
        ProjectionEventPhase::ToolCallCreate => 3,
        ProjectionEventPhase::ToolCallPatch => 4,
        ProjectionEventPhase::PermissionRequest => 5,
    }
}

fn prompt_text_from_blocks(
    prompt: &[ContentBlock],
) -> std::result::Result<String, RpcResponseError> {
    let prompt = prompt
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.trim().to_string(),
        })
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
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        ToolSource, Turn,
    };
    use fluent_code_app::session::store::{FsSessionStore, SessionStore};
    use fluent_code_provider::{MockProvider, ProviderClient};
    use serde_json::Value;
    use tokio::sync::{Mutex, mpsc};
    use uuid::Uuid;

    use super::{
        AcpConnectionState, AcpServer, AcpServerDependencies, CANCELLED_TOOL_MESSAGE,
        HeadlessAppHost, ManagedSession, ReaderEvent,
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
        let permission_requests = responses
            .iter()
            .filter(|frame| frame["method"] == "session/request_permission")
            .collect::<Vec<_>>();

        assert_eq!(tool_calls.len(), 2);
        assert_eq!(permission_requests.len(), 1);
        assert_eq!(
            permission_requests[0]["params"]["toolCall"]["toolCallId"],
            "glob-call-2"
        );
        assert_eq!(
            permission_requests[0]["params"]["toolCall"]["locations"][0]["path"],
            "/tmp/project"
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
        connection.sessions.insert(session_id, managed_session);

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
            .get_mut(&session_id)
            .expect("managed session");
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
        connection.sessions.insert(session_id, managed_session);

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
            .get_mut(&session_id)
            .expect("managed session");
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
        connection.sessions.insert(session_id, managed_session);

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
            .expect("managed session");
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

        cleanup(temp_dir);
    }

    #[tokio::test]
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

            tokio::time::sleep(Duration::from_millis(20)).await;

            frame_sender
                .send(ReaderEvent::Frame(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}",
                    session_id,
                )))
                .unwrap();

            tokio::time::sleep(Duration::from_millis(200)).await;
            frame_sender.send(ReaderEvent::Eof).unwrap();
        });

        let mut output = Vec::new();
        let frames_processed = server
            .serve_live_frames(&mut frame_receiver, &mut output)
            .await
            .unwrap();
        sender.await.unwrap();

        let responses = output_frames(output);
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
    async fn live_session_new_cancel_keeps_prompt_request_resolvable() {
        let _guard = test_lock().lock().await;
        let temp_dir = unique_temp_dir("fluent-code-acp-live-session-new-cancel");
        fs::create_dir_all(&temp_dir).unwrap();
        let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
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

        tokio::time::sleep(Duration::from_millis(20)).await;

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

        let cancel_ids = cancel_frames
            .iter()
            .filter_map(|frame| frame.get("id").and_then(Value::as_u64))
            .collect::<Vec<_>>();

        assert!(cancel_ids.contains(&4));
        assert!(
            cancel_ids.contains(&3)
                || connection
                    .sessions
                    .values()
                    .any(|managed_session| managed_session.pending_prompt_request_id == Some(3)),
            "cancel path must either emit or retain the original prompt response",
        );

        cleanup(temp_dir);
    }

    #[tokio::test]
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
            session_update_kind(session_updates[0]),
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
        let mut host = HeadlessAppHost::new(
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

        let mut host = HeadlessAppHost::new(
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
        let mut host = HeadlessAppHost::new(
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

    fn response_values(frames: Vec<Value>) -> Vec<Value> {
        frames
    }

    fn managed_session_for(host: HeadlessAppHost, server: &AcpServer) -> ManagedSession {
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
            _cwd: PathBuf::from("/tmp"),
            _mcp_servers: Vec::new(),
            host,
            live_prompt_turn,
            pending_prompt_request_id: None,
        }
    }

    fn session_update_frames(frames: &[Value]) -> Vec<&Value> {
        frames
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some("session/update"))
            .collect()
    }

    fn session_update_kind(frame: &Value) -> Option<&str> {
        frame["params"]["update"]["sessionUpdate"].as_str()
    }

    fn collect_agent_message_chunks(frames: &[&Value]) -> String {
        frames
            .iter()
            .filter(|frame| session_update_kind(frame) == Some("agent_message_chunk"))
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
