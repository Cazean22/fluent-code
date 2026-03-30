use std::path::Path;

use fluent_code_app::app::AppState;
use fluent_code_app::config::{AcpConfig, AcpSessionDefaultsConfig};
use fluent_code_app::session::model::{
    ReplaySequence, Role, RunId, RunRecord, RunTerminalStopReason, Session, ToolApprovalState,
    ToolExecutionState, ToolInvocationRecord, ToolSource,
};
use serde_json::json;

use crate::protocol::{
    ACP_PROTOCOL_VERSION, AgentCapabilities, AgentMessageChunk, AgentThoughtChunk, AuthMethod,
    ContentBlock, InitializeResponse, PermissionOption, PermissionOptionKind,
    RequestPermissionRequest, ServerInfo, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolKind, UserMessageChunk,
};

const TOOL_CALL_CREATE_PHASE: ProjectionEventPhase = ProjectionEventPhase::ToolCallCreate;
const TOOL_CALL_PATCH_PHASE: ProjectionEventPhase = ProjectionEventPhase::ToolCallPatch;
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
        let mut events = Vec::new();

        for turn in session
            .turns
            .iter()
            .filter(|turn| turn.run_id == run_id)
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
            .filter(|invocation| invocation.run_id == run_id)
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

        Some(PromptTurnProjection {
            run_id,
            events,
            terminal_stop: self.terminal_stop(run),
        })
    }

    pub fn tool_call_snapshot(&self, invocation: &ToolInvocationRecord) -> ToolCall {
        ToolCall {
            title: tool_title(invocation),
            tool_call_id: invocation.tool_call_id.clone(),
            kind: Some(tool_kind(invocation)),
            status: Some(ToolCallStatus::Pending),
            content: None,
            locations: tool_input_locations(invocation),
            raw_input: Some(invocation.arguments.clone()),
            raw_output: None,
        }
    }

    pub fn tool_call_patch(&self, invocation: &ToolInvocationRecord) -> Option<ToolCallUpdate> {
        let final_status = final_tool_status(invocation);
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
            update.locations = tool_output_locations(invocation);
        } else if let Some(error) = invocation.error.as_ref() {
            update.content = Some(vec![ToolCallContent::text(error.clone())]);
            update.raw_output = Some(json!({ "error": error }));
        }

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
        tool_call.title = Some(tool_title(invocation));
        tool_call.kind = Some(tool_kind(invocation));
        tool_call.status = Some(ToolCallStatus::Pending);
        tool_call.locations = tool_input_locations(invocation);
        tool_call.raw_input = Some(invocation.arguments.clone());

        Some(RequestPermissionRequest {
            session_id: state.session.id.to_string(),
            tool_call,
            options: permission_options(rememberable),
        })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTurnProjection {
    pub run_id: RunId,
    pub events: Vec<OrderedPromptTurnEvent>,
    pub terminal_stop: Option<TerminalStopProjection>,
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

fn final_tool_status(invocation: &ToolInvocationRecord) -> ToolCallStatus {
    match invocation.execution_state {
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

fn tool_input_locations(invocation: &ToolInvocationRecord) -> Option<Vec<ToolCallLocation>> {
    let line = invocation
        .arguments
        .get("offset")
        .and_then(|value| value.as_u64())
        .filter(|line| *line > 0);

    match invocation.tool_name.as_str() {
        "read" => invocation
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .and_then(|path| absolute_tool_location(path, line))
            .map(|location| vec![location]),
        "glob" | "grep" => invocation
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .and_then(|path| absolute_tool_location(path, None))
            .map(|location| vec![location]),
        _ => None,
    }
}

fn tool_output_locations(invocation: &ToolInvocationRecord) -> Option<Vec<ToolCallLocation>> {
    let result = invocation.result.as_deref()?;
    let mut locations = match invocation.tool_name.as_str() {
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

fn tool_title(invocation: &ToolInvocationRecord) -> String {
    let base_title = invocation.tool_name.replace('_', " ");
    match &invocation.tool_source {
        ToolSource::BuiltIn => base_title,
        ToolSource::Plugin { plugin_name, .. } => format!("{base_title} ({plugin_name})"),
    }
}

fn tool_kind(invocation: &ToolInvocationRecord) -> ToolKind {
    let normalized_name = invocation.tool_name.to_ascii_lowercase();

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
    use chrono::Utc;
    use fluent_code_app::app::AppState;
    use fluent_code_app::session::model::{
        Role, RunRecord, RunStatus, Session, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, Turn,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        OrderedPromptTurnEvent, ProjectionEventPhase, PromptTurnEvent, SessionUpdateMapper,
        TerminalStopProjection,
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
        assert!(json["agentCapabilities"].get("mcpCapabilities").is_none());
        assert!(
            json["agentCapabilities"]
                .get("promptCapabilities")
                .is_none()
        );
        assert!(
            json["agentCapabilities"]
                .get("sessionCapabilities")
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

        let projections = mapper.project_prompt_turns(&AppState::new(session));

        assert_eq!(
            projections
                .iter()
                .map(|projection| projection.run_id)
                .collect::<Vec<_>>(),
            vec![first_run_id, second_run_id]
        );
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

        Session {
            id: Uuid::new_v4(),
            title: "mapping test".to_string(),
            created_at: now,
            updated_at: now,
            next_replay_sequence,
            permissions: Default::default(),
            runs: vec![RunRecord {
                id: run_id,
                status,
                parent_run_id: None,
                parent_tool_invocation_id: None,
                created_sequence: 2,
                terminal_sequence,
                terminal_stop_reason,
                created_at: now,
                updated_at: now,
            }],
            turns,
            tool_invocations,
            foreground_owner: None,
        }
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
