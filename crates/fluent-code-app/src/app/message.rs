use fluent_code_provider::{ProviderRequest, ProviderToolCall};
use uuid::Uuid;

use crate::app::permissions::PermissionReply;
use crate::session::model::ToolInvocationId;

#[derive(Debug, Clone)]
pub enum Msg {
    InputChanged(String),
    SubmitPrompt,
    NewSession,
    ReplyToPendingTool(PermissionReply),
    CancelActiveRun,
    AssistantChunk {
        run_id: Uuid,
        delta: String,
    },
    AssistantReasoningChunk {
        run_id: Uuid,
        delta: String,
    },
    AssistantToolCall {
        run_id: Uuid,
        tool_call: ProviderToolCall,
    },
    AssistantDone {
        run_id: Uuid,
    },
    AssistantFailed {
        run_id: Uuid,
        error: String,
    },
    ToolExecutionFinished {
        run_id: Uuid,
        invocation_id: ToolInvocationId,
        result: std::result::Result<String, String>,
    },
    Quit,
}

#[derive(Debug, Clone)]
pub enum Effect {
    PersistSession,
    PersistSessionIfDue,
    StartAssistant {
        run_id: Uuid,
        request: ProviderRequest,
    },
    ExecuteTool {
        run_id: Uuid,
        invocation_id: ToolInvocationId,
        tool_call: ProviderToolCall,
    },
    CancelAssistant {
        run_id: Uuid,
    },
}
