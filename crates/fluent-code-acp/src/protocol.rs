#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const JSONRPC_VERSION: &str = "2.0";
pub const ACP_PROTOCOL_VERSION: u16 = 1;
pub type Meta = serde_json::Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Initialize,
    Authenticate,
    SessionNew,
    SessionLoad,
    SessionResume,
    SessionClose,
    SessionList,
    SessionPrompt,
    SessionCancel,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Authenticate => "authenticate",
            Self::SessionNew => "session/new",
            Self::SessionLoad => "session/load",
            Self::SessionResume => "session/resume",
            Self::SessionClose => "session/close",
            Self::SessionList => "session/list",
            Self::SessionPrompt => "session/prompt",
            Self::SessionCancel => "session/cancel",
        }
    }
}

impl TryFrom<&str> for Method {
    type Error = ProtocolError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "initialize" => Ok(Self::Initialize),
            "authenticate" => Ok(Self::Authenticate),
            "session/new" => Ok(Self::SessionNew),
            "session/load" => Ok(Self::SessionLoad),
            "session/resume" => Ok(Self::SessionResume),
            "session/close" => Ok(Self::SessionClose),
            "session/list" => Ok(Self::SessionList),
            "session/prompt" => Ok(Self::SessionPrompt),
            "session/cancel" => Ok(Self::SessionCancel),
            _ => Err(ProtocolError::UnsupportedMethod(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProtocolState {
    #[default]
    Uninitialized,
    Initialized,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonRpcProtocol {
    state: ProtocolState,
}

impl JsonRpcProtocol {
    pub const fn new() -> Self {
        Self {
            state: ProtocolState::Uninitialized,
        }
    }

    pub const fn state(&self) -> ProtocolState {
        self.state
    }

    pub fn parse_request(&self, frame: &str) -> Result<ParsedRequest, ProtocolError> {
        let raw_request: RawJsonRpcRequest =
            serde_json::from_str(frame).map_err(ProtocolError::MalformedJson)?;

        if raw_request.jsonrpc != JSONRPC_VERSION {
            return Err(ProtocolError::UnsupportedJsonRpcVersion(
                raw_request.jsonrpc,
            ));
        }

        let method = Method::try_from(raw_request.method.as_str())?;
        self.validate_request_order(method, raw_request.method.as_str())?;

        Ok(ParsedRequest {
            jsonrpc: raw_request.jsonrpc,
            id: raw_request.id,
            method,
            params: raw_request.params,
        })
    }

    pub fn mark_initialized(&mut self) -> Result<(), ProtocolError> {
        match self.state {
            ProtocolState::Uninitialized => {
                self.state = ProtocolState::Initialized;
                Ok(())
            }
            ProtocolState::Initialized => Err(ProtocolError::InitializeOutOfOrder),
        }
    }

    fn validate_request_order(
        &self,
        method: Method,
        method_name: &str,
    ) -> Result<(), ProtocolError> {
        match (self.state, method) {
            (ProtocolState::Uninitialized, Method::Initialize) => Ok(()),
            (ProtocolState::Uninitialized, _) => Err(ProtocolError::InitializeRequired {
                method: method_name.to_string(),
            }),
            (ProtocolState::Initialized, Method::Initialize) => {
                Err(ProtocolError::InitializeOutOfOrder)
            }
            (ProtocolState::Initialized, _) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: Method,
    pub params: Value,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("malformed JSON-RPC frame: {0}")]
    MalformedJson(serde_json::Error),
    #[error("unsupported JSON-RPC method `{0}`")]
    UnsupportedMethod(String),
    #[error("unsupported JSON-RPC version `{0}`")]
    UnsupportedJsonRpcVersion(String),
    #[error("initialize must be the first request, got `{method}`")]
    InitializeRequired { method: String },
    #[error("initialize request is out of order")]
    InitializeOutOfOrder,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawJsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcRequest<T> {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: T,
}

impl<T> JsonRpcRequest<T> {
    pub fn new(id: u64, method: Method, params: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.as_str().to_string(),
            params,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: String,
    pub id: u64,
    pub result: T,
}

impl<T> JsonRpcResponse<T> {
    pub fn new(id: u64, result: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcNotification<T> {
    pub jsonrpc: String,
    pub method: String,
    pub params: T,
}

impl<T> JsonRpcNotification<T> {
    pub fn new(method: impl Into<String>, params: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub error: JsonRpcError,
}

impl JsonRpcErrorResponse {
    pub fn new(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

impl ServerInfo {
    pub fn with_title(
        name: impl Into<String>,
        title: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            title: Some(title.into()),
            version: version.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AgentCapabilities,
    pub agent_info: ServerInfo,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_methods: Vec<AuthMethod>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<ClientCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ServerInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<ClientFsCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFsCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<McpCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded_context: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListCapabilities {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateRequest {
    pub method_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticateResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseSessionRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseSessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfoEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_prompt_state: Option<PromptTurnState>,
    #[serde(default)]
    pub replay_fidelity: ReplayFidelity,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayFidelity {
    #[default]
    Exact,
    Approximate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptResponse {
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTurnState {
    Running,
    AwaitingToolApproval,
    RunningTool,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<SessionConfigOptionCategory>,
    #[serde(flatten)]
    pub kind: SessionConfigKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigOptionCategory {
    Mode,
    Model,
    ThoughtLevel,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigKind {
    Select(SessionConfigSelect),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelect {
    pub current_value: String,
    pub options: Vec<SessionConfigSelectOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextContent),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk(UserMessageChunk),
    AgentMessageChunk(AgentMessageChunk),
    AgentThoughtChunk(AgentThoughtChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    SessionInfoUpdate(SessionInfoUpdate),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessageChunk {
    pub content: ContentBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageChunk {
    pub content: ContentBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentThoughtChunk {
    pub content: ContentBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub title: String,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl ToolCallUpdate {
    pub fn new(tool_call_id: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            title: None,
            kind: None,
            status: None,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
            meta: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.kind.is_none()
            && self.status.is_none()
            && self.content.is_none()
            && self.locations.is_none()
            && self.raw_input.is_none()
            && self.raw_output.is_none()
            && self.meta.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content { content: ContentBlock },
}

impl ToolCallContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Content {
            content: ContentBlock::text(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        AgentCapabilities, AuthenticateRequest, ContentBlock, InitializeResponse,
        JsonRpcNotification, JsonRpcProtocol, JsonRpcRequest, JsonRpcResponse, LoadSessionResponse,
        Meta, Method, NewSessionResponse, PromptTurnState, ProtocolError, ProtocolState,
        ReplayFidelity, ServerInfo, SessionCancelRequest, SessionConfigKind, SessionConfigOption,
        SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption,
        SessionNotification, SessionPromptRequest, SessionPromptResponse, SessionUpdate,
        StopReason, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    };

    #[test]
    fn request_serialization_uses_jsonrpc_shape() {
        let request = JsonRpcRequest::new(7, Method::SessionNew, json!({ "cwd": "/tmp" }));

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "session/new",
                "params": {
                    "cwd": "/tmp"
                }
            })
        );
    }

    #[test]
    fn authenticate_request_serialization_uses_jsonrpc_shape() {
        let request = JsonRpcRequest::new(
            8,
            Method::Authenticate,
            AuthenticateRequest {
                method_id: "api_key".to_string(),
            },
        );

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "authenticate",
                "params": {
                    "methodId": "api_key"
                }
            })
        );
    }

    #[test]
    fn session_prompt_request_serialization_uses_content_blocks() {
        let request = JsonRpcRequest::new(
            9,
            Method::SessionPrompt,
            SessionPromptRequest {
                session_id: "session-1".to_string(),
                prompt: vec![ContentBlock::text("Inspect this file")],
            },
        );

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "session/prompt",
                "params": {
                    "sessionId": "session-1",
                    "prompt": [
                        {
                            "type": "text",
                            "text": "Inspect this file"
                        }
                    ]
                }
            })
        );
    }

    #[test]
    fn session_cancel_request_serialization_uses_session_id() {
        let request = JsonRpcRequest::new(
            10,
            Method::SessionCancel,
            SessionCancelRequest {
                session_id: "session-1".to_string(),
            },
        );

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "session/cancel",
                "params": {
                    "sessionId": "session-1"
                }
            })
        );
    }

    #[test]
    fn response_serialization_uses_jsonrpc_shape() {
        let response = JsonRpcResponse::new(11, json!({ "sessionId": "session-1" }));

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "result": {
                    "sessionId": "session-1"
                }
            })
        );
    }

    #[test]
    fn session_prompt_response_serializes_stop_reason() {
        let response = JsonRpcResponse::new(
            12,
            SessionPromptResponse {
                stop_reason: StopReason::EndTurn,
            },
        );

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "result": {
                    "stopReason": "end_turn"
                }
            })
        );
    }

    #[test]
    fn malformed_jsonrpc_frame_is_rejected() {
        let protocol = JsonRpcProtocol::new();

        let error = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}"#)
            .unwrap_err();

        assert!(matches!(error, ProtocolError::MalformedJson(_)));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let protocol = JsonRpcProtocol::new();

        let error = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"session/unknown","params":{}}"#)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "unsupported JSON-RPC method `session/unknown`"
        );
    }

    #[test]
    fn pre_initialize_request_is_rejected() {
        let protocol = JsonRpcProtocol::new();

        let error = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "initialize must be the first request, got `session/new`"
        );
    }

    #[test]
    fn initialize_marks_protocol_ready_for_follow_up_requests() {
        let mut protocol = JsonRpcProtocol::new();
        let initialize_request = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();

        assert_eq!(initialize_request.method, Method::Initialize);

        protocol.mark_initialized().unwrap();

        let next_request = protocol
            .parse_request(
                r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            )
            .unwrap();

        assert_eq!(protocol.state(), ProtocolState::Initialized);
        assert_eq!(next_request.method, Method::SessionNew);
    }

    #[test]
    fn authenticate_method_is_recognized_after_initialize() {
        let mut protocol = JsonRpcProtocol::new();
        protocol.mark_initialized().unwrap();

        let request = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"api_key"}}"#)
            .unwrap();

        assert_eq!(request.method, Method::Authenticate);
    }

    #[test]
    fn initialize_request_is_rejected_after_protocol_initializes() {
        let mut protocol = JsonRpcProtocol::new();
        protocol.mark_initialized().unwrap();

        let error = protocol
            .parse_request(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#)
            .unwrap_err();

        assert!(matches!(error, ProtocolError::InitializeOutOfOrder));
    }

    #[test]
    fn initialize_response_omits_unsupported_capabilities() {
        let response = InitializeResponse {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: Some(true),
                ..AgentCapabilities::default()
            },
            agent_info: ServerInfo::with_title("fluent-code", "Fluent Code", "1.0.0"),
            auth_methods: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true
                },
                "agentInfo": {
                    "name": "fluent-code",
                    "title": "Fluent Code",
                    "version": "1.0.0"
                }
            })
        );
    }

    #[test]
    fn session_update_notification_serializes_tagged_payloads() {
        let notification = SessionNotification {
            session_id: "session-1".to_string(),
            update: SessionUpdate::AgentThoughtChunk(super::AgentThoughtChunk {
                content: ContentBlock::text("thinking"),
            }),
        };

        assert_eq!(
            serde_json::to_value(notification).unwrap(),
            json!({
                "sessionId": "session-1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {
                        "type": "text",
                        "text": "thinking"
                    }
                }
            })
        );
    }

    #[test]
    fn tool_call_update_serializes_only_patched_fields() {
        let mut update = ToolCallUpdate::new("tool-call-1");
        update.status = Some(ToolCallStatus::Completed);
        update.content = Some(vec![ToolCallContent::text("done")]);

        assert_eq!(
            serde_json::to_value(update).unwrap(),
            json!({
                "toolCallId": "tool-call-1",
                "status": "completed",
                "content": [
                    {
                        "type": "content",
                        "content": {
                            "type": "text",
                            "text": "done"
                        }
                    }
                ]
            })
        );
    }

    #[test]
    fn session_setup_response_serializes_config_options_without_modes() {
        let response = NewSessionResponse {
            session_id: "session-1".to_string(),
            config_options: Some(vec![SessionConfigOption {
                id: "reasoning_effort".to_string(),
                name: "Reasoning Effort".to_string(),
                description: None,
                category: Some(SessionConfigOptionCategory::ThoughtLevel),
                kind: SessionConfigKind::Select(SessionConfigSelect {
                    current_value: "medium".to_string(),
                    options: vec![SessionConfigSelectOption {
                        value: "medium".to_string(),
                        name: "Medium".to_string(),
                        description: None,
                    }],
                }),
            }]),
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "sessionId": "session-1",
                "configOptions": [
                    {
                        "id": "reasoning_effort",
                        "name": "Reasoning Effort",
                        "category": "thought_level",
                        "type": "select",
                        "currentValue": "medium",
                        "options": [
                            {
                                "value": "medium",
                                "name": "Medium"
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn load_session_response_serializes_latest_prompt_state() {
        let response = LoadSessionResponse {
            config_options: None,
            latest_prompt_state: Some(PromptTurnState::Interrupted),
            replay_fidelity: ReplayFidelity::Approximate,
            meta: None,
        };

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "latestPromptState": "interrupted",
                "replayFidelity": "approximate"
            })
        );
    }

    #[test]
    fn notification_serialization_uses_jsonrpc_shape() {
        let notification = JsonRpcNotification::new(
            "session/update",
            SessionNotification {
                session_id: "session-1".to_string(),
                update: SessionUpdate::AgentMessageChunk(super::AgentMessageChunk {
                    content: ContentBlock::text("hello"),
                }),
            },
        );

        assert_eq!(
            serde_json::to_value(notification).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "text",
                            "text": "hello"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn tool_call_update_meta_only_is_not_empty() {
        let mut update = ToolCallUpdate::new("tool-call-1");
        let mut meta = Meta::new();
        meta.insert(
            "fluentCodeToolInvocation".to_string(),
            json!({"toolCallId":"tool-call-1"}),
        );
        update.meta = Some(meta);

        assert!(!update.is_empty());
    }
}
