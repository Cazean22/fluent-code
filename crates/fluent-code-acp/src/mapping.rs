use std::collections::HashSet;
use std::path::Path;

use fluent_code_app::app::AppState;
use fluent_code_app::config::{AcpConfig, AcpSessionDefaultsConfig};
use fluent_code_app::session::model::{
    ReplaySequence, Role, RunId, RunRecord, RunTerminalStopReason, Session, ToolApprovalState,
    ToolExecutionState, ToolInvocationRecord, ToolSource, TranscriptFidelity,
    TranscriptItemContent, TranscriptItemId, TranscriptItemRecord, TranscriptPermissionState,
    TranscriptStreamState, transcript_assistant_reasoning_item_id,
    transcript_assistant_text_item_id,
};
use serde_json::json;

use crate::protocol::{
    ACP_PROTOCOL_VERSION, AgentCapabilities, AgentMessageChunk, AgentThoughtChunk, AuthMethod,
    ContentBlock, InitializeResponse, McpCapabilities, Meta, PermissionOption,
    PermissionOptionKind, RequestPermissionRequest, ServerInfo, SessionCapabilities,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOption, SessionInfoUpdate, SessionListCapabilities, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolKind, UserMessageChunk,
};

const SESSION_INFO_UPDATE_PHASE: ProjectionEventPhase = ProjectionEventPhase::SessionInfoUpdate;
const TOOL_CALL_CREATE_PHASE: ProjectionEventPhase = ProjectionEventPhase::ToolCallCreate;
const TOOL_CALL_PATCH_PHASE: ProjectionEventPhase = ProjectionEventPhase::ToolCallPatch;
const ACP_META_TOOL_INVOCATION_KEY: &str = "fluentCodeToolInvocation";
const ACP_META_TRANSCRIPT_ITEM_KEY: &str = "fluentCodeTranscriptItem";
const ACP_META_TOOL_NAME_KEY: &str = "tool_name";
const ACP_META_SUBAGENT_SESSION_INFO_KEY: &str = "subagent_session_info";
const ACP_META_TERMINAL_INFO_KEY: &str = "terminal_info";
const ACP_META_TERMINAL_OUTPUT_KEY: &str = "terminal_output";
const ACP_META_TERMINAL_EXIT_KEY: &str = "terminal_exit";
const SYSTEM_PROMPT_CONFIG_ID: &str = "system_prompt";
const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";
const NO_REASONING_EFFORT_VALUE: &str = "none";

#[derive(Debug, Clone)]
pub struct SessionUpdateMapper {
    protocol_version: u16,
    auth_methods: Vec<AuthMethod>,
    session_config_options: Vec<SessionConfigOption>,
}

impl Default for SessionUpdateMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionUpdateMapper {
    pub const fn new() -> Self {
        Self {
            protocol_version: ACP_PROTOCOL_VERSION,
            auth_methods: Vec::new(),
            session_config_options: Vec::new(),
        }
    }

    pub fn from_acp_config(config: &AcpConfig) -> Self {
        Self {
            protocol_version: config.protocol_version,
            auth_methods: config
                .auth_methods
                .iter()
                .map(|method| AuthMethod {
                    id: method.id.clone(),
                    name: method.name.clone(),
                    description: method.description.clone(),
                })
                .collect(),
            session_config_options: session_config_options(&config.session_defaults),
        }
    }

    pub fn server_info(&self) -> ServerInfo {
        ServerInfo::with_title("fluent-code", "Fluent Code", env!("CARGO_PKG_VERSION"))
    }

    pub fn negotiated_capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            load_session: Some(true),
            resume_session: Some(true),
            close_session: Some(true),
            mcp_capabilities: Some(McpCapabilities {
                http: Some(true),
                sse: Some(true),
            }),
            session_capabilities: Some(SessionCapabilities {
                list: Some(SessionListCapabilities {}),
            }),
            ..AgentCapabilities::default()
        }
    }

    pub fn initialize_response(&self, _requested_protocol_version: u16) -> InitializeResponse {
        InitializeResponse {
            protocol_version: self.protocol_version,
            agent_capabilities: self.negotiated_capabilities(),
            agent_info: self.server_info(),
            auth_methods: self.auth_methods.clone(),
        }
    }

    pub fn session_config_options(&self) -> Option<Vec<SessionConfigOption>> {
        (!self.session_config_options.is_empty()).then(|| self.session_config_options.clone())
    }

    pub fn project_prompt_turns(&self, state: &AppState) -> Vec<PromptTurnProjection> {
        if uses_canonical_transcript_projection(&state.session) {
            return self.project_canonical_prompt_turns(state);
        }

        let mut runs = state
            .session
            .runs
            .iter()
            .filter(|run| run.parent_run_id.is_none())
            .filter(|run| is_valid_sequence(&state.session, run.created_sequence))
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| run.created_sequence);

        runs.into_iter()
            .filter_map(|run| self.project_prompt_turn(state, run.id))
            .collect()
    }

    pub fn project_prompt_turn(
        &self,
        state: &AppState,
        run_id: RunId,
    ) -> Option<PromptTurnProjection> {
        let session = &state.session;
        let run = session.find_run(run_id)?;
        if session.root_run_id(run_id) != Some(run_id) {
            return None;
        }

        if uses_canonical_transcript_projection(session) {
            return Some(self.project_canonical_prompt_turn(state, run));
        }

        Some(self.project_legacy_prompt_turn(state, run))
    }

    pub fn project_live_prompt_turn(
        &self,
        state: &AppState,
        run_id: RunId,
        watermark: Option<ReplaySequence>,
        previously_open_item_ids: &HashSet<TranscriptItemId>,
    ) -> Option<PromptTurnProjection> {
        let session = &state.session;
        let run = session.find_run(run_id)?;
        if session.root_run_id(run_id) != Some(run_id) {
            return None;
        }

        if uses_canonical_transcript_projection(session) {
            return Some(self.project_canonical_prompt_turn_incremental(
                state,
                run,
                watermark,
                previously_open_item_ids,
            ));
        }

        Some(self.project_legacy_prompt_turn(state, run))
    }

    fn project_canonical_prompt_turns(&self, state: &AppState) -> Vec<PromptTurnProjection> {
        self.canonical_root_run_ids_in_projection_order(state)
            .into_iter()
            .filter_map(|run_id| self.project_prompt_turn(state, run_id))
            .collect()
    }

    fn canonical_root_run_ids_in_projection_order(&self, state: &AppState) -> Vec<RunId> {
        let mut root_run_ids = Vec::new();
        let mut seen_root_run_ids = HashSet::new();

        for item in state
            .session
            .transcript_items
            .iter()
            .filter(|item| is_valid_sequence(&state.session, item.sequence_number))
        {
            let Some(root_run_id) = state.session.root_run_id(item.run_id) else {
                continue;
            };
            if state.session.root_run_id(root_run_id) != Some(root_run_id) {
                continue;
            }
            if seen_root_run_ids.insert(root_run_id) {
                root_run_ids.push(root_run_id);
            }
        }

        root_run_ids
    }

    fn project_canonical_prompt_turn(
        &self,
        state: &AppState,
        run: &RunRecord,
    ) -> PromptTurnProjection {
        self.project_canonical_prompt_turn_incremental(state, run, None, &HashSet::new())
    }

    fn project_canonical_prompt_turn_incremental(
        &self,
        state: &AppState,
        run: &RunRecord,
        watermark: Option<ReplaySequence>,
        previously_open_item_ids: &HashSet<TranscriptItemId>,
    ) -> PromptTurnProjection {
        let mut events = Vec::new();
        let mut open_transcript_item_ids = Vec::new();
        let mut max_sequence: Option<ReplaySequence> = None;

        // When we have a watermark and no previously-open items to revisit,
        // skip committed items below the watermark using binary search.
        // Items are sorted by sequence_number (maintained by insert_transcript_item_in_order).
        let skip_count = if let Some(watermark) = watermark {
            if previously_open_item_ids.is_empty() {
                state
                    .session
                    .transcript_items
                    .partition_point(|item| item.sequence_number <= watermark)
            } else {
                0
            }
        } else {
            0
        };

        for item in state
            .session
            .transcript_items
            .iter()
            .skip(skip_count)
            .filter(|item| state.session.root_run_id(item.run_id) == Some(run.id))
            .filter(|item| is_valid_sequence(&state.session, item.sequence_number))
        {
            if let Some(watermark) = watermark
                && item.sequence_number <= watermark
                && item.stream_state != TranscriptStreamState::Open
                && !previously_open_item_ids.contains(&item.item_id)
            {
                continue;
            }

            max_sequence = Some(max_sequence.map_or(item.sequence_number, |current| {
                current.max(item.sequence_number)
            }));
            if item.stream_state == TranscriptStreamState::Open {
                open_transcript_item_ids.push(item.item_id);
            }
            self.project_transcript_item(state, item, &mut events);
        }

        events.sort();

        PromptTurnProjection {
            run_id: run.id,
            events,
            terminal_stop: self.terminal_stop(run),
            max_sequence,
            open_transcript_item_ids,
        }
    }

    fn project_legacy_prompt_turn(
        &self,
        state: &AppState,
        run: &RunRecord,
    ) -> PromptTurnProjection {
        let session = &state.session;
        let mut events = Vec::new();

        for turn in session
            .turns
            .iter()
            .filter(|turn| session.root_run_id(turn.run_id) == Some(run.id))
            .filter(|turn| is_valid_sequence(session, turn.sequence_number))
        {
            match turn.role {
                Role::User => {
                    if !turn.content.is_empty() {
                        events.push(OrderedPromptTurnEvent::new(
                            turn.sequence_number,
                            ProjectionEventPhase::UserMessage,
                            PromptTurnEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
                                UserMessageChunk {
                                    content: ContentBlock::text(turn.content.clone()),
                                },
                            )),
                        ));
                    }
                }
                Role::Assistant => {
                    if !turn.reasoning.is_empty() {
                        events.push(OrderedPromptTurnEvent::new(
                            turn.sequence_number,
                            ProjectionEventPhase::AgentThought,
                            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
                                AgentThoughtChunk {
                                    content: ContentBlock::text(turn.reasoning.clone()),
                                },
                            )),
                        ));
                    }

                    if !turn.content.is_empty() {
                        events.push(OrderedPromptTurnEvent::new(
                            turn.sequence_number,
                            ProjectionEventPhase::AgentMessage,
                            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                                AgentMessageChunk {
                                    content: ContentBlock::text(turn.content.clone()),
                                },
                            )),
                        ));
                    }
                }
                Role::System | Role::Tool => {}
            }
        }

        for invocation in session
            .tool_invocations
            .iter()
            .filter(|invocation| session.root_run_id(invocation.run_id) == Some(run.id))
            .filter(|invocation| is_valid_sequence(session, invocation.sequence_number))
        {
            events.push(OrderedPromptTurnEvent::new(
                invocation.sequence_number,
                TOOL_CALL_CREATE_PHASE,
                PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCall(
                    self.tool_call_snapshot(invocation),
                )),
            ));

            if let Some(tool_call_patch) = self.tool_call_patch(invocation) {
                events.push(OrderedPromptTurnEvent::new(
                    invocation.sequence_number,
                    TOOL_CALL_PATCH_PHASE,
                    PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(tool_call_patch)),
                ));
            }

            if let Some(permission_request) = self.permission_request(state, invocation) {
                events.push(OrderedPromptTurnEvent::new(
                    invocation.sequence_number,
                    ProjectionEventPhase::PermissionRequest,
                    PromptTurnEvent::PermissionRequest(permission_request),
                ));
            }
        }

        events.sort();

        PromptTurnProjection {
            run_id: run.id,
            events,
            terminal_stop: self.terminal_stop(run),
            max_sequence: None,
            open_transcript_item_ids: Vec::new(),
        }
    }

    fn project_transcript_item(
        &self,
        state: &AppState,
        item: &TranscriptItemRecord,
        events: &mut Vec<OrderedPromptTurnEvent>,
    ) {
        match &item.content {
            TranscriptItemContent::Turn(content) => {
                self.project_turn_transcript_item(item, content, events);
            }
            TranscriptItemContent::ToolInvocation(content) => {
                events.push(OrderedPromptTurnEvent::new(
                    item.sequence_number,
                    TOOL_CALL_CREATE_PHASE,
                    PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCall(
                        self.tool_call_snapshot_from_transcript(&state.session, item, content),
                    )),
                ));

                if let Some(tool_call_patch) =
                    self.tool_call_patch_from_transcript(&state.session, item, content)
                {
                    events.push(OrderedPromptTurnEvent::new(
                        item.sequence_number,
                        TOOL_CALL_PATCH_PHASE,
                        PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
                            tool_call_patch,
                        )),
                    ));
                }
            }
            TranscriptItemContent::Permission(content) => {
                if let Some(permission_request) = self.permission_request_from_transcript_item(
                    state,
                    &state.session,
                    item,
                    content,
                ) {
                    events.push(OrderedPromptTurnEvent::new(
                        item.sequence_number,
                        ProjectionEventPhase::PermissionRequest,
                        PromptTurnEvent::PermissionRequest(permission_request),
                    ));
                }
            }
            TranscriptItemContent::RunLifecycle(_)
            | TranscriptItemContent::DelegatedChild(_)
            | TranscriptItemContent::Marker(_) => {
                events.push(OrderedPromptTurnEvent::new(
                    item.sequence_number,
                    SESSION_INFO_UPDATE_PHASE,
                    PromptTurnEvent::SessionUpdate(SessionUpdate::SessionInfoUpdate(
                        transcript_item_session_info_update(item),
                    )),
                ));
            }
        }
    }

    fn project_turn_transcript_item(
        &self,
        item: &TranscriptItemRecord,
        content: &fluent_code_app::session::model::TranscriptTurnContent,
        events: &mut Vec<OrderedPromptTurnEvent>,
    ) {
        match content.role {
            Role::User => {
                if !content.content.is_empty() {
                    events.push(OrderedPromptTurnEvent::new(
                        item.sequence_number,
                        ProjectionEventPhase::UserMessage,
                        PromptTurnEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
                            UserMessageChunk {
                                content: ContentBlock::text(content.content.clone()),
                            },
                        )),
                    ));
                }
            }
            Role::Assistant => {
                if !content.reasoning.is_empty() {
                    events.push(OrderedPromptTurnEvent::new(
                        item.sequence_number,
                        ProjectionEventPhase::AgentThought,
                        PromptTurnEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
                            AgentThoughtChunk {
                                content: ContentBlock::text(content.reasoning.clone()),
                            },
                        )),
                    ));
                }

                if !content.content.is_empty() {
                    events.push(OrderedPromptTurnEvent::new(
                        item.sequence_number,
                        ProjectionEventPhase::AgentMessage,
                        PromptTurnEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                            AgentMessageChunk {
                                content: ContentBlock::text(content.content.clone()),
                            },
                        )),
                    ));
                }
            }
            Role::System | Role::Tool => {}
        }
    }

    pub fn tool_call_snapshot(&self, invocation: &ToolInvocationRecord) -> ToolCall {
        ToolCall {
            title: tool_title(&invocation.tool_name, &invocation.tool_source),
            tool_call_id: invocation.tool_call_id.clone(),
            kind: Some(tool_kind(&invocation.tool_name)),
            status: Some(ToolCallStatus::Pending),
            content: None,
            locations: tool_input_locations(&invocation.tool_name, &invocation.arguments),
            raw_input: Some(invocation.arguments.clone()),
            raw_output: None,
            meta: tool_invocation_meta(Some(invocation)),
        }
    }

    fn tool_call_snapshot_from_transcript(
        &self,
        session: &Session,
        item: &TranscriptItemRecord,
        content: &fluent_code_app::session::model::TranscriptToolInvocationContent,
    ) -> ToolCall {
        let arguments = self
            .transcript_tool_invocation(session, item)
            .map(|invocation| invocation.arguments.clone())
            .unwrap_or_else(|| content.arguments.clone());

        ToolCall {
            title: tool_title(&content.tool_name, &content.tool_source),
            tool_call_id: content.tool_call_id.clone(),
            kind: Some(tool_kind(&content.tool_name)),
            status: Some(ToolCallStatus::Pending),
            content: None,
            locations: tool_input_locations(&content.tool_name, &arguments),
            raw_input: Some(arguments),
            raw_output: None,
            meta: tool_invocation_meta(self.transcript_tool_invocation(session, item)),
        }
    }

    pub fn tool_call_patch(&self, invocation: &ToolInvocationRecord) -> Option<ToolCallUpdate> {
        let final_status = final_tool_status(invocation.execution_state);
        if final_status == ToolCallStatus::Pending
            && invocation.result.is_none()
            && invocation.error.is_none()
        {
            return None;
        }

        let mut update = ToolCallUpdate::new(invocation.tool_call_id.clone());
        if final_status != ToolCallStatus::Pending {
            update.status = Some(final_status);
        }

        if let Some(result) = invocation.result.as_ref() {
            update.content = Some(vec![ToolCallContent::text(result.clone())]);
            update.raw_output = Some(json!({ "result": result }));
            update.locations =
                tool_output_locations(&invocation.tool_name, invocation.result.as_deref());
        } else if let Some(error) = invocation.error.as_ref() {
            update.content = Some(vec![ToolCallContent::text(error.clone())]);
            update.raw_output = Some(json!({ "error": error }));
        }

        let mut meta = tool_invocation_meta(Some(invocation)).unwrap_or_default();
        if let Some(terminal_meta) = tool_invocation_terminal_meta(invocation) {
            meta.extend(terminal_meta);
        }
        update.meta = if meta.is_empty() { None } else { Some(meta) };

        (!update.is_empty()).then_some(update)
    }

    fn tool_call_patch_from_transcript(
        &self,
        session: &Session,
        item: &TranscriptItemRecord,
        content: &fluent_code_app::session::model::TranscriptToolInvocationContent,
    ) -> Option<ToolCallUpdate> {
        let final_status = final_tool_status(content.execution_state);
        if final_status == ToolCallStatus::Pending
            && content.result.is_none()
            && content.error.is_none()
        {
            return None;
        }

        let mut update = ToolCallUpdate::new(content.tool_call_id.clone());
        if final_status != ToolCallStatus::Pending {
            update.status = Some(final_status);
        }

        if let Some(result) = content.result.as_ref() {
            update.content = Some(vec![ToolCallContent::text(result.clone())]);
            update.raw_output = Some(json!({ "result": result }));
            update.locations = tool_output_locations(&content.tool_name, Some(result.as_str()));
        } else if let Some(error) = content.error.as_ref() {
            update.content = Some(vec![ToolCallContent::text(error.clone())]);
            update.raw_output = Some(json!({ "error": error }));
        }

        if update.locations.is_none()
            && let Some(invocation) = self.transcript_tool_invocation(session, item)
            && let Some(result) = invocation.result.as_deref()
        {
            update.locations = tool_output_locations(&content.tool_name, Some(result));
        }

        update.meta = tool_invocation_meta(self.transcript_tool_invocation(session, item));

        (!update.is_empty()).then_some(update)
    }

    pub fn permission_request(
        &self,
        state: &AppState,
        invocation: &ToolInvocationRecord,
    ) -> Option<RequestPermissionRequest> {
        if invocation.approval_state != ToolApprovalState::Pending {
            return None;
        }

        if state
            .session
            .pending_tool_invocation_for_batch(invocation.run_id, invocation.preceding_turn_id)
            .map(|pending| pending.id)
            != Some(invocation.id)
        {
            return None;
        }

        let rememberable = state
            .tool_registry
            .tool_policy(&invocation.tool_name)
            .map(|policy| policy.rememberable)
            .unwrap_or(true);

        let mut tool_call = ToolCallUpdate::new(invocation.tool_call_id.clone());
        tool_call.title = Some(tool_title(&invocation.tool_name, &invocation.tool_source));
        tool_call.kind = Some(tool_kind(&invocation.tool_name));
        tool_call.status = Some(ToolCallStatus::Pending);
        tool_call.locations = tool_input_locations(&invocation.tool_name, &invocation.arguments);
        tool_call.raw_input = Some(invocation.arguments.clone());

        Some(RequestPermissionRequest {
            session_id: state.session.id.to_string(),
            tool_call,
            options: permission_options(rememberable),
        })
    }

    fn permission_request_from_transcript_item(
        &self,
        state: &AppState,
        session: &Session,
        item: &TranscriptItemRecord,
        content: &fluent_code_app::session::model::TranscriptPermissionContent,
    ) -> Option<RequestPermissionRequest> {
        if content.state != TranscriptPermissionState::Pending {
            return None;
        }

        let invocation = self.transcript_tool_invocation(session, item)?;
        if state
            .session
            .pending_tool_invocation_for_batch(invocation.run_id, invocation.preceding_turn_id)
            .map(|pending| pending.id)
            != Some(invocation.id)
        {
            return None;
        }

        let rememberable = state
            .tool_registry
            .tool_policy(&content.tool_name)
            .map(|policy| policy.rememberable)
            .unwrap_or(true);

        let mut tool_call = ToolCallUpdate::new(invocation.tool_call_id.clone());
        tool_call.title = Some(tool_title(&content.tool_name, &content.tool_source));
        tool_call.kind = Some(tool_kind(&content.tool_name));
        tool_call.status = Some(ToolCallStatus::Pending);
        tool_call.locations = tool_input_locations(&content.tool_name, &invocation.arguments);
        tool_call.raw_input = Some(invocation.arguments.clone());

        Some(RequestPermissionRequest {
            session_id: state.session.id.to_string(),
            tool_call,
            options: permission_options(rememberable),
        })
    }

    fn transcript_tool_invocation<'a>(
        &self,
        session: &'a Session,
        item: &TranscriptItemRecord,
    ) -> Option<&'a ToolInvocationRecord> {
        let invocation_id = item.tool_invocation_id?;
        session.find_tool_invocation(invocation_id)
    }

    pub fn terminal_stop(&self, run: &RunRecord) -> Option<TerminalStopProjection> {
        let reason = run
            .terminal_stop_reason
            .or_else(|| run.status.default_terminal_stop_reason())?;

        Some(match reason {
            RunTerminalStopReason::Completed => {
                TerminalStopProjection::PromptResponse(StopReason::EndTurn)
            }
            RunTerminalStopReason::Cancelled => {
                TerminalStopProjection::PromptResponse(StopReason::Cancelled)
            }
            RunTerminalStopReason::Failed => {
                TerminalStopProjection::SessionState(RunTerminalStopReason::Failed)
            }
            RunTerminalStopReason::Interrupted => {
                TerminalStopProjection::SessionState(RunTerminalStopReason::Interrupted)
            }
        })
    }
}

fn tool_invocation_meta(invocation: Option<&ToolInvocationRecord>) -> Option<Meta> {
    let invocation = invocation?;
    let mut meta = Meta::new();
    meta.insert(
        ACP_META_TOOL_INVOCATION_KEY.to_string(),
        serde_json::to_value(invocation).ok()?,
    );

    meta.insert(
        ACP_META_TOOL_NAME_KEY.to_string(),
        serde_json::Value::String(invocation.tool_name.clone()),
    );

    if let Some(delegation) = &invocation.delegation {
        if let Some(child_run_id) = delegation.child_run_id {
            let subagent_info = json!({
                "session_id": child_run_id.to_string(),
                "message_start_index": 0,
            });
            meta.insert(
                ACP_META_SUBAGENT_SESSION_INFO_KEY.to_string(),
                subagent_info,
            );
        }
    }

    if is_terminal_tool(&invocation.tool_name) {
        meta.insert(
            ACP_META_TERMINAL_INFO_KEY.to_string(),
            json!({
                "terminal_id": invocation.tool_call_id,
            }),
        );
    }

    Some(meta)
}

fn tool_invocation_terminal_meta(invocation: &ToolInvocationRecord) -> Option<Meta> {
    if !is_terminal_tool(&invocation.tool_name) {
        return None;
    }

    let is_completed = matches!(
        invocation.execution_state,
        ToolExecutionState::Completed | ToolExecutionState::Failed
    );

    if !is_completed {
        return None;
    }

    let mut meta = Meta::new();

    let output_data = invocation
        .result
        .as_deref()
        .or(invocation.error.as_deref())
        .unwrap_or("");

    meta.insert(
        ACP_META_TERMINAL_OUTPUT_KEY.to_string(),
        json!({
            "terminal_id": invocation.tool_call_id,
            "data": output_data,
        }),
    );

    let exit_code = if invocation.error.is_some() { 1 } else { 0 };
    meta.insert(
        ACP_META_TERMINAL_EXIT_KEY.to_string(),
        json!({
            "terminal_id": invocation.tool_call_id,
            "exit_code": exit_code,
        }),
    );

    Some(meta)
}

fn is_terminal_tool(tool_name: &str) -> bool {
    matches!(tool_kind(tool_name), ToolKind::Execute)
}

fn transcript_item_session_info_update(item: &TranscriptItemRecord) -> SessionInfoUpdate {
    let mut meta = Meta::new();
    meta.insert(
        ACP_META_TRANSCRIPT_ITEM_KEY.to_string(),
        serde_json::to_value(item).expect("transcript item metadata should serialize"),
    );

    SessionInfoUpdate {
        title: None,
        updated_at: None,
        meta: Some(meta),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTurnProjection {
    pub run_id: RunId,
    pub events: Vec<OrderedPromptTurnEvent>,
    pub terminal_stop: Option<TerminalStopProjection>,
    pub max_sequence: Option<ReplaySequence>,
    pub open_transcript_item_ids: Vec<TranscriptItemId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedPromptTurnEvent {
    pub sequence: ReplaySequence,
    pub phase: ProjectionEventPhase,
    pub event: PromptTurnEvent,
}

impl OrderedPromptTurnEvent {
    fn new(sequence: ReplaySequence, phase: ProjectionEventPhase, event: PromptTurnEvent) -> Self {
        Self {
            sequence,
            phase,
            event,
        }
    }
}

impl Ord for OrderedPromptTurnEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.sequence, self.phase).cmp(&(other.sequence, other.phase))
    }
}

impl PartialOrd for OrderedPromptTurnEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptTurnEvent {
    SessionUpdate(SessionUpdate),
    PermissionRequest(RequestPermissionRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionEventPhase {
    UserMessage,
    AgentThought,
    AgentMessage,
    SessionInfoUpdate,
    ToolCallCreate,
    ToolCallPatch,
    PermissionRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStopProjection {
    PromptResponse(StopReason),
    SessionState(RunTerminalStopReason),
}

fn is_valid_sequence(session: &Session, sequence: ReplaySequence) -> bool {
    sequence > 0 && sequence < session.next_replay_sequence
}

fn uses_canonical_transcript_projection(session: &Session) -> bool {
    session.transcript_fidelity == TranscriptFidelity::Exact
        && !session.transcript_items.is_empty()
        && session
            .turns
            .iter()
            .all(|turn| turn_has_canonical_transcript_item(session, turn))
        && session
            .tool_invocations
            .iter()
            .all(|invocation| session.find_transcript_item(invocation.id).is_some())
}

fn turn_has_canonical_transcript_item(
    session: &Session,
    turn: &fluent_code_app::session::model::Turn,
) -> bool {
    match turn.role {
        Role::Assistant => {
            let has_reasoning_item = turn.reasoning.is_empty()
                || session
                    .find_transcript_item(transcript_assistant_reasoning_item_id(turn.id))
                    .is_some();
            let has_text_item = turn.content.is_empty()
                || session
                    .find_transcript_item(transcript_assistant_text_item_id(turn.id))
                    .is_some()
                || session.find_transcript_item(turn.id).is_some();

            has_reasoning_item && has_text_item
        }
        Role::User | Role::System | Role::Tool => session.find_transcript_item(turn.id).is_some(),
    }
}

fn final_tool_status(execution_state: ToolExecutionState) -> ToolCallStatus {
    match execution_state {
        ToolExecutionState::NotStarted => ToolCallStatus::Pending,
        ToolExecutionState::Running => ToolCallStatus::InProgress,
        ToolExecutionState::Completed => ToolCallStatus::Completed,
        ToolExecutionState::Failed | ToolExecutionState::Skipped => ToolCallStatus::Failed,
    }
}

fn permission_options(rememberable: bool) -> Vec<PermissionOption> {
    let mut options = vec![
        PermissionOption {
            option_id: "allow_once".to_string(),
            name: "Allow once".to_string(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: "reject_once".to_string(),
            name: "Reject once".to_string(),
            kind: PermissionOptionKind::RejectOnce,
        },
    ];

    if rememberable {
        options.insert(
            1,
            PermissionOption {
                option_id: "allow_always".to_string(),
                name: "Always allow".to_string(),
                kind: PermissionOptionKind::AllowAlways,
            },
        );
        options.push(PermissionOption {
            option_id: "reject_always".to_string(),
            name: "Always reject".to_string(),
            kind: PermissionOptionKind::RejectAlways,
        });
    }

    options
}

fn tool_input_locations(
    tool_name: &str,
    arguments: &serde_json::Value,
) -> Option<Vec<ToolCallLocation>> {
    let line = arguments
        .get("offset")
        .and_then(|value| value.as_u64())
        .filter(|line| *line > 0);

    match tool_name {
        "read" => arguments
            .get("path")
            .and_then(|value| value.as_str())
            .and_then(|path| absolute_tool_location(path, line))
            .map(|location| vec![location]),
        "glob" | "grep" => arguments
            .get("path")
            .and_then(|value| value.as_str())
            .and_then(|path| absolute_tool_location(path, None))
            .map(|location| vec![location]),
        _ => None,
    }
}

fn tool_output_locations(tool_name: &str, result: Option<&str>) -> Option<Vec<ToolCallLocation>> {
    let result = result?;
    let mut locations = match tool_name {
        "read" => parse_read_output_locations(result),
        "glob" => parse_path_listing_locations(result),
        "grep" => parse_grep_output_locations(result),
        _ => Vec::new(),
    };

    locations.dedup();
    (!locations.is_empty()).then_some(locations)
}

fn parse_read_output_locations(output: &str) -> Vec<ToolCallLocation> {
    let Some(first_line) = output.lines().next() else {
        return Vec::new();
    };

    let Some(path) = first_line
        .strip_prefix("<path>")
        .and_then(|line| line.strip_suffix("</path>"))
    else {
        return Vec::new();
    };

    let line = output
        .lines()
        .skip(1)
        .find_map(|line| line.split_once(':')?.0.parse::<u64>().ok())
        .filter(|line| *line > 0);

    absolute_tool_location(path, line).into_iter().collect()
}

fn parse_path_listing_locations(output: &str) -> Vec<ToolCallLocation> {
    output
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("Found "))
        .filter_map(|line| absolute_tool_location(line.trim_end_matches('/'), None))
        .collect()
}

fn parse_grep_output_locations(output: &str) -> Vec<ToolCallLocation> {
    output
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("Found "))
        .filter_map(parse_grep_output_location)
        .collect()
}

fn parse_grep_output_location(line: &str) -> Option<ToolCallLocation> {
    if let Some((path_and_line, _)) = line.split_once(": ")
        && let Some((path, line_number)) = path_and_line.rsplit_once(':')
        && let Ok(line_number) = line_number.parse::<u64>()
        && line_number > 0
        && Path::new(path).is_absolute()
    {
        return absolute_tool_location(path, Some(line_number));
    }

    if let Some((path, count)) = line.rsplit_once(": ")
        && count.parse::<u64>().is_ok()
        && Path::new(path).is_absolute()
    {
        return absolute_tool_location(path, None);
    }

    absolute_tool_location(line.trim_end_matches('/'), None)
}

fn absolute_tool_location(path: &str, line: Option<u64>) -> Option<ToolCallLocation> {
    Path::new(path).is_absolute().then(|| ToolCallLocation {
        path: path.replace('\\', "/"),
        line,
    })
}

fn tool_title(tool_name: &str, tool_source: &ToolSource) -> String {
    let base_title = tool_name.replace('_', " ");
    match tool_source {
        ToolSource::BuiltIn => base_title,
        ToolSource::Plugin { plugin_name, .. } => format!("{base_title} ({plugin_name})"),
    }
}

fn tool_kind(tool_name: &str) -> ToolKind {
    let normalized_name = tool_name.to_ascii_lowercase();

    match normalized_name.as_str() {
        "read" => ToolKind::Read,
        "glob" | "grep" => ToolKind::Search,
        "task" => ToolKind::Think,
        "uppercase_text" => ToolKind::Edit,
        _ if normalized_name.contains("read") => ToolKind::Read,
        _ if normalized_name.contains("search")
            || normalized_name.contains("grep")
            || normalized_name.contains("glob") =>
        {
            ToolKind::Search
        }
        _ if normalized_name.contains("write")
            || normalized_name.contains("edit")
            || normalized_name.contains("patch") =>
        {
            ToolKind::Edit
        }
        _ if normalized_name.contains("delete") || normalized_name.contains("remove") => {
            ToolKind::Delete
        }
        _ if normalized_name.contains("move") || normalized_name.contains("rename") => {
            ToolKind::Move
        }
        _ if normalized_name.contains("fetch") => ToolKind::Fetch,
        _ if normalized_name.contains("bash")
            || normalized_name.contains("exec")
            || normalized_name.contains("run") =>
        {
            ToolKind::Execute
        }
        _ => ToolKind::Other,
    }
}

fn session_config_options(defaults: &AcpSessionDefaultsConfig) -> Vec<SessionConfigOption> {
    vec![
        SessionConfigOption {
            id: SYSTEM_PROMPT_CONFIG_ID.to_string(),
            name: "System Prompt".to_string(),
            description: Some("Configured ACP session system prompt.".to_string()),
            category: None,
            kind: SessionConfigKind::Select(SessionConfigSelect {
                current_value: defaults.system_prompt.clone(),
                options: vec![SessionConfigSelectOption {
                    value: defaults.system_prompt.clone(),
                    name: "Configured system prompt".to_string(),
                    description: Some(defaults.system_prompt.clone()),
                }],
            }),
        },
        SessionConfigOption {
            id: REASONING_EFFORT_CONFIG_ID.to_string(),
            name: "Reasoning Effort".to_string(),
            description: Some("Configured ACP session reasoning effort.".to_string()),
            category: Some(SessionConfigOptionCategory::ThoughtLevel),
            kind: SessionConfigKind::Select(SessionConfigSelect {
                current_value: defaults
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| NO_REASONING_EFFORT_VALUE.to_string()),
                options: reasoning_effort_options(),
            }),
        },
    ]
}

fn reasoning_effort_options() -> Vec<SessionConfigSelectOption> {
    [
        ("none", "None", "Disable extra reasoning effort."),
        ("minimal", "Minimal", "Use minimal reasoning effort."),
        ("low", "Low", "Use low reasoning effort."),
        ("medium", "Medium", "Use medium reasoning effort."),
        ("high", "High", "Use high reasoning effort."),
        ("xhigh", "XHigh", "Use extra-high reasoning effort."),
    ]
    .into_iter()
    .map(|(value, name, description)| SessionConfigSelectOption {
        value: value.to_string(),
        name: name.to_string(),
        description: Some(description.to_string()),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::Utc;
    use fluent_code_app::app::AppState;
    use fluent_code_app::session::model::{
        Role, RunRecord, RunStatus, RunTerminalStopReason, Session, TaskDelegationRecord,
        TaskDelegationStatus, ToolApprovalState, ToolExecutionState, ToolInvocationRecord,
        ToolSource, TranscriptItemContent, TranscriptItemRecord, TranscriptPermissionState,
        TranscriptStreamState, Turn, transcript_assistant_text_item_id,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        ACP_META_TOOL_INVOCATION_KEY, ACP_META_TRANSCRIPT_ITEM_KEY, OrderedPromptTurnEvent,
        ProjectionEventPhase, PromptTurnEvent, SessionUpdateMapper, TerminalStopProjection,
    };
    use crate::protocol::{
        ContentBlock, PermissionOptionKind, SessionConfigKind, SessionUpdate, StopReason,
        ToolCallLocation, ToolCallStatus, ToolKind,
    };

    #[test]
    fn mapper_exposes_server_info_for_bootstrap() {
        let server_info = SessionUpdateMapper::new().server_info();

        assert_eq!(server_info.name, "fluent-code");
        assert_eq!(server_info.title.as_deref(), Some("Fluent Code"));
        assert!(!server_info.version.is_empty());
    }

    #[test]
    fn initialize_response_omits_unsupported_capabilities() {
        let mapper = SessionUpdateMapper::new();
        let response = mapper.initialize_response(99);
        let json = serde_json::to_value(response).unwrap();

        assert_eq!(json["protocolVersion"], 1);
        assert_eq!(json["agentCapabilities"]["loadSession"], true);
        assert_eq!(json["agentCapabilities"]["resumeSession"], true);
        assert_eq!(json["agentCapabilities"]["closeSession"], true);
        assert_eq!(json["agentCapabilities"]["mcpCapabilities"]["http"], true);
        assert_eq!(json["agentCapabilities"]["mcpCapabilities"]["sse"], true);
        assert!(
            json["agentCapabilities"]["sessionCapabilities"]["list"]
                .is_object()
        );
        assert!(
            json["agentCapabilities"]
                .get("promptCapabilities")
                .is_none()
        );
        assert!(json.get("authMethods").is_none());
    }

    #[test]
    fn initialize_response_uses_configured_auth_methods() {
        let mapper = SessionUpdateMapper::from_acp_config(&fluent_code_app::config::AcpConfig {
            protocol_version: 1,
            auth_methods: vec![fluent_code_app::config::AcpAuthMethodConfig {
                id: "api_key".to_string(),
                name: "API key".to_string(),
                description: Some("Provide a bearer token.".to_string()),
            }],
            session_defaults: fluent_code_app::config::AcpSessionDefaultsConfig {
                system_prompt: "You are a helpful coding assistant.".to_string(),
                reasoning_effort: None,
            },
        });
        let json = serde_json::to_value(mapper.initialize_response(1)).unwrap();

        assert_eq!(json["authMethods"][0]["id"], "api_key");
        assert_eq!(json["authMethods"][0]["name"], "API key");
    }

    #[test]
    fn session_config_options_follow_acp_session_defaults() {
        let mapper = SessionUpdateMapper::from_acp_config(&fluent_code_app::config::AcpConfig {
            protocol_version: 1,
            auth_methods: Vec::new(),
            session_defaults: fluent_code_app::config::AcpSessionDefaultsConfig {
                system_prompt: "ACP prompt".to_string(),
                reasoning_effort: Some("medium".to_string()),
            },
        });

        let config_options = mapper.session_config_options().unwrap();

        assert_eq!(config_options.len(), 2);
        assert_eq!(config_options[0].id, "system_prompt");
        assert!(matches!(
            config_options[0].kind,
            SessionConfigKind::Select(_)
        ));
        assert_eq!(config_options[1].id, "reasoning_effort");
        assert_eq!(
            serde_json::to_value(&config_options[1]).unwrap()["category"],
            "thought_level"
        );

        let reasoning_option = serde_json::to_value(&config_options[1]).unwrap();
        assert_eq!(reasoning_option["currentValue"], "medium");
        assert_eq!(reasoning_option["options"][0]["value"], "none");
        assert_eq!(reasoning_option["options"][3]["value"], "medium");
    }

    #[test]
    fn completed_prompt_turn_projects_text_reasoning_tool_lifecycle_and_end_turn() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::Completed,
            Some(fluent_code_app::session::model::RunTerminalStopReason::Completed),
            vec![
                user_turn(run_id, 1, "write a summary"),
                assistant_turn(run_id, 3, "final answer", "thinking"),
            ],
            vec![tool_invocation(
                invocation_id,
                run_id,
                ("read-call-1", "read"),
                4,
                ToolApprovalState::Approved,
                ToolExecutionState::Completed,
                (Some("<path>/tmp/notes.txt</path>\n1: alpha\n2: beta"), None),
            )],
            6,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();

        assert_eq!(
            projection
                .events
                .iter()
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::UserMessage, "session_update"),
                (3, ProjectionEventPhase::AgentThought, "session_update"),
                (3, ProjectionEventPhase::AgentMessage, "session_update"),
                (4, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (4, ProjectionEventPhase::ToolCallPatch, "session_update"),
            ]
        );

        assert!(matches!(
            projection.terminal_stop,
            Some(TerminalStopProjection::PromptResponse(StopReason::EndTurn))
        ));

        match &projection.events[1].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(chunk)) => {
                assert_eq!(chunk.content, ContentBlock::text("thinking"));
            }
            other => panic!("expected reasoning update, got {other:?}"),
        }

        match &projection.events[3].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCall(tool_call)) => {
                assert_eq!(tool_call.tool_call_id, "read-call-1");
                assert_eq!(tool_call.kind, Some(ToolKind::Read));
                assert_eq!(tool_call.status, Some(ToolCallStatus::Pending));
                assert_eq!(tool_call.raw_input, Some(json!({"path":"/tmp/notes.txt"})));
                assert_eq!(
                    tool_call.locations,
                    Some(vec![ToolCallLocation {
                        path: "/tmp/notes.txt".to_string(),
                        line: None,
                    }])
                );
            }
            other => panic!("expected tool call snapshot, got {other:?}"),
        }

        match &projection.events[4].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(tool_call_update)) => {
                assert_eq!(tool_call_update.tool_call_id, "read-call-1");
                assert_eq!(tool_call_update.status, Some(ToolCallStatus::Completed));
                assert_eq!(
                    tool_call_update.raw_output,
                    Some(json!({"result":"<path>/tmp/notes.txt</path>\n1: alpha\n2: beta"}))
                );
                assert_eq!(
                    tool_call_update.locations,
                    Some(vec![ToolCallLocation {
                        path: "/tmp/notes.txt".to_string(),
                        line: Some(1),
                    }])
                );
            }
            other => panic!("expected tool call patch, got {other:?}"),
        }
    }

    #[test]
    fn relative_tool_paths_are_sanitized_out_of_acp_locations() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let mut invocation = tool_invocation(
            invocation_id,
            run_id,
            ("read-call-relative", "read"),
            3,
            ToolApprovalState::Approved,
            ToolExecutionState::Completed,
            (Some("<path>notes.txt</path>\n2: beta"), None),
        );
        invocation.arguments = json!({"path":"notes.txt"});

        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::Completed,
            Some(fluent_code_app::session::model::RunTerminalStopReason::Completed),
            vec![user_turn(run_id, 1, "read it")],
            vec![invocation],
            5,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();

        match &projection.events[1].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCall(tool_call)) => {
                assert_eq!(tool_call.locations, None);
            }
            other => panic!("expected tool call snapshot, got {other:?}"),
        }

        match &projection.events[2].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(tool_call_update)) => {
                assert_eq!(tool_call_update.locations, None);
            }
            other => panic!("expected tool call patch, got {other:?}"),
        }
    }

    #[test]
    fn pending_permission_request_projects_after_tool_creation_with_nonrememberable_options() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::InProgress,
            None,
            vec![user_turn(run_id, 1, "delegate this")],
            vec![tool_invocation(
                invocation_id,
                run_id,
                ("task-call-1", "task"),
                3,
                ToolApprovalState::Pending,
                ToolExecutionState::NotStarted,
                (None, None),
            )],
            5,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();

        assert_eq!(projection.events.len(), 3);
        assert_eq!(
            projection.events[1].phase,
            ProjectionEventPhase::ToolCallCreate
        );
        assert_eq!(
            projection.events[2].phase,
            ProjectionEventPhase::PermissionRequest
        );

        match &projection.events[2].event {
            PromptTurnEvent::PermissionRequest(request) => {
                assert_eq!(request.tool_call.tool_call_id, "task-call-1");
                assert_eq!(request.tool_call.kind, Some(ToolKind::Think));
                assert_eq!(request.tool_call.status, Some(ToolCallStatus::Pending));
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.kind)
                        .collect::<Vec<_>>(),
                    vec![
                        PermissionOptionKind::AllowOnce,
                        PermissionOptionKind::RejectOnce,
                    ]
                );
            }
            other => panic!("expected permission request, got {other:?}"),
        }
    }

    #[test]
    fn pending_permission_request_is_emitted_once_for_the_current_batch() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let shared_turn_id = Uuid::new_v4();
        let first_invocation_id = Uuid::new_v4();
        let second_invocation_id = Uuid::new_v4();
        let mut first_invocation = tool_invocation(
            first_invocation_id,
            run_id,
            ("read-call-1", "read"),
            3,
            ToolApprovalState::Pending,
            ToolExecutionState::NotStarted,
            (None, None),
        );
        first_invocation.preceding_turn_id = Some(shared_turn_id);
        first_invocation.arguments = json!({"path":"/tmp/first.txt"});

        let mut second_invocation = tool_invocation(
            second_invocation_id,
            run_id,
            ("glob-call-2", "glob"),
            4,
            ToolApprovalState::Pending,
            ToolExecutionState::NotStarted,
            (None, None),
        );
        second_invocation.preceding_turn_id = Some(shared_turn_id);
        second_invocation.arguments = json!({"pattern":"**/*.rs","path":"/tmp/project"});

        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::InProgress,
            None,
            vec![user_turn(run_id, 1, "inspect the repo")],
            vec![first_invocation, second_invocation],
            6,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();
        let permission_requests = projection
            .events
            .iter()
            .filter_map(|event| match &event.event {
                PromptTurnEvent::PermissionRequest(request) => Some(request),
                PromptTurnEvent::SessionUpdate(_) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(permission_requests.len(), 1);
        assert_eq!(permission_requests[0].tool_call.tool_call_id, "glob-call-2");
        assert_eq!(
            permission_requests[0].tool_call.locations,
            Some(vec![ToolCallLocation {
                path: "/tmp/project".to_string(),
                line: None,
            }])
        );
    }

    #[test]
    fn cancelled_prompt_turn_maps_to_cancelled_stop_reason() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::Cancelled,
            Some(fluent_code_app::session::model::RunTerminalStopReason::Cancelled),
            vec![user_turn(run_id, 1, "cancel me")],
            Vec::new(),
            4,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();

        assert!(matches!(
            projection.terminal_stop,
            Some(TerminalStopProjection::PromptResponse(
                StopReason::Cancelled
            ))
        ));
    }

    #[test]
    fn interrupted_load_projection_preserves_durable_terminal_state_and_failed_tool_patch() {
        let mapper = SessionUpdateMapper::new();
        let run_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let state = AppState::new(session_with_records(
            run_id,
            RunStatus::Failed,
            Some(fluent_code_app::session::model::RunTerminalStopReason::Interrupted),
            vec![user_turn(run_id, 1, "recover startup")],
            vec![tool_invocation(
                invocation_id,
                run_id,
                ("read-call-2", "read"),
                3,
                ToolApprovalState::Approved,
                ToolExecutionState::Failed,
                (None, Some("interrupted during startup recovery")),
            )],
            6,
        ));

        let projection = mapper.project_prompt_turn(&state, run_id).unwrap();

        assert!(matches!(
            projection.terminal_stop,
            Some(TerminalStopProjection::SessionState(
                fluent_code_app::session::model::RunTerminalStopReason::Interrupted,
            ))
        ));

        match &projection.events[2].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(tool_call_update)) => {
                assert_eq!(tool_call_update.status, Some(ToolCallStatus::Failed));
                assert_eq!(
                    tool_call_update.raw_output,
                    Some(json!({"error":"interrupted during startup recovery"}))
                );
            }
            other => panic!("expected failed tool patch, got {other:?}"),
        }
    }

    #[test]
    fn replay_projects_canonical_transcript_items_in_exact_order() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let root_assistant_turn_id = Uuid::new_v4();
        let child_user_turn = user_turn(child_run_id, 43, "inspect child state");
        let child_assistant_turn = assistant_turn(child_run_id, 44, "Child summary", "");
        let now = Utc::now();

        let root_user_turn = user_turn(root_run_id, 41, "inspect and delegate");
        let root_assistant_turn =
            assistant_turn(root_run_id, 42, "I will inspect the repo.", "plan first");

        let mut pending_invocation = tool_invocation(
            Uuid::new_v4(),
            root_run_id,
            ("glob-call-1", "glob"),
            60,
            ToolApprovalState::Pending,
            ToolExecutionState::NotStarted,
            (None, None),
        );
        pending_invocation.preceding_turn_id = Some(root_assistant_turn_id);
        pending_invocation.arguments = json!({"pattern":"**/*.rs","path":"/tmp/project"});

        let mut child_invocation = tool_invocation(
            Uuid::new_v4(),
            child_run_id,
            ("read-call-2", "read"),
            61,
            ToolApprovalState::Approved,
            ToolExecutionState::Completed,
            (Some("<path>/tmp/child.txt</path>\n1: child output"), None),
        );
        child_invocation.preceding_turn_id = Some(child_assistant_turn.id);
        child_invocation.arguments = json!({"path":"/tmp/child.txt"});

        let mut session = Session::new("exact canonical projection");
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
                status: RunStatus::InProgress,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(pending_invocation.id),
                created_sequence: 90,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            root_user_turn.clone(),
            Turn {
                id: root_assistant_turn_id,
                ..root_assistant_turn.clone()
            },
            child_user_turn.clone(),
            child_assistant_turn.clone(),
        ];
        session.tool_invocations = vec![pending_invocation.clone(), child_invocation.clone()];
        session.transcript_items = vec![
            TranscriptItemRecord::from_turn(&Turn {
                sequence_number: 1,
                ..root_user_turn.clone()
            }),
            TranscriptItemRecord::assistant_reasoning(
                root_run_id,
                root_assistant_turn_id,
                2,
                "plan first",
                fluent_code_app::session::model::TranscriptStreamState::Committed,
            ),
            TranscriptItemRecord::assistant_text(
                root_run_id,
                root_assistant_turn_id,
                3,
                "I will inspect the repo.",
                fluent_code_app::session::model::TranscriptStreamState::Committed,
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
                ..child_user_turn.clone()
            }),
            TranscriptItemRecord::from_turn(&Turn {
                sequence_number: 7,
                ..child_assistant_turn.clone()
            }),
            TranscriptItemRecord::from_tool_invocation(&ToolInvocationRecord {
                sequence_number: 8,
                ..child_invocation.clone()
            }),
        ];
        session.next_replay_sequence = 100;
        session.rebuild_run_indexes();

        let state = AppState::new(session);
        let projection = mapper.project_prompt_turn(&state, root_run_id).unwrap();

        assert_eq!(
            projection
                .events
                .iter()
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::UserMessage, "session_update"),
                (2, ProjectionEventPhase::AgentThought, "session_update"),
                (3, ProjectionEventPhase::AgentMessage, "session_update"),
                (4, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (
                    5,
                    ProjectionEventPhase::PermissionRequest,
                    "permission_request"
                ),
                (6, ProjectionEventPhase::UserMessage, "session_update"),
                (7, ProjectionEventPhase::AgentMessage, "session_update"),
                (8, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (8, ProjectionEventPhase::ToolCallPatch, "session_update"),
            ]
        );

        match &projection.events[4].event {
            PromptTurnEvent::PermissionRequest(request) => {
                assert_eq!(request.tool_call.tool_call_id, "glob-call-1");
                assert_eq!(
                    request.tool_call.locations.as_ref().unwrap()[0].path,
                    "/tmp/project"
                );
            }
            other => panic!("expected permission request, got {other:?}"),
        }

        match &projection.events[8].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
                assert_eq!(update.tool_call_id, "read-call-2");
                assert_eq!(update.status, Some(ToolCallStatus::Completed));
                assert_eq!(update.locations.as_ref().unwrap()[0].path, "/tmp/child.txt");
            }
            other => panic!("expected child tool patch, got {other:?}"),
        }
    }

    #[test]
    fn canonical_replay_contract_orders_thought_text_tool_permission_events() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let root_assistant_turn_id = Uuid::new_v4();
        let child_assistant_turn_id = Uuid::new_v4();
        let task_invocation_id = Uuid::new_v4();
        let now = Utc::now();
        let mut session = Session::new("canonical replay contract");

        session.runs = vec![
            RunRecord {
                id: root_run_id,
                status: RunStatus::InProgress,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 2,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: child_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(task_invocation_id),
                created_sequence: 6,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            user_turn(root_run_id, 1, "delegate and inspect"),
            Turn {
                id: root_assistant_turn_id,
                run_id: root_run_id,
                role: Role::Assistant,
                content: "I will inspect the repo and delegate follow-up work.".to_string(),
                reasoning: "first think".to_string(),
                sequence_number: 3,
                timestamp: now,
            },
            user_turn(child_run_id, 6, "Inspect delegated child state"),
            Turn {
                id: child_assistant_turn_id,
                run_id: child_run_id,
                role: Role::Assistant,
                content: "Delegated child summary".to_string(),
                reasoning: "child thinks".to_string(),
                sequence_number: 7,
                timestamp: now,
            },
            Turn {
                id: Uuid::new_v4(),
                run_id: root_run_id,
                role: Role::Assistant,
                content: "Final answer after permission".to_string(),
                reasoning: String::new(),
                sequence_number: 10,
                timestamp: now,
            },
        ];

        let mut completed_read = tool_invocation(
            Uuid::new_v4(),
            root_run_id,
            ("read-call-1", "read"),
            4,
            ToolApprovalState::Approved,
            ToolExecutionState::Completed,
            (Some("root output"), None),
        );
        completed_read.preceding_turn_id = Some(root_assistant_turn_id);
        completed_read.arguments = json!({"path":"/tmp/root.txt"});

        let mut delegated_task = tool_invocation(
            task_invocation_id,
            root_run_id,
            ("task-call-1", "task"),
            5,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        delegated_task.preceding_turn_id = Some(root_assistant_turn_id);
        delegated_task.arguments =
            json!({"agent":"explore","prompt":"Inspect delegated child state"});
        delegated_task.delegation = Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect delegated child state".to_string()),
            status: TaskDelegationStatus::Running,
        });

        let mut child_read = tool_invocation(
            Uuid::new_v4(),
            child_run_id,
            ("read-call-2", "read"),
            8,
            ToolApprovalState::Approved,
            ToolExecutionState::Completed,
            (Some("child output"), None),
        );
        child_read.preceding_turn_id = Some(child_assistant_turn_id);
        child_read.arguments = json!({"path":"/tmp/child.txt"});

        let mut pending_permission = tool_invocation(
            Uuid::new_v4(),
            root_run_id,
            ("glob-call-3", "glob"),
            9,
            ToolApprovalState::Pending,
            ToolExecutionState::NotStarted,
            (None, None),
        );
        pending_permission.preceding_turn_id = Some(root_assistant_turn_id);
        pending_permission.arguments = json!({"pattern":"**/*.rs","path":"/tmp/project"});

        session.tool_invocations = vec![
            completed_read,
            delegated_task,
            child_read,
            pending_permission,
        ];
        session.next_replay_sequence = 11;
        session.rebuild_run_indexes();

        let state = AppState::new(session);
        let projection = mapper.project_prompt_turn(&state, root_run_id).unwrap();

        assert!(mapper.project_prompt_turn(&state, child_run_id).is_none());
        assert_eq!(
            projection
                .events
                .iter()
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::UserMessage, "session_update"),
                (3, ProjectionEventPhase::AgentThought, "session_update"),
                (3, ProjectionEventPhase::AgentMessage, "session_update"),
                (4, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (4, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (5, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (5, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (6, ProjectionEventPhase::UserMessage, "session_update"),
                (7, ProjectionEventPhase::AgentThought, "session_update"),
                (7, ProjectionEventPhase::AgentMessage, "session_update"),
                (8, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (8, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (9, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (
                    9,
                    ProjectionEventPhase::PermissionRequest,
                    "permission_request"
                ),
                (10, ProjectionEventPhase::AgentMessage, "session_update"),
            ]
        );

        match &projection.events[6].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
                assert_eq!(update.tool_call_id, "task-call-1");
                assert_eq!(update.status, Some(ToolCallStatus::InProgress));
            }
            other => panic!("expected delegated task patch, got {other:?}"),
        }

        match &projection.events[13].event {
            PromptTurnEvent::PermissionRequest(request) => {
                assert_eq!(request.tool_call.tool_call_id, "glob-call-3");
                assert_eq!(request.tool_call.kind, Some(ToolKind::Search));
            }
            other => panic!("expected permission request, got {other:?}"),
        }

        match &projection.events[14].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
                assert_eq!(
                    chunk.content,
                    ContentBlock::text("Final answer after permission")
                );
            }
            other => panic!("expected final assistant message, got {other:?}"),
        }
    }

    #[test]
    fn project_prompt_turns_orders_root_runs_by_durable_run_sequence() {
        let mapper = SessionUpdateMapper::new();
        let first_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let second_run_id = Uuid::new_v4();
        let now = Utc::now();
        let mut session = Session::new("ordering");
        session.runs = vec![
            RunRecord {
                id: second_run_id,
                status: RunStatus::Completed,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 9,
                terminal_sequence: Some(10),
                terminal_stop_reason: Some(
                    fluent_code_app::session::model::RunTerminalStopReason::Completed,
                ),
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: child_run_id,
                status: RunStatus::Completed,
                parent_run_id: Some(first_run_id),
                parent_tool_invocation_id: Some(Uuid::new_v4()),
                created_sequence: 5,
                terminal_sequence: Some(6),
                terminal_stop_reason: Some(
                    fluent_code_app::session::model::RunTerminalStopReason::Completed,
                ),
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: first_run_id,
                status: RunStatus::Completed,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 2,
                terminal_sequence: Some(4),
                terminal_stop_reason: Some(
                    fluent_code_app::session::model::RunTerminalStopReason::Completed,
                ),
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            user_turn(first_run_id, 1, "first"),
            user_turn(child_run_id, 7, "child"),
            user_turn(second_run_id, 8, "second"),
        ];
        session.next_replay_sequence = 11;
        session.rebuild_run_indexes();

        let projections = mapper.project_prompt_turns(&AppState::new(session));

        assert_eq!(
            projections
                .iter()
                .map(|projection| projection.run_id)
                .collect::<Vec<_>>(),
            vec![first_run_id, second_run_id]
        );
    }

    #[test]
    fn project_prompt_turn_preserves_root_grouping_with_cached_root_lookup() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let root_task_invocation_id = Uuid::new_v4();
        let child_tool_invocation_id = Uuid::new_v4();
        let child_assistant_turn_id = Uuid::new_v4();
        let now = Utc::now();
        let mut session = Session::new("delegated projection");
        session.runs = vec![
            RunRecord {
                id: root_run_id,
                status: RunStatus::InProgress,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 2,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: child_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(root_task_invocation_id),
                created_sequence: 6,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            user_turn(root_run_id, 1, "delegate work"),
            assistant_turn(root_run_id, 3, "I will delegate that task.", "planning"),
            user_turn(child_run_id, 7, "Inspect cancellation flow"),
            Turn {
                id: child_assistant_turn_id,
                run_id: child_run_id,
                role: Role::Assistant,
                content: "Child summary".to_string(),
                reasoning: "child reasoning".to_string(),
                sequence_number: 8,
                timestamp: now,
            },
        ];

        let mut root_task_invocation = tool_invocation(
            root_task_invocation_id,
            root_run_id,
            ("task-call-1", "task"),
            5,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        root_task_invocation.preceding_turn_id = Some(session.turns[1].id);

        let mut child_tool_invocation = tool_invocation(
            child_tool_invocation_id,
            child_run_id,
            ("read-call-2", "read"),
            9,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        child_tool_invocation.preceding_turn_id = Some(child_assistant_turn_id);
        child_tool_invocation.arguments = json!({"path":"/tmp/child.txt"});

        session.tool_invocations = vec![root_task_invocation, child_tool_invocation];
        session.next_replay_sequence = 10;
        session.rebuild_run_indexes();

        let state = AppState::new(session);
        let projection = mapper.project_prompt_turn(&state, root_run_id).unwrap();
        assert!(mapper.project_prompt_turn(&state, child_run_id).is_none());

        assert_eq!(
            projection
                .events
                .iter()
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::UserMessage, "session_update"),
                (3, ProjectionEventPhase::AgentThought, "session_update"),
                (3, ProjectionEventPhase::AgentMessage, "session_update"),
                (5, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (5, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (7, ProjectionEventPhase::UserMessage, "session_update"),
                (8, ProjectionEventPhase::AgentThought, "session_update"),
                (8, ProjectionEventPhase::AgentMessage, "session_update"),
                (9, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (9, ProjectionEventPhase::ToolCallPatch, "session_update"),
            ]
        );

        match &projection.events[6].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(chunk)) => {
                assert_eq!(chunk.content, ContentBlock::text("child reasoning"));
            }
            other => panic!("expected delegated child reasoning update, got {other:?}"),
        }

        match &projection.events[7].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
                assert_eq!(chunk.content, ContentBlock::text("Child summary"));
            }
            other => panic!("expected delegated child message update, got {other:?}"),
        }
    }

    #[test]
    fn delegated_child_projection_ignores_broken_lineage_with_cached_lookup() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let valid_child_run_id = Uuid::new_v4();
        let broken_child_run_id = Uuid::new_v4();
        let cycle_a_run_id = Uuid::new_v4();
        let cycle_b_run_id = Uuid::new_v4();
        let missing_parent_run_id = Uuid::new_v4();
        let root_task_invocation_id = Uuid::new_v4();
        let valid_child_tool_invocation_id = Uuid::new_v4();
        let broken_child_tool_invocation_id = Uuid::new_v4();
        let cycle_tool_invocation_id = Uuid::new_v4();
        let now = Utc::now();
        let mut session = Session::new("broken delegated projection");
        session.runs = vec![
            RunRecord {
                id: root_run_id,
                status: RunStatus::InProgress,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 2,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: valid_child_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(root_task_invocation_id),
                created_sequence: 6,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: broken_child_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(missing_parent_run_id),
                parent_tool_invocation_id: Some(root_task_invocation_id),
                created_sequence: 7,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: cycle_a_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(cycle_b_run_id),
                parent_tool_invocation_id: Some(root_task_invocation_id),
                created_sequence: 8,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
            RunRecord {
                id: cycle_b_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(cycle_a_run_id),
                parent_tool_invocation_id: Some(root_task_invocation_id),
                created_sequence: 9,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
        ];
        session.turns = vec![
            user_turn(root_run_id, 1, "delegate work"),
            assistant_turn(root_run_id, 3, "I will delegate that task.", "planning"),
            user_turn(valid_child_run_id, 10, "Inspect cancellation flow"),
            assistant_turn(valid_child_run_id, 11, "Child summary", "child reasoning"),
            user_turn(broken_child_run_id, 12, "broken lineage should not replay"),
            assistant_turn(
                cycle_a_run_id,
                13,
                "cycle should not replay",
                "cycle reasoning",
            ),
        ];

        let mut root_task_invocation = tool_invocation(
            root_task_invocation_id,
            root_run_id,
            ("task-call-1", "task"),
            5,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        root_task_invocation.preceding_turn_id = Some(session.turns[1].id);

        let mut valid_child_tool_invocation = tool_invocation(
            valid_child_tool_invocation_id,
            valid_child_run_id,
            ("read-call-2", "read"),
            14,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        valid_child_tool_invocation.preceding_turn_id = Some(session.turns[3].id);
        valid_child_tool_invocation.arguments = json!({"path":"/tmp/child.txt"});

        let mut broken_child_tool_invocation = tool_invocation(
            broken_child_tool_invocation_id,
            broken_child_run_id,
            ("read-call-broken", "read"),
            15,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        broken_child_tool_invocation.preceding_turn_id = Some(session.turns[4].id);
        broken_child_tool_invocation.arguments = json!({"path":"/tmp/broken.txt"});

        let mut cycle_tool_invocation = tool_invocation(
            cycle_tool_invocation_id,
            cycle_a_run_id,
            ("read-call-cycle", "read"),
            16,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        cycle_tool_invocation.preceding_turn_id = Some(session.turns[5].id);
        cycle_tool_invocation.arguments = json!({"path":"/tmp/cycle.txt"});

        session.tool_invocations = vec![
            root_task_invocation,
            valid_child_tool_invocation,
            broken_child_tool_invocation,
            cycle_tool_invocation,
        ];
        session.next_replay_sequence = 17;
        session.rebuild_run_indexes();

        let state = AppState::new(session);
        let projection = mapper.project_prompt_turn(&state, root_run_id).unwrap();

        assert_eq!(
            projection
                .events
                .iter()
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::UserMessage, "session_update"),
                (3, ProjectionEventPhase::AgentThought, "session_update"),
                (3, ProjectionEventPhase::AgentMessage, "session_update"),
                (5, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (5, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (10, ProjectionEventPhase::UserMessage, "session_update"),
                (11, ProjectionEventPhase::AgentThought, "session_update"),
                (11, ProjectionEventPhase::AgentMessage, "session_update"),
                (14, ProjectionEventPhase::ToolCallCreate, "session_update"),
                (14, ProjectionEventPhase::ToolCallPatch, "session_update"),
            ]
        );
        assert!(
            projection
                .events
                .iter()
                .all(|event| { !matches!(event.sequence, 12 | 13 | 15 | 16) })
        );
        assert!(
            mapper
                .project_prompt_turn(&state, valid_child_run_id)
                .is_none()
        );
        assert!(
            mapper
                .project_prompt_turn(&state, broken_child_run_id)
                .is_none()
        );
        assert!(mapper.project_prompt_turn(&state, cycle_a_run_id).is_none());
        assert!(mapper.project_prompt_turn(&state, cycle_b_run_id).is_none());
    }

    #[test]
    fn canonical_projection_preserves_tool_and_marker_metadata_extensions() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let task_invocation_id = Uuid::new_v4();
        let now = Utc::now();
        let mut session = Session::new("canonical metadata projection");
        let mut task_invocation = tool_invocation(
            task_invocation_id,
            root_run_id,
            ("task-call-1", "task"),
            1,
            ToolApprovalState::Approved,
            ToolExecutionState::Running,
            (None, None),
        );
        task_invocation.arguments =
            json!({"agent":"explore","prompt":"Inspect delegated child state"});
        task_invocation.delegation = Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect delegated child state".to_string()),
            status: TaskDelegationStatus::Running,
        });

        let root_run = RunRecord {
            id: root_run_id,
            status: RunStatus::Failed,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: 1,
            terminal_sequence: Some(3),
            terminal_stop_reason: Some(RunTerminalStopReason::Interrupted),
            created_at: now,
            updated_at: now,
        };

        session.runs = vec![
            root_run.clone(),
            RunRecord {
                id: child_run_id,
                status: RunStatus::InProgress,
                parent_run_id: Some(root_run_id),
                parent_tool_invocation_id: Some(task_invocation_id),
                created_sequence: 2,
                terminal_sequence: None,
                terminal_stop_reason: None,
                created_at: now,
                updated_at: now,
            },
        ];
        session.tool_invocations = vec![task_invocation.clone()];
        session.transcript_items = vec![
            TranscriptItemRecord::from_tool_invocation(&task_invocation),
            TranscriptItemRecord::delegated_child(&task_invocation, 2),
            TranscriptItemRecord::run_terminal(&root_run),
            TranscriptItemRecord::marker(
                root_run_id,
                4,
                "interrupted",
                Some("startup recovery failed closed".to_string()),
                None,
                None,
            ),
        ];
        session.next_replay_sequence = 5;
        session.rebuild_run_indexes();

        let state = AppState::new(session);
        let projection = mapper.project_prompt_turn(&state, root_run_id).unwrap();

        match &projection.events[0].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::ToolCall(tool_call)) => {
                assert_eq!(tool_call.tool_call_id, "task-call-1");
                assert_eq!(
                    tool_call
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get(ACP_META_TOOL_INVOCATION_KEY))
                        .and_then(|value| value.get("delegation"))
                        .and_then(|value| value.get("agent_name"))
                        .and_then(serde_json::Value::as_str),
                    Some("explore")
                );
            }
            other => panic!("expected tool call metadata, got {other:?}"),
        }

        assert_eq!(
            projection
                .events
                .iter()
                .skip(1)
                .map(event_signature)
                .collect::<Vec<_>>(),
            vec![
                (1, ProjectionEventPhase::ToolCallPatch, "session_update"),
                (2, ProjectionEventPhase::SessionInfoUpdate, "session_update"),
                (3, ProjectionEventPhase::SessionInfoUpdate, "session_update"),
                (4, ProjectionEventPhase::SessionInfoUpdate, "session_update"),
            ]
        );

        match &projection.events[2].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::SessionInfoUpdate(update)) => {
                assert_eq!(
                    update
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get(ACP_META_TRANSCRIPT_ITEM_KEY))
                        .and_then(|value| value.get("kind"))
                        .and_then(serde_json::Value::as_str),
                    Some("delegated_child")
                );
            }
            other => panic!("expected delegated child metadata update, got {other:?}"),
        }
    }

    #[test]
    fn project_live_prompt_turn_reprojects_open_stream_items_without_sequence_advance() {
        let mapper = SessionUpdateMapper::new();
        let root_run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        let now = Utc::now();
        let assistant_item_id = transcript_assistant_text_item_id(assistant_turn_id);
        let mut session = Session::new("live incremental open stream");
        session.runs = vec![RunRecord {
            id: root_run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: 1,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: now,
            updated_at: now,
        }];
        session.transcript_items = vec![
            TranscriptItemRecord::from_turn(&user_turn(root_run_id, 2, "resume me")),
            TranscriptItemRecord {
                item_id: assistant_item_id,
                sequence_number: 3,
                run_id: root_run_id,
                kind: fluent_code_app::session::model::TranscriptItemKind::Turn,
                stream_state: TranscriptStreamState::Open,
                turn_id: Some(assistant_turn_id),
                tool_invocation_id: None,
                parent_item_id: None,
                parent_tool_invocation_id: None,
                child_run_id: None,
                content: TranscriptItemContent::Turn(
                    fluent_code_app::session::model::TranscriptTurnContent {
                        role: Role::Assistant,
                        content: "partial answer".to_string(),
                        reasoning: String::new(),
                    },
                ),
            },
        ];
        session.next_replay_sequence = 4;
        session.normalize_persistence();

        let projection = mapper
            .project_live_prompt_turn(
                &AppState::new(session),
                root_run_id,
                Some(3),
                &HashSet::from([assistant_item_id]),
            )
            .expect("live prompt turn projection");

        assert_eq!(projection.max_sequence, Some(3));
        assert_eq!(projection.open_transcript_item_ids, vec![assistant_item_id]);
        assert_eq!(projection.events.len(), 1);
        assert_eq!(event_signature(&projection.events[0]).0, 3);
        assert_eq!(
            event_signature(&projection.events[0]).1,
            ProjectionEventPhase::AgentMessage
        );

        match &projection.events[0].event {
            PromptTurnEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
                assert_eq!(chunk.content, ContentBlock::text("partial answer"));
            }
            other => panic!("expected assistant message reprojection, got {other:?}"),
        }
    }

    fn event_signature(
        event: &OrderedPromptTurnEvent,
    ) -> (u64, ProjectionEventPhase, &'static str) {
        (
            event.sequence,
            event.phase,
            match event.event {
                PromptTurnEvent::SessionUpdate(_) => "session_update",
                PromptTurnEvent::PermissionRequest(_) => "permission_request",
            },
        )
    }

    fn session_with_records(
        run_id: Uuid,
        status: RunStatus,
        terminal_stop_reason: Option<fluent_code_app::session::model::RunTerminalStopReason>,
        turns: Vec<Turn>,
        tool_invocations: Vec<ToolInvocationRecord>,
        next_replay_sequence: u64,
    ) -> Session {
        let now = Utc::now();
        let terminal_sequence = status.is_terminal().then_some(next_replay_sequence - 1);
        let mut session = Session::new("mapping test");
        session.created_at = now;
        session.updated_at = now;
        session.next_replay_sequence = next_replay_sequence;
        session.runs = vec![RunRecord {
            id: run_id,
            status,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: 2,
            terminal_sequence,
            terminal_stop_reason,
            created_at: now,
            updated_at: now,
        }];
        session.turns = turns;
        session.tool_invocations = tool_invocations;
        session.rebuild_run_indexes();
        session
    }

    fn user_turn(run_id: Uuid, sequence_number: u64, content: &str) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: content.to_string(),
            reasoning: String::new(),
            sequence_number,
            timestamp: Utc::now(),
        }
    }

    fn assistant_turn(run_id: Uuid, sequence_number: u64, content: &str, reasoning: &str) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: content.to_string(),
            reasoning: reasoning.to_string(),
            sequence_number,
            timestamp: Utc::now(),
        }
    }

    fn tool_invocation(
        invocation_id: Uuid,
        run_id: Uuid,
        tool: (&str, &str),
        sequence_number: u64,
        approval_state: ToolApprovalState,
        execution_state: ToolExecutionState,
        output: (Option<&str>, Option<&str>),
    ) -> ToolInvocationRecord {
        ToolInvocationRecord {
            id: invocation_id,
            run_id,
            tool_call_id: tool.0.to_string(),
            tool_name: tool.1.to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path":"/tmp/notes.txt"}),
            preceding_turn_id: None,
            approval_state,
            execution_state,
            result: output.0.map(str::to_string),
            error: output.1.map(str::to_string),
            delegation: None,
            sequence_number,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        }
    }
}
