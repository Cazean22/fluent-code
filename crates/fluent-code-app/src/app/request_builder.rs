use std::collections::HashSet;

use fluent_code_provider::{ProviderMessage, ProviderRequest};
use tracing::warn;
use uuid::Uuid;

use crate::agent::AgentToolPermissions;
use crate::app::AppState;
use crate::session::model::{Role, Turn};

pub fn build_provider_request(state: &AppState, run_id: Uuid) -> ProviderRequest {
    let root_run_ids: HashSet<Uuid> = state
        .session
        .runs
        .iter()
        .filter(|run| run.parent_run_id.is_none())
        .map(|run| run.id)
        .collect();

    let is_root_run = root_run_ids.contains(&run_id);

    let messages = state
        .session
        .turns
        .iter()
        .filter(|turn| {
            if is_root_run {
                root_run_ids.contains(&turn.run_id)
            } else {
                turn.run_id == run_id
            }
        })
        .flat_map(|turn| {
            let mut messages = vec![turn_to_provider_message(turn)];
            append_tool_messages_after_turn(&mut messages, state, turn.run_id, turn.id);
            messages
        })
        .collect();

    ProviderRequest::new(messages, state.tool_registry.provider_tools())
}

pub fn child_provider_request(
    state: &AppState,
    prompt: String,
    system_prompt: String,
    agent_tool_permissions: &AgentToolPermissions,
) -> ProviderRequest {
    ProviderRequest::new(
        vec![ProviderMessage::UserText { text: prompt }],
        state
            .tool_registry
            .provider_tools_for_agent(agent_tool_permissions),
    )
    .with_system_prompt_override(Some(system_prompt))
}

fn append_tool_messages_after_turn(
    messages: &mut Vec<ProviderMessage>,
    state: &AppState,
    run_id: Uuid,
    turn_id: Uuid,
) {
    let mut invocations = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.run_id == run_id && invocation.preceding_turn_id == Some(turn_id)
        })
        .collect::<Vec<_>>();
    invocations.sort_by_key(|invocation| invocation.sequence_number);

    for invocation in invocations {
        if invocation.tool_call_id.trim().is_empty() {
            warn!(
                run_id = %run_id,
                invocation_id = %invocation.id,
                tool_name = %invocation.tool_name,
                "skipping tool invocation replay because tool_call_id is empty"
            );
            continue;
        }

        messages.push(ProviderMessage::AssistantToolCall {
            id: invocation.tool_call_id.clone(),
            name: invocation.tool_name.clone(),
            arguments: invocation.arguments.clone(),
        });

        match invocation.approval_state {
            crate::session::model::ToolApprovalState::Pending => {}
            crate::session::model::ToolApprovalState::Approved => {
                match invocation.execution_state {
                    crate::session::model::ToolExecutionState::Completed => {
                        messages.push(ProviderMessage::ToolResult {
                            tool_call_id: invocation.tool_call_id.clone(),
                            content: invocation.result.clone().unwrap_or_default(),
                        })
                    }
                    crate::session::model::ToolExecutionState::Failed => {
                        messages.push(ProviderMessage::ToolResult {
                            tool_call_id: invocation.tool_call_id.clone(),
                            content: invocation
                                .error
                                .clone()
                                .unwrap_or_else(|| "Tool execution failed".to_string()),
                        })
                    }
                    crate::session::model::ToolExecutionState::Running
                    | crate::session::model::ToolExecutionState::NotStarted
                    | crate::session::model::ToolExecutionState::Skipped => {}
                }
            }
            crate::session::model::ToolApprovalState::Denied => {
                messages.push(ProviderMessage::ToolResult {
                    tool_call_id: invocation.tool_call_id.clone(),
                    content: invocation
                        .error
                        .clone()
                        .unwrap_or_else(|| "Tool execution denied by user".to_string()),
                })
            }
        }
    }
}

fn turn_to_provider_message(turn: &Turn) -> ProviderMessage {
    match turn.role {
        Role::User => ProviderMessage::UserText {
            text: turn.content.clone(),
        },
        Role::Assistant => ProviderMessage::AssistantText {
            text: turn.content.clone(),
        },
        Role::Tool => ProviderMessage::ToolResult {
            tool_call_id: turn.id.to_string(),
            content: turn.content.clone(),
        },
        Role::System => ProviderMessage::AssistantText {
            text: turn.content.clone(),
        },
    }
}
