use chrono::Utc;
use fluent_code_provider::{ProviderMessage, ProviderRequest, ProviderToolCall};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::app::{AppState, AppStatus, Effect, Msg};
use crate::session::model::{
    Role, RunStatus, ToolApprovalState, ToolExecutionState, ToolInvocationRecord, Turn,
};

const READ_TOOL_NAME: &str = "read";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBatchProgress {
    AwaitingApproval,
    Running,
    ReadyToResume,
}

pub fn update(state: &mut AppState, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::InputChanged(input) => {
            state.draft_input = input;
            Vec::new()
        }
        Msg::SubmitPrompt => {
            if !matches!(state.status, AppStatus::Idle | AppStatus::Error(_)) {
                debug!(
                    session_id = %state.session.id,
                    status = ?state.status,
                    active_run_id = ?state.active_run_id,
                    "ignored prompt submission because app is busy"
                );
                return Vec::new();
            }

            let prompt = state.draft_input.trim().to_owned();
            if prompt.is_empty() {
                debug!(
                    session_id = %state.session.id,
                    draft_input_bytes = state.draft_input.len(),
                    "ignored empty prompt submission"
                );
                return Vec::new();
            }

            let run_id = Uuid::new_v4();
            let turn = Turn {
                id: Uuid::new_v4(),
                run_id,
                role: Role::User,
                content: prompt,
                timestamp: Utc::now(),
            };
            state.session.turns.push(turn);
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::InProgress);
            state.draft_input.clear();
            state.status = AppStatus::Generating;
            state.active_run_id = Some(run_id);

            let request = build_provider_request(state, run_id);

            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                request_message_count = request.messages.len(),
                request_tool_count = request.tools.len(),
                "queued assistant run from submitted prompt"
            );

            vec![
                Effect::PersistSession,
                Effect::StartAssistant { run_id, request },
            ]
        }
        Msg::NewSession => Vec::new(),
        Msg::ApprovePendingTool => {
            let Some(run_id) = state.active_run_id else {
                debug!(
                    session_id = %state.session.id,
                    "ignored tool approval because no run is active"
                );
                return Vec::new();
            };

            let Some(preceding_turn_id) = state
                .session
                .pending_tool_invocation()
                .map(|invocation| invocation.preceding_turn_id)
            else {
                return Vec::new();
            };

            let approved_at = Utc::now();
            let mut approved_tool_calls = Vec::new();

            for invocation in state
                .session
                .tool_invocations
                .iter_mut()
                .filter(|invocation| {
                    invocation.run_id == run_id
                        && invocation.preceding_turn_id == preceding_turn_id
                        && invocation.approval_state == ToolApprovalState::Pending
                })
            {
                invocation.approval_state = ToolApprovalState::Approved;
                invocation.execution_state = ToolExecutionState::Running;
                invocation.approved_at = Some(approved_at);
                invocation.error = None;

                approved_tool_calls.push((
                    invocation.id,
                    ProviderToolCall {
                        id: invocation.tool_call_id.clone(),
                        name: invocation.tool_name.clone(),
                        arguments: invocation.arguments.clone(),
                    },
                ));
            }

            if approved_tool_calls.is_empty() {
                return Vec::new();
            }

            state.status = AppStatus::RunningTool;
            state.pending_resume_request = None;
            state.session.updated_at = Utc::now();

            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                approved_count = approved_tool_calls.len(),
                "approved pending tool invocation batch"
            );

            let mut effects = vec![Effect::PersistSession];
            effects.extend(
                approved_tool_calls
                    .into_iter()
                    .map(|(invocation_id, tool_call)| Effect::ExecuteTool {
                        run_id,
                        invocation_id,
                        tool_call,
                    }),
            );
            effects
        }
        Msg::DenyPendingTool => {
            let Some(run_id) = state.active_run_id else {
                debug!(
                    session_id = %state.session.id,
                    "ignored tool denial because no run is active"
                );
                return Vec::new();
            };
            let session_id = state.session.id;

            let Some((invocation_id, preceding_turn_id)) = state
                .session
                .pending_tool_invocation()
                .map(|invocation| (invocation.id, invocation.preceding_turn_id))
            else {
                return Vec::new();
            };

            if let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) {
                invocation.approval_state = ToolApprovalState::Denied;
                invocation.execution_state = ToolExecutionState::Skipped;
                invocation.error = Some("Tool execution denied by user".to_string());
                invocation.completed_at = Some(Utc::now());
            }

            state.session.updated_at = Utc::now();

            match tool_batch_progress(state, run_id, preceding_turn_id) {
                ToolBatchProgress::AwaitingApproval => {
                    state.status = AppStatus::AwaitingToolApproval;
                    state.pending_resume_request = None;
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "denied pending tool invocation and waiting for remaining tool decisions"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::Running => {
                    state.status = AppStatus::RunningTool;
                    state.pending_resume_request = None;
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "denied pending tool invocation while another tool is still running"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::ReadyToResume => {
                    state.status = AppStatus::Generating;
                    state.pending_resume_request = Some(build_provider_request(state, run_id));
                }
            }

            let request = state
                .pending_resume_request
                .clone()
                .expect("resume request just prepared");

            if let Some((logged_invocation_id, logged_tool_name)) = state
                .session
                .tool_invocations
                .iter()
                .find(|invocation| invocation.id == invocation_id)
                .map(|invocation| (invocation.id, invocation.tool_name.clone()))
            {
                info!(
                    session_id = %session_id,
                    run_id = %run_id,
                    invocation_id = %logged_invocation_id,
                    tool_name = %logged_tool_name,
                    "denied pending tool invocation and resumed assistant"
                );
            }

            vec![
                Effect::PersistSession,
                Effect::StartAssistant { run_id, request },
            ]
        }
        Msg::CancelActiveRun => {
            let Some(run_id) = state.active_run_id.take() else {
                debug!(
                    session_id = %state.session.id,
                    "ignored cancel because no run is active"
                );
                return Vec::new();
            };

            state.status = AppStatus::Idle;
            state.pending_resume_request = None;
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::Cancelled);

            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                "cancelled active assistant run"
            );

            vec![Effect::CancelAssistant { run_id }, Effect::PersistSession]
        }
        Msg::AssistantChunk { run_id, delta } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    active_run_id = ?state.active_run_id,
                    "ignored stale assistant chunk"
                );
                return Vec::new();
            }

            state.status = AppStatus::Generating;
            state.pending_resume_request = None;
            let should_checkpoint = state.should_checkpoint_now();
            let chunk_bytes = delta.len();

            if let Some(last_turn) = state.session.turns.last_mut()
                && matches!(last_turn.role, Role::Assistant)
                && last_turn.run_id == run_id
            {
                last_turn.content.push_str(&delta);
                state.session.updated_at = Utc::now();
                debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, "appended assistant chunk to existing turn");
                return if should_checkpoint {
                    vec![Effect::PersistSessionIfDue]
                } else {
                    Vec::new()
                };
            }

            state.session.turns.push(Turn {
                id: Uuid::new_v4(),
                run_id,
                role: Role::Assistant,
                content: delta,
                timestamp: Utc::now(),
            });
            state.session.updated_at = Utc::now();
            debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, "started assistant turn from first chunk");

            if should_checkpoint {
                vec![Effect::PersistSessionIfDue]
            } else {
                Vec::new()
            }
        }
        Msg::AssistantToolCall { run_id, tool_call } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    active_run_id = ?state.active_run_id,
                    tool_name = %tool_call.name,
                    "ignored stale tool call"
                );
                return Vec::new();
            }

            let tool_name = tool_call.name.clone();
            let tool_source = state.tool_registry.tool_source(&tool_name);

            let invocation = ToolInvocationRecord {
                id: Uuid::new_v4(),
                run_id,
                tool_call_id: tool_call.id.clone(),
                tool_name,
                tool_source,
                arguments: tool_call.arguments,
                preceding_turn_id: state.session.turns.last().map(|turn| turn.id),
                approval_state: ToolApprovalState::Pending,
                execution_state: ToolExecutionState::NotStarted,
                result: None,
                error: None,
                requested_at: Utc::now(),
                approved_at: None,
                completed_at: None,
            };
            let auto_approve = state
                .tool_registry
                .plugin_registration(&invocation.tool_name)
                .map(|registration| !registration.requires_approval)
                .unwrap_or(false);

            state.session.tool_invocations.push(invocation);
            state.pending_resume_request = None;
            state.session.updated_at = Utc::now();

            let invocation = state
                .session
                .tool_invocations
                .last_mut()
                .expect("tool invocation just pushed");

            if auto_approve {
                invocation.approval_state = ToolApprovalState::Approved;
                invocation.execution_state = ToolExecutionState::Running;
                invocation.approved_at = Some(Utc::now());
                state.status = AppStatus::RunningTool;
                info!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    invocation_id = %invocation.id,
                    tool_name = %invocation.tool_name,
                    tool_call_id = %invocation.tool_call_id,
                    "assistant tool auto-approved by plugin policy"
                );

                return vec![
                    Effect::PersistSession,
                    Effect::ExecuteTool {
                        run_id,
                        invocation_id: invocation.id,
                        tool_call: ProviderToolCall {
                            id: invocation.tool_call_id.clone(),
                            name: invocation.tool_name.clone(),
                            arguments: invocation.arguments.clone(),
                        },
                    },
                ];
            }

            state.status = AppStatus::AwaitingToolApproval;
            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                invocation_id = %invocation.id,
                tool_name = %invocation.tool_name,
                tool_call_id = %invocation.tool_call_id,
                "assistant entered tool approval state"
            );

            vec![Effect::PersistSession]
        }
        Msg::AssistantDone { run_id } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    active_run_id = ?state.active_run_id,
                    "ignored stale assistant completion"
                );
                return Vec::new();
            }

            state.active_run_id = None;
            state.pending_resume_request = None;
            state.status = AppStatus::Idle;
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::Completed);

            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                "assistant run completed"
            );

            vec![Effect::PersistSession]
        }
        Msg::AssistantFailed { run_id, error } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    active_run_id = ?state.active_run_id,
                    error = %error,
                    "ignored stale assistant failure"
                );
                return Vec::new();
            }

            state.active_run_id = None;
            state.pending_resume_request = None;
            state.status = AppStatus::Error(error);
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::Failed);

            if let AppStatus::Error(message) = &state.status {
                warn!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    error = %message,
                    "assistant run failed"
                );
            }

            vec![Effect::PersistSession]
        }
        Msg::ToolExecutionFinished {
            run_id,
            invocation_id,
            result,
        } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    invocation_id = %invocation_id,
                    active_run_id = ?state.active_run_id,
                    "ignored stale tool result"
                );
                return Vec::new();
            }
            let session_id = state.session.id;
            let Some(preceding_turn_id) = state
                .session
                .tool_invocations
                .iter()
                .find(|invocation| invocation.id == invocation_id)
                .map(|invocation| invocation.preceding_turn_id)
            else {
                return Vec::new();
            };

            let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) else {
                return Vec::new();
            };

            invocation.completed_at = Some(Utc::now());

            match result {
                Ok(output) => {
                    invocation.execution_state = ToolExecutionState::Completed;
                    invocation.result = Some(output);
                    invocation.error = None;
                    let tool_name = invocation.tool_name.clone();
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        tool_name = %tool_name,
                        "tool execution finished and assistant will resume"
                    );
                }
                Err(error) => {
                    invocation.execution_state = ToolExecutionState::Failed;
                    invocation.error = Some(error.clone());
                    let tool_name = invocation.tool_name.clone();
                    if should_resume_after_tool_failure(&tool_name, &error) {
                        info!(
                            session_id = %session_id,
                            run_id = %run_id,
                            invocation_id = %invocation_id,
                            tool_name = %tool_name,
                            error = %error,
                            "tool execution failed but assistant will resume"
                        );
                    } else {
                        state.status = AppStatus::Error(error);
                        state.session.updated_at = Utc::now();
                        state.session.upsert_run(run_id, RunStatus::Failed);
                        state.active_run_id = None;
                        state.pending_resume_request = None;
                        if let AppStatus::Error(message) = &state.status {
                            warn!(
                                session_id = %session_id,
                                run_id = %run_id,
                                invocation_id = %invocation_id,
                                tool_name = %tool_name,
                                error = %message,
                                "tool execution failed and ended the run"
                            );
                        }
                        return vec![Effect::PersistSession];
                    }
                }
            }

            state.session.updated_at = Utc::now();

            match tool_batch_progress(state, run_id, preceding_turn_id) {
                ToolBatchProgress::AwaitingApproval => {
                    state.status = AppStatus::AwaitingToolApproval;
                    state.pending_resume_request = None;
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "tool execution finished and waiting for remaining tool approvals"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::Running => {
                    state.status = AppStatus::RunningTool;
                    state.pending_resume_request = None;
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "tool execution finished and another tool is still running"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::ReadyToResume => {
                    state.status = AppStatus::Generating;
                    state.pending_resume_request = Some(build_provider_request(state, run_id));
                }
            }

            let request = state
                .pending_resume_request
                .clone()
                .expect("resume request just prepared");

            vec![
                Effect::PersistSession,
                Effect::StartAssistant { run_id, request },
            ]
        }
        Msg::Quit => {
            state.should_quit = true;
            Vec::new()
        }
    }
}

fn build_provider_request(state: &AppState, run_id: Uuid) -> ProviderRequest {
    let messages = state
        .session
        .turns
        .iter()
        .filter(|turn| turn.run_id == run_id)
        .flat_map(|turn| {
            let mut messages = vec![turn_to_provider_message(turn)];
            append_tool_messages_after_turn(&mut messages, state, run_id, turn.id);
            messages
        })
        .collect();

    ProviderRequest::new(messages, state.tool_registry.provider_tools())
}

fn should_resume_after_tool_failure(tool_name: &str, error: &str) -> bool {
    tool_name == READ_TOOL_NAME && error.contains("is not accessible")
}

fn tool_batch_progress(
    state: &AppState,
    run_id: Uuid,
    preceding_turn_id: Option<Uuid>,
) -> ToolBatchProgress {
    let mut has_pending = false;
    let mut has_running = false;

    for invocation in &state.session.tool_invocations {
        if invocation.run_id != run_id || invocation.preceding_turn_id != preceding_turn_id {
            continue;
        }

        match invocation.approval_state {
            ToolApprovalState::Pending => {
                has_pending = true;
            }
            ToolApprovalState::Approved => {
                if matches!(
                    invocation.execution_state,
                    ToolExecutionState::NotStarted | ToolExecutionState::Running
                ) {
                    has_running = true;
                }
            }
            ToolApprovalState::Denied => {}
        }
    }

    if has_pending {
        ToolBatchProgress::AwaitingApproval
    } else if has_running {
        ToolBatchProgress::Running
    } else {
        ToolBatchProgress::ReadyToResume
    }
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
    invocations.sort_by_key(|invocation| invocation.requested_at);

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
            ToolApprovalState::Pending => {}
            ToolApprovalState::Approved => match invocation.execution_state {
                ToolExecutionState::Completed => messages.push(ProviderMessage::ToolResult {
                    tool_call_id: invocation.tool_call_id.clone(),
                    content: invocation.result.clone().unwrap_or_default(),
                }),
                ToolExecutionState::Failed => messages.push(ProviderMessage::ToolResult {
                    tool_call_id: invocation.tool_call_id.clone(),
                    content: invocation
                        .error
                        .clone()
                        .unwrap_or_else(|| "Tool execution failed".to_string()),
                }),
                ToolExecutionState::Running
                | ToolExecutionState::NotStarted
                | ToolExecutionState::Skipped => {}
            },
            ToolApprovalState::Denied => messages.push(ProviderMessage::ToolResult {
                tool_call_id: invocation.tool_call_id.clone(),
                content: invocation
                    .error
                    .clone()
                    .unwrap_or_else(|| "Tool execution denied by user".to_string()),
            }),
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

#[cfg(test)]
mod tests {
    use super::update;
    use crate::app::{AppState, AppStatus, Effect, Msg};
    use crate::session::model::{Role, RunStatus, Session, ToolApprovalState, ToolExecutionState};

    #[test]
    fn new_session_message_is_ignored_by_reducer() {
        let session = Session::new("existing session");
        let session_id = session.id;
        let mut state = AppState::new(session);
        state.draft_input = "keep current state until tui swaps".to_string();

        let effects = update(&mut state, Msg::NewSession);

        assert!(effects.is_empty());
        assert_eq!(state.session.id, session_id);
        assert_eq!(state.draft_input, "keep current state until tui swaps");
        assert!(matches!(state.status, AppStatus::Idle));
    }

    #[test]
    fn submit_prompt_creates_structured_request() {
        let session = Session::new("request test");
        let mut state = AppState::new(session);
        state.draft_input = "hello".to_string();

        let effects = update(&mut state, Msg::SubmitPrompt);

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(state.session.turns.len(), 1);
        assert!(matches!(
            state.session.latest_run_status(),
            Some(RunStatus::InProgress)
        ));
        assert!(matches!(
            effects.get(1),
            Some(Effect::StartAssistant { request, .. })
                if matches!(request.messages.first(), Some(fluent_code_provider::ProviderMessage::UserText { text }) if text == "hello")
        ));
        assert!(request_contains_tool_name(
            match effects.get(1) {
                Some(Effect::StartAssistant { request, .. }) => request,
                _ => panic!("expected assistant start effect"),
            },
            "read"
        ));
    }

    #[test]
    fn provider_request_uses_registry_tools() {
        use std::sync::Arc;

        use crate::plugin::ToolRegistry;

        let session = Session::new("plugin request test");
        let registry = Arc::new(ToolRegistry::built_in());
        let mut state = AppState::new_with_tool_registry(session, registry);
        state.draft_input = "hello".to_string();

        let effects = update(&mut state, Msg::SubmitPrompt);
        let request = match effects.get(1) {
            Some(Effect::StartAssistant { request, .. }) => request,
            _ => panic!("expected assistant start effect"),
        };

        assert!(request_contains_tool_name(request, "uppercase_text"));
        assert!(request_contains_tool_name(request, "read"));
    }

    #[test]
    fn tool_call_enters_approval_state_and_approval_resumes() {
        let mut state = AppState::new(Session::new("tool flow"));
        state.draft_input = "use uppercase_text: hello world".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        let tool_call = fluent_code_provider::ProviderToolCall {
            id: "tool-call-1".to_string(),
            name: "uppercase_text".to_string(),
            arguments: serde_json::json!({ "text": "hello world" }),
        };

        let effects = update(&mut state, Msg::AssistantToolCall { run_id, tool_call });
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::PersistSession))
        );
        assert!(matches!(state.status, AppStatus::AwaitingToolApproval));
        assert_eq!(state.session.tool_invocations.len(), 1);
        assert_eq!(
            state.session.tool_invocations[0].approval_state,
            ToolApprovalState::Pending
        );

        let effects = update(&mut state, Msg::ApprovePendingTool);
        assert!(matches!(state.status, AppStatus::RunningTool));
        assert!(matches!(
            effects.last(),
            Some(Effect::ExecuteTool { tool_call, .. }) if tool_call.name == "uppercase_text"
        ));

        let invocation_id = state.session.tool_invocations[0].id;
        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Ok("HELLO WORLD".to_string()),
            },
        );

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Completed
        );
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { request, .. })
                if request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == "HELLO WORLD"))
        ));
    }

    #[test]
    fn multi_tool_batch_waits_for_all_terminal_results_before_resuming() {
        let mut state = AppState::new(Session::new("multi tool flow"));
        state.draft_input = "inspect repository with multiple tools".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-read-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "Cargo.toml" }),
                },
            },
        );
        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-glob-2".to_string(),
                    name: "glob".to_string(),
                    arguments: serde_json::json!({ "pattern": "**/*.rs" }),
                },
            },
        );

        assert!(matches!(state.status, AppStatus::AwaitingToolApproval));
        assert_eq!(state.session.tool_invocations.len(), 2);

        let effects = update(&mut state, Msg::ApprovePendingTool);
        assert!(matches!(state.status, AppStatus::RunningTool));
        let approved_invocation_ids = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ExecuteTool { invocation_id, .. } => Some(*invocation_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(approved_invocation_ids.len(), 2);

        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id: approved_invocation_ids[0],
                result: Ok("[\"src/main.rs\"]".to_string()),
            },
        );

        assert!(matches!(state.status, AppStatus::RunningTool));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartAssistant { .. }))
        );

        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id: approved_invocation_ids[1],
                result: Ok("[\"src/lib.rs\"]".to_string()),
            },
        );

        assert!(matches!(state.status, AppStatus::Generating));
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { request, .. })
                if request.messages.iter().filter(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { .. })).count() == 2
                    && request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == "[\"src/main.rs\"]"))
                    && request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == "[\"src/lib.rs\"]"))
        ));
    }

    #[test]
    fn approve_pending_tool_approves_all_pending_calls_in_batch() {
        let mut state = AppState::new(Session::new("batch approval flow"));
        state.draft_input = "run multiple tools".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-read-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "Cargo.toml" }),
                },
            },
        );
        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-glob-2".to_string(),
                    name: "glob".to_string(),
                    arguments: serde_json::json!({ "pattern": "**/*.rs" }),
                },
            },
        );

        let effects = update(&mut state, Msg::ApprovePendingTool);

        assert!(matches!(state.status, AppStatus::RunningTool));
        assert_eq!(
            state
                .session
                .tool_invocations
                .iter()
                .filter(|invocation| invocation.approval_state == ToolApprovalState::Approved)
                .count(),
            2
        );
        assert_eq!(
            state
                .session
                .tool_invocations
                .iter()
                .filter(|invocation| invocation.execution_state == ToolExecutionState::Running)
                .count(),
            2
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::ExecuteTool { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn deny_pending_tool_resumes_with_denial_result() {
        let mut state = AppState::new(Session::new("deny tool flow"));
        state.draft_input = "use uppercase_text: deny me".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-1".to_string(),
                    name: "uppercase_text".to_string(),
                    arguments: serde_json::json!({ "text": "deny me" }),
                },
            },
        );

        let effects = update(&mut state, Msg::DenyPendingTool);

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].approval_state,
            ToolApprovalState::Denied
        );
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { request, .. })
                if request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == "Tool execution denied by user"))
        ));
    }

    #[test]
    fn failed_read_missing_path_resumes_with_tool_error_result() {
        let mut state = AppState::new(Session::new("failed read flow"));
        state.draft_input = "inspect missing file".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-read-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "missing.txt" }),
                },
            },
        );

        let effects = update(&mut state, Msg::ApprovePendingTool);
        assert!(matches!(state.status, AppStatus::RunningTool));
        assert!(matches!(
            effects.last(),
            Some(Effect::ExecuteTool { tool_call, .. }) if tool_call.name == "read"
        ));

        let invocation_id = state.session.tool_invocations[0].id;
        let tool_error =
            "provider error: path '/tmp/fluent-code-test/missing.txt' is not accessible: No such file or directory (os error 2)".to_string();
        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Err(tool_error.clone()),
            },
        );

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(state.active_run_id, Some(run_id));
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Failed
        );
        assert_eq!(
            state.session.tool_invocations[0].error.as_deref(),
            Some(tool_error.as_str())
        );
        assert!(matches!(
            state.session.latest_run_status(),
            Some(RunStatus::InProgress)
        ));
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { request, .. })
                if request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == &tool_error))
        ));
    }

    #[test]
    fn failed_non_read_tool_still_ends_run() {
        let mut state = AppState::new(Session::new("failed non-read flow"));
        state.draft_input = "use uppercase_text".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "tool-call-upper-1".to_string(),
                    name: "uppercase_text".to_string(),
                    arguments: serde_json::json!({ "text": "hello" }),
                },
            },
        );

        update(&mut state, Msg::ApprovePendingTool);

        let invocation_id = state.session.tool_invocations[0].id;
        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Err("provider error: uppercase_text exploded".to_string()),
            },
        );

        assert!(matches!(state.status, AppStatus::Error(_)));
        assert!(state.active_run_id.is_none());
        assert!(matches!(
            state.session.latest_run_status(),
            Some(RunStatus::Failed)
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartAssistant { .. }))
        );
    }

    #[test]
    fn resume_request_skips_tool_replay_when_tool_call_id_is_empty() {
        let mut state = AppState::new(Session::new("empty tool call id flow"));
        state.draft_input = "inspect repo".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "missing.txt" }),
                },
            },
        );

        update(&mut state, Msg::ApprovePendingTool);

        let invocation_id = state.session.tool_invocations[0].id;
        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Err(
                    "provider error: path '/tmp/fluent-code-test/missing.txt' is not accessible: No such file or directory (os error 2)"
                        .to_string(),
                ),
            },
        );

        let request = match effects.last() {
            Some(Effect::StartAssistant { request, .. }) => request,
            _ => panic!("expected assistant resume effect"),
        };

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(request.messages.len(), 1);
        assert!(matches!(
            request.messages.first(),
            Some(fluent_code_provider::ProviderMessage::UserText { text }) if text == "inspect repo"
        ));
        assert!(!request.messages.iter().any(|message| matches!(
            message,
            fluent_code_provider::ProviderMessage::AssistantToolCall { .. }
                | fluent_code_provider::ProviderMessage::ToolResult { .. }
        )));
    }

    #[test]
    fn assistant_done_completes_active_run() {
        let mut state = AppState::new(Session::new("done flow"));
        state.draft_input = "hello".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: "hi".to_string(),
            },
        );
        update(&mut state, Msg::AssistantDone { run_id });

        assert!(matches!(state.status, AppStatus::Idle));
        assert!(state.active_run_id.is_none());
        assert!(matches!(
            state.session.latest_run_status(),
            Some(RunStatus::Completed)
        ));
        assert!(matches!(
            state.session.turns.last().map(|turn| turn.role),
            Some(Role::Assistant)
        ));
    }

    #[test]
    fn plugin_tool_call_records_plugin_tool_source() {
        use std::sync::Arc;

        use crate::plugin::ToolRegistry;
        use crate::session::model::ToolSource;

        let tool_registry = Arc::new(ToolRegistry::with_plugin_tool_source_for_tests(
            "plugin_echo",
            "project.echo",
            "Project Echo",
            "1.2.3",
            crate::plugin::DiscoveryScope::Project,
        ));

        let mut state = AppState::new_with_tool_registry(Session::new("plugin source flow"), tool_registry);
        state.draft_input = "run plugin tool".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "plugin-call-1".to_string(),
                    name: "plugin_echo".to_string(),
                    arguments: serde_json::json!({ "text": "hello" }),
                },
            },
        );

        assert!(matches!(
            state.session.tool_invocations[0].tool_source,
            ToolSource::Plugin {
                ref plugin_id,
                ref plugin_name,
                ref plugin_version,
                scope: crate::plugin::DiscoveryScope::Project,
            } if plugin_id == "project.echo"
                && plugin_name == "Project Echo"
                && plugin_version == "1.2.3"
        ));
    }

    fn request_contains_tool_name(
        request: &fluent_code_provider::ProviderRequest,
        name: &str,
    ) -> bool {
        request.tools.iter().any(|tool| tool.name == name)
    }
}
