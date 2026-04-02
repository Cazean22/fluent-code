use chrono::Utc;
use fluent_code_provider::ProviderToolCall;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent::TASK_TOOL_NAME;
use crate::app::delegation::{ChildRunOutcome, complete_child_run, start_child_run};
use crate::app::permissions::{
    PermissionDecision, PermissionReply, can_remember_reply, denial_message,
    evaluate_tool_permission, remember_reply, tool_denied_by_policy_message,
};
use crate::app::request_builder::build_provider_request;
use crate::app::{AppState, AppStatus, Effect, Msg};
use crate::session::model::{
    ForegroundPhase, Role, RunStatus, ToolApprovalState, ToolExecutionState, ToolInvocationId,
    ToolInvocationRecord, ToolPermissionAction, TranscriptItemContent, TranscriptItemRecord,
    TranscriptPermissionState, TranscriptStreamState, Turn, transcript_assistant_reasoning_item_id,
    transcript_assistant_text_item_id, transcript_permission_item_id,
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
                reasoning: String::new(),
                sequence_number: state.session.allocate_replay_sequence(),
                timestamp: Utc::now(),
            };
            state.session.turns.push(turn);
            let user_turn = state
                .session
                .turns
                .last()
                .expect("user turn just pushed")
                .clone();
            state
                .session
                .upsert_transcript_item(TranscriptItemRecord::from_turn(&user_turn));
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::InProgress);
            upsert_run_started_transcript_item(state, run_id);
            state.draft_input.clear();
            state.set_foreground(run_id, ForegroundPhase::Generating, None);

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
        Msg::ReplyToPendingTool(reply) => {
            let Some(run_id) = state.active_run_id else {
                debug!(
                    session_id = %state.session.id,
                    "ignored tool reply because no run is active"
                );
                return Vec::new();
            };

            let Some(preceding_turn_id) =
                current_foreground_batch_anchor(state, run_id).or_else(|| {
                    state
                        .session
                        .pending_tool_invocation()
                        .filter(|invocation| invocation.run_id == run_id)
                        .map(|invocation| invocation.preceding_turn_id)
                })
            else {
                return Vec::new();
            };

            if matches!(reply, PermissionReply::Deny) {
                return deny_pending_tool_batch(state, run_id, preceding_turn_id);
            }

            let approved_at = Utc::now();
            let mut approved_tool_calls = Vec::new();
            let mut delegated_child_start = None;
            let mut remembered_policies = Vec::new();
            let mut approved_invocation_ids = Vec::new();

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
                invocation.approved_at = Some(approved_at);
                invocation.error = None;
                approved_invocation_ids.push(invocation.id);

                if matches!(reply, PermissionReply::Always)
                    && let Some(policy) = state.tool_registry.tool_policy(&invocation.tool_name)
                    && can_remember_reply(&policy, reply)
                {
                    remembered_policies.push(policy);
                }

                if invocation.tool_name == TASK_TOOL_NAME {
                    invocation.execution_state = ToolExecutionState::Running;
                    if delegated_child_start.is_none() {
                        delegated_child_start = Some((
                            invocation.id,
                            ProviderToolCall {
                                id: invocation.tool_call_id.clone(),
                                name: invocation.tool_name.clone(),
                                arguments: invocation.arguments.clone(),
                            },
                        ));
                    }
                } else {
                    invocation.execution_state = ToolExecutionState::Running;
                    approved_tool_calls.push((
                        invocation.id,
                        ProviderToolCall {
                            id: invocation.tool_call_id.clone(),
                            name: invocation.tool_name.clone(),
                            arguments: invocation.arguments.clone(),
                        },
                    ));
                }
            }

            if approved_tool_calls.is_empty() && delegated_child_start.is_none() {
                return Vec::new();
            }

            for policy in remembered_policies {
                remember_reply(&mut state.session, &policy, reply);
            }

            let permission_action = permission_action_for_reply(reply);
            for invocation_id in approved_invocation_ids {
                upsert_tool_invocation_transcript_item(state, invocation_id);
                upsert_permission_transcript_item(
                    state,
                    invocation_id,
                    TranscriptPermissionState::Approved,
                    Some(permission_action),
                );
            }

            state.set_foreground(run_id, ForegroundPhase::RunningTool, preceding_turn_id);
            state.session.updated_at = Utc::now();

            info!(
                session_id = %state.session.id,
                run_id = %run_id,
                approved_count = approved_tool_calls.len(),
                reply = ?reply,
                "resolved pending tool invocation batch"
            );

            let mut effects = vec![Effect::PersistSession];
            if let Some((invocation_id, tool_call)) = delegated_child_start {
                effects.extend(start_child_run(state, run_id, invocation_id, &tool_call));
            }
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
        Msg::CancelActiveRun => {
            let Some(run_id) = state.active_run_id else {
                debug!(
                    session_id = %state.session.id,
                    "ignored cancel because no run is active"
                );
                return Vec::new();
            };

            if state
                .session
                .find_run(run_id)
                .is_some_and(|run| run.parent_run_id.is_some())
                && let Some(mut parent_effects) =
                    complete_child_run(state, run_id, ChildRunOutcome::Cancelled)
            {
                parent_effects.insert(0, Effect::CancelAssistant { run_id });
                return parent_effects;
            }

            commit_open_assistant_transcript_items_for_run(state, run_id);
            state.clear_foreground();
            state.status = AppStatus::Idle;
            state.session.updated_at = Utc::now();
            state.session.upsert_run(run_id, RunStatus::Cancelled);
            upsert_run_terminal_transcript_item(state, run_id);

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
            let should_checkpoint = state.should_checkpoint_now();
            let chunk_bytes = delta.len();

            let existing_turn = active_assistant_turn_id(state, run_id).is_some();
            let (turn_id, turn_sequence_number) = {
                let turn = ensure_active_assistant_turn(state, run_id);
                turn.content.push_str(&delta);
                (turn.id, turn.sequence_number)
            };
            upsert_assistant_text_transcript_item(
                state,
                run_id,
                turn_id,
                turn_sequence_number,
                delta,
            );
            state.session.updated_at = Utc::now();
            if existing_turn {
                debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, "appended assistant chunk to existing turn");
            } else {
                debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, "started assistant turn from first chunk");
            }

            if should_checkpoint {
                vec![Effect::PersistSessionIfDue]
            } else {
                Vec::new()
            }
        }
        Msg::AssistantReasoningChunk { run_id, delta } => {
            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    active_run_id = ?state.active_run_id,
                    "ignored stale assistant reasoning chunk"
                );
                return Vec::new();
            }

            state.status = AppStatus::Generating;
            let should_checkpoint = state.should_checkpoint_now();
            let chunk_bytes = delta.len();
            let existing_turn = active_assistant_turn_id(state, run_id).is_some();
            let had_text = active_assistant_turn_ref(state, run_id)
                .map(|turn| !turn.content.is_empty())
                .unwrap_or(false);
            let (turn_id, turn_sequence_number) = {
                let turn = ensure_active_assistant_turn(state, run_id);
                turn.reasoning.push_str(&delta);
                (turn.id, turn.sequence_number)
            };
            upsert_assistant_reasoning_transcript_item(
                state,
                run_id,
                turn_id,
                turn_sequence_number,
                delta,
            );
            state.session.updated_at = Utc::now();

            if existing_turn {
                debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, had_text, "appended assistant reasoning chunk to existing turn");
            } else {
                debug!(run_id = %run_id, chunk_bytes, checkpoint_due = should_checkpoint, "started assistant turn from first reasoning chunk");
            }

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
            let Some(tool_policy) = state.tool_registry.tool_policy(&tool_name) else {
                state.status = AppStatus::Error(format!("unsupported tool '{}'", tool_name));
                state.session.updated_at = Utc::now();
                state.session.upsert_run(run_id, RunStatus::Failed);
                state.clear_foreground();
                return vec![Effect::PersistSession];
            };

            // Check agent-level tool permissions: if the active agent disallows
            // this tool, deny it immediately without creating an approval prompt.
            let agent_denied = active_agent_for_run(state, run_id)
                .is_some_and(|agent| !agent.tool_permissions.is_tool_permitted(&tool_name));

            commit_open_assistant_transcript_items_for_run(state, run_id);

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
                delegation: None,
                sequence_number: state.session.allocate_replay_sequence(),
                requested_at: Utc::now(),
                approved_at: None,
                completed_at: None,
            };

            state.session.tool_invocations.push(invocation);
            let invocation_id = state
                .session
                .tool_invocations
                .last()
                .expect("tool invocation just pushed")
                .id;
            upsert_tool_invocation_transcript_item(state, invocation_id);
            state.session.updated_at = Utc::now();
            let permission_decision = if agent_denied {
                PermissionDecision::Deny
            } else {
                evaluate_tool_permission(&state.session, &tool_policy)
            };

            match permission_decision {
                PermissionDecision::Allow => {
                    let (invocation_id, tool_name, tool_call_id, arguments, batch_anchor_turn_id) = {
                        let invocation = state
                            .session
                            .tool_invocations
                            .last_mut()
                            .expect("tool invocation just pushed");
                        invocation.approval_state = ToolApprovalState::Approved;
                        invocation.execution_state = ToolExecutionState::Running;
                        invocation.approved_at = Some(Utc::now());
                        (
                            invocation.id,
                            invocation.tool_name.clone(),
                            invocation.tool_call_id.clone(),
                            invocation.arguments.clone(),
                            invocation.preceding_turn_id,
                        )
                    };
                    upsert_tool_invocation_transcript_item(state, invocation_id);
                    state.set_foreground(
                        run_id,
                        ForegroundPhase::RunningTool,
                        batch_anchor_turn_id,
                    );
                    info!(
                        session_id = %state.session.id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        tool_name = %tool_name,
                        tool_call_id = %tool_call_id,
                        "assistant tool auto-approved by permission policy"
                    );

                    vec![
                        Effect::PersistSession,
                        Effect::ExecuteTool {
                            run_id,
                            invocation_id,
                            tool_call: ProviderToolCall {
                                id: tool_call_id,
                                name: tool_name,
                                arguments,
                            },
                        },
                    ]
                }
                PermissionDecision::Ask => {
                    let (invocation_id, tool_name, tool_call_id, batch_anchor_turn_id) = {
                        let invocation = state
                            .session
                            .tool_invocations
                            .last_mut()
                            .expect("tool invocation just pushed");
                        (
                            invocation.id,
                            invocation.tool_name.clone(),
                            invocation.tool_call_id.clone(),
                            invocation.preceding_turn_id,
                        )
                    };
                    upsert_permission_transcript_item(
                        state,
                        invocation_id,
                        TranscriptPermissionState::Pending,
                        None,
                    );
                    state.set_foreground(
                        run_id,
                        ForegroundPhase::AwaitingToolApproval,
                        batch_anchor_turn_id,
                    );
                    info!(
                        session_id = %state.session.id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        tool_name = %tool_name,
                        tool_call_id = %tool_call_id,
                        "assistant entered tool approval state"
                    );

                    vec![Effect::PersistSession]
                }
                PermissionDecision::Deny => {
                    let (invocation_id, preceding_turn_id) = {
                        let invocation = state
                            .session
                            .tool_invocations
                            .last_mut()
                            .expect("tool invocation just pushed");
                        let preceding_turn_id = invocation.preceding_turn_id;
                        invocation.approval_state = ToolApprovalState::Denied;
                        invocation.execution_state = ToolExecutionState::Skipped;
                        invocation.error =
                            Some(tool_denied_by_policy_message(&invocation.tool_name));
                        invocation.completed_at = Some(Utc::now());
                        (invocation.id, preceding_turn_id)
                    };
                    state.session.updated_at = Utc::now();
                    upsert_tool_invocation_transcript_item(state, invocation_id);

                    match tool_batch_progress(state, run_id, preceding_turn_id) {
                        ToolBatchProgress::AwaitingApproval => {
                            state.set_foreground(
                                run_id,
                                ForegroundPhase::AwaitingToolApproval,
                                preceding_turn_id,
                            );
                            vec![Effect::PersistSession]
                        }
                        ToolBatchProgress::Running => {
                            state.set_foreground(
                                run_id,
                                ForegroundPhase::RunningTool,
                                preceding_turn_id,
                            );
                            vec![Effect::PersistSession]
                        }
                        ToolBatchProgress::ReadyToResume => {
                            state.set_foreground(run_id, ForegroundPhase::Generating, None);
                            let request = build_provider_request(state, run_id);
                            vec![
                                Effect::PersistSession,
                                Effect::StartAssistant { run_id, request },
                            ]
                        }
                    }
                }
            }
        }
        Msg::AssistantDone { run_id } => {
            if state.active_run_id == Some(run_id) {
                if let Some(parent_effects) =
                    complete_child_run(state, run_id, ChildRunOutcome::Completed)
                {
                    return parent_effects;
                }

                commit_open_assistant_transcript_items_for_run(state, run_id);
                state.clear_foreground();
                state.status = AppStatus::Idle;
                state.session.updated_at = Utc::now();
                state.session.upsert_run(run_id, RunStatus::Completed);
                upsert_run_terminal_transcript_item(state, run_id);

                info!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    "assistant run completed"
                );

                return vec![Effect::PersistSession];
            }

            if state.session.find_run(run_id).is_some()
                && let Some(parent_effects) =
                    complete_child_run(state, run_id, ChildRunOutcome::Completed)
            {
                return parent_effects;
            }

            debug!(
                session_id = %state.session.id,
                run_id = %run_id,
                active_run_id = ?state.active_run_id,
                "ignored stale assistant completion"
            );
            Vec::new()
        }
        Msg::AssistantFailed { run_id, error } => {
            if state.active_run_id == Some(run_id) {
                if let Some(parent_effects) = complete_child_run(
                    state,
                    run_id,
                    ChildRunOutcome::Failed {
                        error: error.clone(),
                    },
                ) {
                    return parent_effects;
                }

                commit_open_assistant_transcript_items_for_run(state, run_id);
                state.clear_foreground();
                state.status = AppStatus::Error(error);
                state.session.updated_at = Utc::now();
                state.session.upsert_run(run_id, RunStatus::Failed);
                upsert_run_terminal_transcript_item(state, run_id);

                if let AppStatus::Error(message) = &state.status {
                    warn!(
                        session_id = %state.session.id,
                        run_id = %run_id,
                        error = %message,
                        "assistant run failed"
                    );
                }

                return vec![Effect::PersistSession];
            }

            if state.session.find_run(run_id).is_some()
                && let Some(parent_effects) = complete_child_run(
                    state,
                    run_id,
                    ChildRunOutcome::Failed {
                        error: error.clone(),
                    },
                )
            {
                return parent_effects;
            }

            debug!(
                session_id = %state.session.id,
                run_id = %run_id,
                active_run_id = ?state.active_run_id,
                error = %error,
                "ignored stale assistant failure"
            );
            Vec::new()
        }
        Msg::ToolExecutionFinished {
            run_id,
            invocation_id,
            result,
        } => {
            if state.active_run_id != Some(run_id)
                && state
                    .session
                    .find_run(run_id)
                    .is_some_and(|run| run.status.is_terminal())
            {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    invocation_id = %invocation_id,
                    active_run_id = ?state.active_run_id,
                    "ignored stale tool result for terminal run"
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

            let mut terminal_tool_failure = None;
            {
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
                            terminal_tool_failure = Some((tool_name, error));
                        }
                    }
                }
            }

            upsert_tool_invocation_transcript_item(state, invocation_id);

            if let Some((tool_name, error)) = terminal_tool_failure {
                commit_open_assistant_transcript_items_for_run(state, run_id);
                state.status = AppStatus::Error(error);
                state.session.updated_at = Utc::now();
                state.session.upsert_run(run_id, RunStatus::Failed);
                upsert_run_terminal_transcript_item(state, run_id);
                state.clear_foreground();
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

            state.session.updated_at = Utc::now();

            if state.active_run_id != Some(run_id) {
                debug!(
                    session_id = %state.session.id,
                    run_id = %run_id,
                    invocation_id = %invocation_id,
                    active_run_id = ?state.active_run_id,
                    "recorded tool result for non-foreground run"
                );
                return vec![Effect::PersistSession];
            }

            match tool_batch_progress(state, run_id, preceding_turn_id) {
                ToolBatchProgress::AwaitingApproval => {
                    state.set_foreground(
                        run_id,
                        ForegroundPhase::AwaitingToolApproval,
                        preceding_turn_id,
                    );
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "tool execution finished and waiting for remaining tool approvals"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::Running => {
                    state.set_foreground(run_id, ForegroundPhase::RunningTool, preceding_turn_id);
                    info!(
                        session_id = %session_id,
                        run_id = %run_id,
                        invocation_id = %invocation_id,
                        "tool execution finished and another tool is still running"
                    );
                    return vec![Effect::PersistSession];
                }
                ToolBatchProgress::ReadyToResume => {
                    state.set_foreground(run_id, ForegroundPhase::Generating, None);
                }
            }

            let request = build_provider_request(state, run_id);

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

#[derive(Debug, Clone, Copy)]
enum AssistantTranscriptStream {
    Reasoning,
    Text,
}

fn active_assistant_turn_id(state: &AppState, run_id: Uuid) -> Option<Uuid> {
    let turn = state
        .session
        .turns
        .last()
        .filter(|turn| matches!(turn.role, Role::Assistant) && turn.run_id == run_id)?;

    if assistant_turn_has_tool_boundary(state, turn.id) {
        None
    } else {
        Some(turn.id)
    }
}

fn active_assistant_turn_ref(state: &AppState, run_id: Uuid) -> Option<&Turn> {
    let turn_id = active_assistant_turn_id(state, run_id)?;
    state
        .session
        .turns
        .iter()
        .rev()
        .find(|turn| turn.id == turn_id)
}

fn active_assistant_turn_mut(state: &mut AppState, run_id: Uuid) -> Option<&mut Turn> {
    let turn_id = active_assistant_turn_id(state, run_id)?;
    state
        .session
        .turns
        .iter_mut()
        .rev()
        .find(|turn| turn.id == turn_id)
}

fn ensure_active_assistant_turn(state: &mut AppState, run_id: Uuid) -> &mut Turn {
    if active_assistant_turn_mut(state, run_id).is_none() {
        let sequence_number = state.session.allocate_replay_sequence();
        state.session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            sequence_number,
            timestamp: Utc::now(),
        });
    }

    active_assistant_turn_mut(state, run_id).expect("assistant turn just ensured")
}

fn assistant_turn_has_tool_boundary(state: &AppState, turn_id: Uuid) -> bool {
    state
        .session
        .tool_invocations
        .iter()
        .any(|invocation| invocation.preceding_turn_id == Some(turn_id))
}

fn assistant_turn_has_transcript_stream(state: &AppState, turn_id: Uuid) -> bool {
    state
        .session
        .find_transcript_item(transcript_assistant_reasoning_item_id(turn_id))
        .is_some()
        || state
            .session
            .find_transcript_item(transcript_assistant_text_item_id(turn_id))
            .is_some()
}

fn upsert_run_started_transcript_item(state: &mut AppState, run_id: Uuid) {
    let Some(run) = state.session.find_run(run_id).cloned() else {
        return;
    };
    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::run_started(&run));
}

fn upsert_run_terminal_transcript_item(state: &mut AppState, run_id: Uuid) {
    let Some(run) = state.session.find_run(run_id).cloned() else {
        return;
    };
    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::run_terminal(&run));
}

fn upsert_tool_invocation_transcript_item(state: &mut AppState, invocation_id: ToolInvocationId) {
    let Some(invocation) = state
        .session
        .tool_invocations
        .iter()
        .find(|invocation| invocation.id == invocation_id)
        .cloned()
    else {
        return;
    };

    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::from_tool_invocation(&invocation));
}

fn upsert_permission_transcript_item(
    state: &mut AppState,
    invocation_id: ToolInvocationId,
    permission_state: TranscriptPermissionState,
    decision: Option<ToolPermissionAction>,
) {
    let Some(invocation) = state
        .session
        .tool_invocations
        .iter()
        .find(|invocation| invocation.id == invocation_id)
        .cloned()
    else {
        return;
    };

    let item_id = transcript_permission_item_id(invocation_id);
    let sequence_number = state
        .session
        .find_transcript_item(item_id)
        .map(|item| item.sequence_number)
        .unwrap_or_else(|| state.session.allocate_replay_sequence());
    let item =
        TranscriptItemRecord::permission(&invocation, sequence_number, permission_state, decision);
    state.session.upsert_transcript_item(item);
}

fn commit_open_assistant_transcript_items_for_run(state: &mut AppState, run_id: Uuid) {
    let Some(turn_id) = state
        .session
        .turns
        .iter()
        .rev()
        .find(|turn| turn.run_id == run_id && matches!(turn.role, Role::Assistant))
        .map(|turn| turn.id)
    else {
        return;
    };

    commit_assistant_transcript_items_for_turn(state, turn_id);
}

fn commit_assistant_transcript_items_for_turn(state: &mut AppState, turn_id: Uuid) {
    for item_id in [
        transcript_assistant_reasoning_item_id(turn_id),
        transcript_assistant_text_item_id(turn_id),
    ] {
        if let Some(item) = state.session.find_transcript_item_mut(item_id) {
            item.stream_state = TranscriptStreamState::Committed;
        }
    }
}

fn upsert_assistant_reasoning_transcript_item(
    state: &mut AppState,
    run_id: Uuid,
    turn_id: Uuid,
    turn_sequence_number: u64,
    delta: String,
) {
    upsert_assistant_stream_transcript_item(
        state,
        run_id,
        turn_id,
        turn_sequence_number,
        delta,
        AssistantTranscriptStream::Reasoning,
    );
}

fn upsert_assistant_text_transcript_item(
    state: &mut AppState,
    run_id: Uuid,
    turn_id: Uuid,
    turn_sequence_number: u64,
    delta: String,
) {
    upsert_assistant_stream_transcript_item(
        state,
        run_id,
        turn_id,
        turn_sequence_number,
        delta,
        AssistantTranscriptStream::Text,
    );
}

fn upsert_assistant_stream_transcript_item(
    state: &mut AppState,
    run_id: Uuid,
    turn_id: Uuid,
    turn_sequence_number: u64,
    delta: String,
    stream: AssistantTranscriptStream,
) {
    let item_id = match stream {
        AssistantTranscriptStream::Reasoning => transcript_assistant_reasoning_item_id(turn_id),
        AssistantTranscriptStream::Text => transcript_assistant_text_item_id(turn_id),
    };

    if let Some(item) = state.session.find_transcript_item_mut(item_id) {
        if let TranscriptItemContent::Turn(content) = &mut item.content {
            match stream {
                AssistantTranscriptStream::Reasoning => content.reasoning.push_str(&delta),
                AssistantTranscriptStream::Text => content.content.push_str(&delta),
            }
        }
        item.stream_state = TranscriptStreamState::Open;
        return;
    }

    let sequence_number = if assistant_turn_has_transcript_stream(state, turn_id) {
        state.session.allocate_replay_sequence()
    } else {
        turn_sequence_number
    };

    let item = match stream {
        AssistantTranscriptStream::Reasoning => TranscriptItemRecord::assistant_reasoning(
            run_id,
            turn_id,
            sequence_number,
            delta,
            TranscriptStreamState::Open,
        ),
        AssistantTranscriptStream::Text => TranscriptItemRecord::assistant_text(
            run_id,
            turn_id,
            sequence_number,
            delta,
            TranscriptStreamState::Open,
        ),
    };
    state.session.upsert_transcript_item(item);
}

fn permission_action_for_reply(reply: PermissionReply) -> ToolPermissionAction {
    match reply {
        PermissionReply::Once | PermissionReply::Always => ToolPermissionAction::Allow,
        PermissionReply::Deny => ToolPermissionAction::Deny,
    }
}

fn should_resume_after_tool_failure(tool_name: &str, error: &str) -> bool {
    tool_name == READ_TOOL_NAME && error.contains("is not accessible")
}

fn current_foreground_batch_anchor(state: &AppState, run_id: Uuid) -> Option<Option<Uuid>> {
    state
        .session
        .foreground_owner
        .as_ref()
        .filter(|owner| {
            owner.run_id == run_id && owner.phase == ForegroundPhase::AwaitingToolApproval
        })
        .map(|owner| owner.batch_anchor_turn_id)
}

fn deny_pending_tool_batch(
    state: &mut AppState,
    run_id: Uuid,
    preceding_turn_id: Option<Uuid>,
) -> Vec<Effect> {
    let session_id = state.session.id;
    let denied_at = Utc::now();
    let mut denied_invocation_ids = Vec::new();

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
        invocation.approval_state = ToolApprovalState::Denied;
        invocation.execution_state = ToolExecutionState::Skipped;
        invocation.error = Some(denial_message(&invocation.tool_name));
        invocation.completed_at = Some(denied_at);
        denied_invocation_ids.push(invocation.id);
    }

    if denied_invocation_ids.is_empty() {
        return Vec::new();
    }

    for invocation_id in &denied_invocation_ids {
        upsert_tool_invocation_transcript_item(state, *invocation_id);
        upsert_permission_transcript_item(
            state,
            *invocation_id,
            TranscriptPermissionState::Denied,
            Some(ToolPermissionAction::Deny),
        );
    }

    state.session.updated_at = Utc::now();

    match tool_batch_progress(state, run_id, preceding_turn_id) {
        ToolBatchProgress::AwaitingApproval => {
            state.set_foreground(
                run_id,
                ForegroundPhase::AwaitingToolApproval,
                preceding_turn_id,
            );
            info!(
                session_id = %session_id,
                run_id = %run_id,
                denied_count = denied_invocation_ids.len(),
                "denied pending tool invocation batch and waiting for remaining tool decisions"
            );
            vec![Effect::PersistSession]
        }
        ToolBatchProgress::Running => {
            state.set_foreground(run_id, ForegroundPhase::RunningTool, preceding_turn_id);
            info!(
                session_id = %session_id,
                run_id = %run_id,
                denied_count = denied_invocation_ids.len(),
                "denied pending tool invocation batch while another tool is still running"
            );
            vec![Effect::PersistSession]
        }
        ToolBatchProgress::ReadyToResume => {
            state.set_foreground(run_id, ForegroundPhase::Generating, None);
            let request = build_provider_request(state, run_id);

            info!(
                session_id = %session_id,
                run_id = %run_id,
                denied_count = denied_invocation_ids.len(),
                "denied pending tool invocation batch and resumed assistant"
            );

            vec![
                Effect::PersistSession,
                Effect::StartAssistant { run_id, request },
            ]
        }
    }
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

/// Look up the agent definition for the run that owns `run_id`.
///
/// For child (delegated) runs we inspect the parent's task invocation to find
/// which agent was delegated. For root runs we return `None` because the
/// primary orchestrator is not constrained by agent-level tool permissions.
fn active_agent_for_run(state: &AppState, run_id: Uuid) -> Option<&crate::agent::AgentDefinition> {
    let run = state.session.find_run(run_id)?;
    let parent_invocation_id = run.parent_tool_invocation_id?;
    let invocation = state
        .session
        .tool_invocations
        .iter()
        .find(|inv| inv.id == parent_invocation_id)?;
    let agent_name = invocation.delegation_agent_name()?;
    state.agent_registry.get(agent_name)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread, time::Duration};

    use super::update;
    use crate::agent::AgentRegistry;
    use crate::app::permissions::PermissionReply;
    use crate::app::request_builder::build_provider_request;
    use crate::app::{AppState, AppStatus, Effect, Msg};
    use crate::config::AgentConfig;
    use crate::session::model::{
        Role, RunStatus, Session, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolPermissionAction, TranscriptItemContent, TranscriptPermissionState,
        TranscriptRunLifecycleEvent, TranscriptStreamState,
    };

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

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
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

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
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
    fn once_reply_approves_all_pending_calls_in_batch() {
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

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));

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
    fn deny_reply_resumes_with_denial_result() {
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

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Deny));

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].approval_state,
            ToolApprovalState::Denied
        );
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { request, .. })
                if request.messages.iter().any(|message| matches!(message, fluent_code_provider::ProviderMessage::ToolResult { content, .. } if content == "Permission denied for tool 'uppercase_text' by user"))
        ));
    }

    #[test]
    fn always_reply_persists_session_permission_rule() {
        let mut state = AppState::new(Session::new("remember approval flow"));
        state.draft_input = "use uppercase_text: remember me".to_string();
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
                    id: "tool-call-remember-1".to_string(),
                    name: "uppercase_text".to_string(),
                    arguments: serde_json::json!({ "text": "remember me" }),
                },
            },
        );

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Always));

        assert!(matches!(state.status, AppStatus::RunningTool));
        assert_eq!(state.session.permissions.rules.len(), 1);
        assert_eq!(
            state.session.permissions.rules[0].subject.tool_name,
            "uppercase_text"
        );
        assert!(matches!(
            effects.last(),
            Some(Effect::ExecuteTool { tool_call, .. }) if tool_call.name == "uppercase_text"
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

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
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

        update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));

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
    fn assistant_reasoning_chunk_starts_assistant_turn_before_text() {
        let mut state = AppState::new(Session::new("reasoning first flow"));
        state.draft_input = "hello".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        let effects = update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: "plan first".to_string(),
            },
        );

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::PersistSession))
        );
        assert_eq!(state.session.turns.len(), 2);
        assert!(matches!(state.session.turns[1].role, Role::Assistant));
        assert_eq!(state.session.turns[1].content, "");
        assert_eq!(state.session.turns[1].reasoning, "plan first");

        update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: "final answer".to_string(),
            },
        );

        assert_eq!(state.session.turns.len(), 2);
        assert_eq!(state.session.turns[1].content, "final answer");
        assert_eq!(state.session.turns[1].reasoning, "plan first");
    }

    #[test]
    fn long_stream_checkpointing_remains_throttled_while_chunks_continue() {
        let checkpoint_interval = Duration::from_millis(5);
        let mut state = AppState::new_with_checkpoint_interval(
            Session::new("long stream checkpoint flow"),
            checkpoint_interval,
        );
        state.draft_input = "hello".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        let first_chunk_effects = update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: "alpha".to_string(),
            },
        );

        assert!(matches!(
            first_chunk_effects.as_slice(),
            [Effect::PersistSessionIfDue]
        ));
        assert_eq!(state.session.turns[1].content, "alpha");
        assert_eq!(state.session.turns[1].reasoning, "");

        state.mark_checkpoint_saved();

        let second_chunk_effects = update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: " beta".to_string(),
            },
        );
        let first_reasoning_effects = update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: "plan".to_string(),
            },
        );
        let third_chunk_effects = update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: " gamma".to_string(),
            },
        );
        let second_reasoning_effects = update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: " more".to_string(),
            },
        );

        assert!(second_chunk_effects.is_empty());
        assert!(first_reasoning_effects.is_empty());
        assert!(third_chunk_effects.is_empty());
        assert!(second_reasoning_effects.is_empty());
        assert_eq!(state.session.turns[1].content, "alpha beta gamma");
        assert_eq!(state.session.turns[1].reasoning, "plan more");

        thread::sleep(checkpoint_interval + Duration::from_millis(10));

        let checkpoint_due_again_effects = update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: " done".to_string(),
            },
        );

        assert!(matches!(
            checkpoint_due_again_effects.as_slice(),
            [Effect::PersistSessionIfDue]
        ));
        assert_eq!(state.session.turns.len(), 2);
        assert_eq!(state.session.turns[1].content, "alpha beta gamma");
        assert_eq!(state.session.turns[1].reasoning, "plan more done");
    }

    #[test]
    fn build_provider_request_replays_assistant_text_without_reasoning() {
        let mut state = AppState::new(Session::new("reasoning replay boundary"));
        state.draft_input = "hello".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: "private chain".to_string(),
            },
        );
        update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: "public answer".to_string(),
            },
        );

        let request = build_provider_request(&state, run_id);

        assert!(request.messages.iter().any(|message| matches!(
            message,
            fluent_code_provider::ProviderMessage::AssistantText { text }
                if text == "public answer"
        )));
        assert!(!request.messages.iter().any(|message| matches!(
            message,
            fluent_code_provider::ProviderMessage::AssistantText { text }
                if text.contains("private chain")
        )));
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

        update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));

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
    fn assistant_completion_clears_foreground_after_streaming_chunks() {
        let mut state = AppState::new_with_checkpoint_interval(
            Session::new("streaming completion flow"),
            Duration::from_secs(60),
        );
        state.draft_input = "hello".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id,
                delta: "plan first".to_string(),
            },
        );
        state.mark_checkpoint_saved();
        update(
            &mut state,
            Msg::AssistantChunk {
                run_id,
                delta: "final answer".to_string(),
            },
        );

        let done_effects = update(&mut state, Msg::AssistantDone { run_id });

        assert!(matches!(done_effects.as_slice(), [Effect::PersistSession]));
        assert!(matches!(state.status, AppStatus::Idle));
        assert!(state.active_run_id.is_none());
        assert!(state.session.foreground_owner.is_none());
        assert!(matches!(
            state.session.latest_run_status(),
            Some(RunStatus::Completed)
        ));
        assert_eq!(state.session.turns.len(), 2);
        assert!(matches!(state.session.turns[1].role, Role::Assistant));
        assert_eq!(state.session.turns[1].content, "final answer");
        assert_eq!(state.session.turns[1].reasoning, "plan first");
    }

    #[test]
    fn plugin_tool_call_records_plugin_tool_source() {
        use crate::plugin::ToolRegistry;
        use crate::session::model::ToolSource;

        let tool_registry = Arc::new(ToolRegistry::with_plugin_tool_source_for_tests(
            "plugin_echo",
            "project.echo",
            "Project Echo",
            "1.2.3",
            crate::plugin::DiscoveryScope::Project,
        ));

        let mut state =
            AppState::new_with_tool_registry(Session::new("plugin source flow"), tool_registry);
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

    #[test]
    fn approving_task_tool_starts_child_run_with_lineage_and_prompt_override() {
        let mut state = AppState::new(Session::new("task delegation flow"));
        state.draft_input = "delegate work".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let parent_run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "explore",
                        "prompt": "Inspect the runtime orchestrator"
                    }),
                },
            },
        );

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        let child_run_id = state.active_run_id.expect("child run should become active");
        let child_request = match effects.last() {
            Some(Effect::StartAssistant { run_id, request }) if *run_id == child_run_id => request,
            _ => panic!("expected child assistant start effect"),
        };

        assert_ne!(child_run_id, parent_run_id);
        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.turns.last().map(|turn| turn.run_id),
            Some(child_run_id)
        );
        assert_eq!(
            state.session.turns.last().map(|turn| turn.content.as_str()),
            Some("Inspect the runtime orchestrator")
        );
        assert_eq!(
            state.session.tool_invocations[0].child_run_id(),
            Some(child_run_id)
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_agent_name(),
            Some("explore")
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_prompt(),
            Some("Inspect the runtime orchestrator")
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Running)
        );
        assert_eq!(
            state
                .session
                .find_run(child_run_id)
                .expect("child run record")
                .parent_run_id,
            Some(parent_run_id)
        );
        assert_eq!(
            child_request.system_prompt_override.as_deref(),
            Some(
                "You are the explore subagent. Investigate the repository carefully, follow existing code patterns, and answer with concrete findings grounded in the code you read. Focus on discovery, not implementation."
            )
        );
        assert!(child_request.tools.iter().all(|tool| tool.name != "task"));
    }

    #[test]
    fn child_completion_resumes_parent_with_synthetic_task_result() {
        let mut state = AppState::new(Session::new("task completion flow"));
        state.draft_input = "delegate work".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let parent_run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "librarian",
                        "prompt": "Summarize the provider layer"
                    }),
                },
            },
        );

        update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        let child_run_id = state.active_run_id.expect("child run should become active");

        update(
            &mut state,
            Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "Provider layer summary".to_string(),
            },
        );

        let effects = update(
            &mut state,
            Msg::AssistantDone {
                run_id: child_run_id,
            },
        );
        let parent_request = match effects.last() {
            Some(Effect::StartAssistant { run_id, request }) if *run_id == parent_run_id => request,
            _ => panic!("expected resumed parent assistant effect"),
        };

        assert_eq!(state.active_run_id, Some(parent_run_id));
        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Completed
        );
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some("Subagent finished: Provider layer summary")
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Completed)
        );
        assert!(matches!(
            state.session.find_run(child_run_id).map(|run| run.status),
            Some(RunStatus::Completed)
        ));
        assert!(parent_request.messages.iter().any(|message| matches!(
            message,
            fluent_code_provider::ProviderMessage::ToolResult { content, .. }
                if content == "Subagent finished: Provider layer summary"
        )));
    }

    #[test]
    fn cancelling_child_run_marks_delegation_cancelled_and_resumes_parent() {
        let mut state = AppState::new(Session::new("task cancel flow"));
        state.draft_input = "delegate work".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let parent_run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "explore",
                        "prompt": "Inspect cancellation flow"
                    }),
                },
            },
        );

        update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        let child_run_id = state.active_run_id.expect("child run should become active");

        let effects = update(&mut state, Msg::CancelActiveRun);

        assert!(
            matches!(effects.first(), Some(Effect::CancelAssistant { run_id }) if *run_id == child_run_id)
        );
        assert_eq!(state.active_run_id, Some(parent_run_id));
        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Completed
        );
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some("Subagent cancelled by user.")
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Cancelled)
        );
        assert!(matches!(
            state.session.find_run(child_run_id).map(|run| run.status),
            Some(RunStatus::Cancelled)
        ));
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { run_id, request }) if *run_id == parent_run_id
                && request.messages.iter().any(|message| matches!(
                    message,
                    fluent_code_provider::ProviderMessage::ToolResult { content, .. }
                        if content == "Subagent cancelled by user."
                ))
        ));
    }

    #[test]
    fn stale_tool_result_is_ignored_after_run_is_cancelled() {
        let mut state = AppState::new(Session::new("cancelled tool result"));
        state.draft_input = "run a tool".to_string();
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
                    id: "uppercase-call-1".to_string(),
                    name: "uppercase_text".to_string(),
                    arguments: serde_json::json!({ "text": "hello" }),
                },
            },
        );

        assert!(matches!(state.status, AppStatus::AwaitingToolApproval));

        update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        assert!(matches!(state.status, AppStatus::RunningTool));

        update(&mut state, Msg::CancelActiveRun);
        let invocation_id = state.session.tool_invocations[0].id;

        let effects = update(
            &mut state,
            Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Ok("HELLO".to_string()),
            },
        );

        assert!(effects.is_empty());
        assert!(state.active_run_id.is_none());
        assert!(matches!(state.status, AppStatus::Idle));
        assert_eq!(state.session.tool_invocations[0].result, None);
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Running
        );
        assert!(matches!(
            state.session.find_run(run_id).map(|run| run.status),
            Some(RunStatus::Cancelled)
        ));
    }

    #[test]
    fn custom_agent_registry_drives_task_delegation() {
        let agent_registry = Arc::new(
            AgentRegistry::from_agent_configs(&[AgentConfig {
                name: "oracle".to_string(),
                description: "Answer architecture questions.".to_string(),
                system_prompt: "You are the oracle subagent.".to_string(),
                tools_allowed: None,
                tools_denied: Some(vec!["task".to_string()]),
                delegation_targets: None,
            }])
            .expect("custom agent registry"),
        );
        let tool_registry = Arc::new(crate::plugin::ToolRegistry::with_agent_registry(
            &agent_registry,
        ));
        let mut state = AppState::new_with_registries(
            Session::new("custom task flow"),
            agent_registry,
            tool_registry,
        );
        state.draft_input = "delegate work".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let parent_run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "oracle",
                        "prompt": "Answer the architecture question"
                    }),
                },
            },
        );

        let effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        let child_run_id = state.active_run_id.expect("child run should become active");
        let child_request = match effects.last() {
            Some(Effect::StartAssistant { run_id, request }) if *run_id == child_run_id => request,
            _ => panic!("expected child assistant start effect"),
        };

        assert_ne!(child_run_id, parent_run_id);
        assert_eq!(
            state.session.tool_invocations[0].delegation_agent_name(),
            Some("oracle")
        );
        assert_eq!(
            child_request.system_prompt_override.as_deref(),
            Some("You are the oracle subagent.")
        );
        assert!(child_request.tools.iter().all(|tool| tool.name != "task"));
    }

    #[test]
    fn reducer_emits_canonical_transcript_items_in_exact_stream_order() {
        let mut state = AppState::new(Session::new("canonical transcript order"));
        state.draft_input = "delegate work".to_string();
        let effects = update(&mut state, Msg::SubmitPrompt);
        let parent_run_id = match effects.get(1) {
            Some(Effect::StartAssistant { run_id, .. }) => *run_id,
            _ => panic!("expected assistant start effect"),
        };

        update(
            &mut state,
            Msg::AssistantReasoningChunk {
                run_id: parent_run_id,
                delta: "think before tool".to_string(),
            },
        );
        update(
            &mut state,
            Msg::AssistantChunk {
                run_id: parent_run_id,
                delta: "delegating now".to_string(),
            },
        );
        update(
            &mut state,
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "explore",
                        "prompt": "Inspect runtime"
                    }),
                },
            },
        );

        let approval_effects = update(&mut state, Msg::ReplyToPendingTool(PermissionReply::Once));
        let child_run_id = state.active_run_id.expect("child run should be active");
        assert!(matches!(
            approval_effects.last(),
            Some(Effect::StartAssistant { run_id, .. }) if *run_id == child_run_id
        ));

        update(
            &mut state,
            Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "Child summary".to_string(),
            },
        );
        update(
            &mut state,
            Msg::AssistantDone {
                run_id: child_run_id,
            },
        );
        update(
            &mut state,
            Msg::AssistantChunk {
                run_id: parent_run_id,
                delta: "Parent final answer".to_string(),
            },
        );
        update(
            &mut state,
            Msg::AssistantDone {
                run_id: parent_run_id,
            },
        );

        assert_eq!(
            state
                .session
                .turns
                .iter()
                .filter(|turn| turn.run_id == parent_run_id && matches!(turn.role, Role::Assistant))
                .count(),
            2,
            "parent assistant output should split across the tool boundary"
        );

        let items = ordered_transcript_items(&state.session.transcript_items);
        assert_eq!(items.len(), 13);
        assert!(
            items
                .iter()
                .all(|item| item.stream_state == TranscriptStreamState::Committed)
        );

        assert_turn_text_item(items[0], parent_run_id, Role::User, "delegate work");
        assert_run_started_item(items[1], parent_run_id);
        assert_turn_reasoning_item(items[2], parent_run_id, "think before tool");
        assert_turn_text_item(items[3], parent_run_id, Role::Assistant, "delegating now");
        assert_task_tool_item(items[4], parent_run_id, child_run_id);
        assert_permission_item(items[5], parent_run_id);
        assert_run_started_item(items[6], child_run_id);
        assert_delegated_child_item(items[7], parent_run_id, child_run_id);
        assert_turn_text_item(items[8], child_run_id, Role::User, "Inspect runtime");
        assert_turn_text_item(items[9], child_run_id, Role::Assistant, "Child summary");
        assert_run_terminal_completed_item(items[10], child_run_id);
        assert_turn_text_item(
            items[11],
            parent_run_id,
            Role::Assistant,
            "Parent final answer",
        );
        assert_run_terminal_completed_item(items[12], parent_run_id);
    }

    fn request_contains_tool_name(
        request: &fluent_code_provider::ProviderRequest,
        name: &str,
    ) -> bool {
        request.tools.iter().any(|tool| tool.name == name)
    }

    fn ordered_transcript_items(
        items: &[crate::session::model::TranscriptItemRecord],
    ) -> Vec<&crate::session::model::TranscriptItemRecord> {
        let mut ordered = items.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|item| item.sequence_number);
        ordered
    }

    fn assert_turn_text_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
        expected_role: Role,
        expected_text: &str,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::Turn(content) => {
                assert_eq!(content.role, expected_role);
                assert_eq!(content.content, expected_text);
                assert!(content.reasoning.is_empty());
            }
            other => panic!("expected turn item, got {other:?}"),
        }
    }

    fn assert_turn_reasoning_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
        expected_reasoning: &str,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::Turn(content) => {
                assert!(matches!(content.role, Role::Assistant));
                assert!(content.content.is_empty());
                assert_eq!(content.reasoning, expected_reasoning);
            }
            other => panic!("expected reasoning turn item, got {other:?}"),
        }
    }

    fn assert_run_started_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::RunLifecycle(content) => {
                assert_eq!(content.event, TranscriptRunLifecycleEvent::Started);
                assert!(matches!(content.status, RunStatus::InProgress));
            }
            other => panic!("expected run-start item, got {other:?}"),
        }
    }

    fn assert_run_terminal_completed_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::RunLifecycle(content) => {
                assert_eq!(content.event, TranscriptRunLifecycleEvent::Terminal);
                assert!(matches!(content.status, RunStatus::Completed));
            }
            other => panic!("expected terminal run item, got {other:?}"),
        }
    }

    fn assert_task_tool_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
        child_run_id: uuid::Uuid,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::ToolInvocation(content) => {
                assert_eq!(content.tool_name, "task");
                assert!(matches!(
                    content.execution_state,
                    ToolExecutionState::Completed
                ));
                assert_eq!(
                    content.result.as_deref(),
                    Some("Subagent finished: Child summary")
                );
                assert_eq!(
                    content
                        .delegation
                        .as_ref()
                        .map(|delegation| delegation.child_run_id),
                    Some(Some(child_run_id))
                );
                assert_eq!(
                    content
                        .delegation
                        .as_ref()
                        .map(|delegation| delegation.status),
                    Some(TaskDelegationStatus::Completed)
                );
            }
            other => panic!("expected task tool item, got {other:?}"),
        }
    }

    fn assert_permission_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::Permission(content) => {
                assert_eq!(content.state, TranscriptPermissionState::Approved);
                assert_eq!(content.decision, Some(ToolPermissionAction::Allow));
            }
            other => panic!("expected permission item, got {other:?}"),
        }
    }

    fn assert_delegated_child_item(
        item: &crate::session::model::TranscriptItemRecord,
        expected_run_id: uuid::Uuid,
        child_run_id: uuid::Uuid,
    ) {
        assert_eq!(item.run_id, expected_run_id);
        match &item.content {
            TranscriptItemContent::DelegatedChild(content) => {
                assert_eq!(content.child_run_id, Some(child_run_id));
                assert_eq!(content.agent_name.as_deref(), Some("explore"));
                assert_eq!(content.prompt.as_deref(), Some("Inspect runtime"));
                assert_eq!(content.status, TaskDelegationStatus::Completed);
            }
            other => panic!("expected delegated-child item, got {other:?}"),
        }
    }
}
