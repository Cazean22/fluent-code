use chrono::Utc;
use fluent_code_provider::ProviderToolCall;
use tracing::{info, warn};
use uuid::Uuid;

use crate::agent::TASK_TOOL_NAME;
use crate::agent::parse_task_request;
use crate::app::request_builder::{build_provider_request, child_provider_request};
use crate::app::{AppState, AppStatus, Effect, Msg};
use crate::session::model::{
    ForegroundPhase, Role, RunStatus, RunTerminalStopReason, TaskDelegationStatus,
    ToolApprovalState, ToolExecutionState, ToolInvocationId, TranscriptItemRecord,
    TranscriptStreamState, Turn, transcript_assistant_reasoning_item_id,
    transcript_assistant_text_item_id, transcript_delegated_child_item_id,
};

pub const RESTART_INTERRUPTED_TASK_RESULT: &str =
    "Subagent interrupted by application restart before completion.";

#[derive(Debug, Clone)]
pub enum ChildRunOutcome {
    Completed,
    Failed { error: String },
    Cancelled,
    InterruptedByRestart,
}

#[derive(Debug, Clone, Copy)]
struct InterruptedChildRecoveryCandidate {
    parent_run_id: Uuid,
    parent_tool_invocation_id: ToolInvocationId,
    child_run_id: Uuid,
}

pub fn start_child_run(
    state: &mut AppState,
    parent_run_id: Uuid,
    invocation_id: ToolInvocationId,
    tool_call: &ProviderToolCall,
) -> Vec<Effect> {
    let session_id = state.session.id;
    let task_request = match parse_task_request(&state.agent_registry, &tool_call.arguments) {
        Ok(task_request) => task_request,
        Err(error) => {
            finish_task_invocation_with_error(state, invocation_id, &error.to_string());
            state.status = AppStatus::Generating;
            let request = build_provider_request(state, parent_run_id);
            warn!(
                session_id = %session_id,
                run_id = %parent_run_id,
                invocation_id = %invocation_id,
                error = %error,
                "task invocation could not be delegated"
            );
            return vec![
                Effect::PersistSession,
                Effect::StartAssistant {
                    run_id: parent_run_id,
                    request,
                },
            ];
        }
    };

    let delegated_agent = state.agent_registry.get(&task_request.agent).map(|agent| {
        (
            agent.name.clone(),
            agent.system_prompt.clone(),
            agent.tool_permissions.clone(),
        )
    });
    let Some((agent_name, agent_system_prompt, agent_tool_permissions)) = delegated_agent else {
        let error_message = format!("task requested unknown agent '{}'", task_request.agent);
        finish_task_invocation_with_error(state, invocation_id, &error_message);
        state.status = AppStatus::Generating;
        let request = build_provider_request(state, parent_run_id);
        warn!(
            session_id = %session_id,
            run_id = %parent_run_id,
            invocation_id = %invocation_id,
            agent = %task_request.agent,
            "task invocation referenced an unknown agent"
        );
        return vec![
            Effect::PersistSession,
            Effect::StartAssistant {
                run_id: parent_run_id,
                request,
            },
        ];
    };

    let child_run_id = Uuid::new_v4();
    if let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) {
        invocation.set_task_delegation(
            child_run_id,
            task_request.agent.clone(),
            task_request.prompt.clone(),
        );
    }
    upsert_tool_invocation_transcript_item(state, invocation_id);

    state.session.upsert_run_with_parent(
        child_run_id,
        RunStatus::InProgress,
        Some(parent_run_id),
        Some(invocation_id),
    );
    upsert_run_started_transcript_item(state, child_run_id);
    let delegated_child_sequence = state.session.allocate_replay_sequence();
    upsert_delegated_child_transcript_item(state, invocation_id, Some(delegated_child_sequence));
    state.set_foreground(child_run_id, ForegroundPhase::Generating, None);

    let sequence_number = state.session.allocate_replay_sequence();
    state.session.turns.push(Turn {
        id: Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::User,
        content: task_request.prompt.clone(),
        reasoning: String::new(),
        sequence_number,
        timestamp: Utc::now(),
    });
    let child_prompt_turn = state
        .session
        .turns
        .last()
        .expect("child prompt turn just pushed")
        .clone();
    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::from_turn(&child_prompt_turn));
    state.session.updated_at = Utc::now();

    let child_request = child_provider_request(
        state,
        task_request.prompt,
        agent_system_prompt,
        &agent_tool_permissions,
    );

    info!(
        session_id = %session_id,
        parent_run_id = %parent_run_id,
        child_run_id = %child_run_id,
        invocation_id = %invocation_id,
        agent = %agent_name,
        "started delegated child run in foreground"
    );

    vec![
        Effect::PersistSession,
        Effect::StartAssistant {
            run_id: child_run_id,
            request: child_request,
        },
    ]
}

pub fn complete_child_run(
    state: &mut AppState,
    child_run_id: Uuid,
    outcome: ChildRunOutcome,
) -> Option<Vec<Effect>> {
    let parent_link = state
        .session
        .find_run(child_run_id)
        .and_then(|run| Some((run.parent_run_id?, run.parent_tool_invocation_id?)));

    let (parent_run_id, parent_tool_invocation_id) = parent_link?;
    let session_id = state.session.id;

    let (child_status, child_stop_reason, delegation_status, synthetic_result) = match outcome {
        ChildRunOutcome::Completed => {
            let final_text = latest_assistant_text_for_run(state, child_run_id).unwrap_or_default();
            (
                RunStatus::Completed,
                Some(RunTerminalStopReason::Completed),
                TaskDelegationStatus::Completed,
                summarize_child_result(&final_text),
            )
        }
        ChildRunOutcome::Failed { error } => {
            let final_text = latest_assistant_text_for_run(state, child_run_id).unwrap_or_default();
            let message = if final_text.trim().is_empty() {
                format!("Subagent failed: {error}")
            } else {
                format!(
                    "Subagent failed after replying: {}",
                    summarize_child_result(&final_text)
                )
            };
            (
                RunStatus::Failed,
                Some(RunTerminalStopReason::Failed),
                TaskDelegationStatus::Failed,
                message,
            )
        }
        ChildRunOutcome::Cancelled => (
            RunStatus::Cancelled,
            Some(RunTerminalStopReason::Cancelled),
            TaskDelegationStatus::Cancelled,
            "Subagent cancelled by user.".to_string(),
        ),
        ChildRunOutcome::InterruptedByRestart => (
            RunStatus::Failed,
            Some(RunTerminalStopReason::Interrupted),
            TaskDelegationStatus::Failed,
            RESTART_INTERRUPTED_TASK_RESULT.to_string(),
        ),
    };

    state
        .session
        .upsert_run_with_stop_reason(child_run_id, child_status, child_stop_reason);
    commit_open_assistant_transcript_items_for_run(state, child_run_id);
    upsert_run_terminal_transcript_item(state, child_run_id);

    if let Some(invocation) = state
        .session
        .find_tool_invocation_mut(parent_tool_invocation_id)
    {
        invocation.set_task_delegation_status(delegation_status);
    }
    upsert_tool_invocation_transcript_item(state, parent_tool_invocation_id);
    upsert_delegated_child_transcript_item(state, parent_tool_invocation_id, None);

    state.set_foreground(parent_run_id, ForegroundPhase::Generating, None);

    let result_effects = crate::app::update::update(
        state,
        Msg::ToolExecutionFinished {
            run_id: parent_run_id,
            invocation_id: parent_tool_invocation_id,
            result: Ok(synthetic_result),
        },
    );

    info!(
        session_id = %session_id,
        parent_run_id = %parent_run_id,
        child_run_id = %child_run_id,
        invocation_id = %parent_tool_invocation_id,
        "child run reached terminal state and parent resumed"
    );

    Some(result_effects)
}

pub fn recover_interrupted_delegated_child(state: &mut AppState) -> Vec<Effect> {
    recover_interrupted_delegated_child_for_owner(state, None)
}

pub fn recover_interrupted_delegated_child_for_owner(
    state: &mut AppState,
    expected_child_run_id: Option<Uuid>,
) -> Vec<Effect> {
    let session_id = state.session.id;
    match find_interrupted_child_recovery_candidate(state) {
        Ok(Some(candidate)) => {
            if expected_child_run_id
                .is_some_and(|child_run_id| child_run_id != candidate.child_run_id)
            {
                return fail_closed_startup_recovery(
                    state,
                    format!(
                        "foreground owner expected interrupted child run {} but recovery found {}",
                        expected_child_run_id.expect("checked above"),
                        candidate.child_run_id
                    ),
                );
            }

            let Some(effects) = complete_child_run(
                state,
                candidate.child_run_id,
                ChildRunOutcome::InterruptedByRestart,
            ) else {
                return fail_closed_startup_recovery(
                    state,
                    format!(
                        "interrupted delegated child recovery lost lineage for child run {}",
                        candidate.child_run_id
                    ),
                );
            };

            info!(
                session_id = %session_id,
                parent_run_id = %candidate.parent_run_id,
                child_run_id = %candidate.child_run_id,
                invocation_id = %candidate.parent_tool_invocation_id,
                "recovered interrupted delegated child run during startup"
            );
            effects
        }
        Ok(None) => {
            if let Some(child_run_id) = expected_child_run_id {
                fail_closed_startup_recovery(
                    state,
                    format!(
                        "foreground owner expected interrupted child run {} but no recoverable delegated child was found",
                        child_run_id
                    ),
                )
            } else {
                Vec::new()
            }
        }
        Err(message) => fail_closed_startup_recovery(state, message),
    }
}

pub fn latest_assistant_text_for_run(state: &AppState, run_id: Uuid) -> Option<String> {
    state
        .session
        .turns
        .iter()
        .rev()
        .find(|turn| turn.run_id == run_id && matches!(turn.role, Role::Assistant))
        .map(|turn| turn.content.clone())
}

fn finish_task_invocation_with_error(
    state: &mut AppState,
    invocation_id: ToolInvocationId,
    error: &str,
) {
    if let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) {
        invocation.execution_state = ToolExecutionState::Failed;
        invocation.error = Some(error.to_string());
        invocation.completed_at = Some(Utc::now());
        invocation.set_task_delegation_status(TaskDelegationStatus::Failed);
    }
    upsert_tool_invocation_transcript_item(state, invocation_id);
    upsert_delegated_child_transcript_item(state, invocation_id, None);
    state.session.updated_at = Utc::now();
}

fn summarize_child_result(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "Subagent finished without a final text response.".to_string()
    } else {
        format!("Subagent finished: {trimmed}")
    }
}

fn find_interrupted_child_recovery_candidate(
    state: &AppState,
) -> Result<Option<InterruptedChildRecoveryCandidate>, String> {
    let mut candidates = Vec::new();

    for run in state
        .session
        .runs
        .iter()
        .filter(|run| run.status == RunStatus::InProgress)
    {
        match (run.parent_run_id, run.parent_tool_invocation_id) {
            (None, None) => continue,
            (Some(_), None) | (None, Some(_)) => {
                return Err(format!(
                    "interrupted delegated child recovery found child run {} with incomplete parent links",
                    run.id
                ));
            }
            (Some(parent_run_id), Some(parent_tool_invocation_id)) => {
                let Some(parent_run) = state.session.find_run(parent_run_id) else {
                    return Err(format!(
                        "interrupted delegated child recovery found missing parent run {} for child run {}",
                        parent_run_id, run.id
                    ));
                };
                if parent_run.status != RunStatus::InProgress {
                    return Err(format!(
                        "interrupted delegated child recovery expected parent run {} to remain in progress",
                        parent_run_id
                    ));
                }

                let Some(invocation) = state
                    .session
                    .tool_invocations
                    .iter()
                    .find(|invocation| invocation.id == parent_tool_invocation_id)
                else {
                    return Err(format!(
                        "interrupted delegated child recovery found child run {} without parent task invocation {}",
                        run.id, parent_tool_invocation_id
                    ));
                };

                validate_interrupted_task_invocation(invocation, parent_run_id, run.id)?;

                candidates.push(InterruptedChildRecoveryCandidate {
                    parent_run_id,
                    parent_tool_invocation_id,
                    child_run_id: run.id,
                });
            }
        }
    }

    for invocation in state.session.tool_invocations.iter().filter(|invocation| {
        invocation.tool_name == TASK_TOOL_NAME
            && (invocation.execution_state == ToolExecutionState::Running
                || invocation.delegation_status() == Some(TaskDelegationStatus::Running))
    }) {
        let Some(child_run_id) = invocation.child_run_id() else {
            return Err(format!(
                "interrupted delegated child recovery found running task invocation {} without a child run id",
                invocation.id
            ));
        };

        let Some(parent_tool_invocation_id) = state
            .session
            .find_run(child_run_id)
            .and_then(|run| run.parent_tool_invocation_id)
        else {
            return Err(format!(
                "interrupted delegated child recovery found running task invocation {} without a matching child run record {}",
                invocation.id, child_run_id
            ));
        };

        if parent_tool_invocation_id != invocation.id {
            return Err(format!(
                "interrupted delegated child recovery found mismatched parent invocation linkage for child run {}",
                child_run_id
            ));
        }
    }

    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.into_iter().next()),
        count => Err(format!(
            "interrupted delegated child recovery found {count} running delegated child runs and refused to guess"
        )),
    }
}

fn validate_interrupted_task_invocation(
    invocation: &crate::session::model::ToolInvocationRecord,
    parent_run_id: Uuid,
    child_run_id: Uuid,
) -> Result<(), String> {
    if invocation.tool_name != TASK_TOOL_NAME {
        return Err(format!(
            "interrupted delegated child recovery expected invocation {} to be the task tool",
            invocation.id
        ));
    }
    if invocation.run_id != parent_run_id {
        return Err(format!(
            "interrupted delegated child recovery found invocation {} linked to parent run {} instead of {}",
            invocation.id, invocation.run_id, parent_run_id
        ));
    }
    if invocation.approval_state != ToolApprovalState::Approved {
        return Err(format!(
            "interrupted delegated child recovery expected task invocation {} to be approved",
            invocation.id
        ));
    }
    if invocation.execution_state != ToolExecutionState::Running {
        return Err(format!(
            "interrupted delegated child recovery expected task invocation {} to still be running",
            invocation.id
        ));
    }
    if invocation.delegation_status() != Some(TaskDelegationStatus::Running) {
        return Err(format!(
            "interrupted delegated child recovery expected task invocation {} delegation to still be running",
            invocation.id
        ));
    }
    if invocation.child_run_id() != Some(child_run_id) {
        return Err(format!(
            "interrupted delegated child recovery found invocation {} linked to child run {:?} instead of {}",
            invocation.id,
            invocation.child_run_id(),
            child_run_id
        ));
    }

    Ok(())
}

fn fail_closed_startup_recovery(state: &mut AppState, message: String) -> Vec<Effect> {
    warn!(
        session_id = %state.session.id,
        error = %message,
        "startup delegated child recovery failed closed"
    );
    state.active_run_id = None;
    state.status = AppStatus::Error(message);
    Vec::new()
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

fn upsert_delegated_child_transcript_item(
    state: &mut AppState,
    invocation_id: ToolInvocationId,
    sequence_number_override: Option<u64>,
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

    if invocation.task_delegation().is_none() {
        return;
    }

    let item_id = transcript_delegated_child_item_id(invocation_id);
    let sequence_number = state
        .session
        .find_transcript_item(item_id)
        .map(|item| item.sequence_number)
        .or(sequence_number_override)
        .unwrap_or_else(|| state.session.allocate_replay_sequence());
    let item = TranscriptItemRecord::delegated_child(&invocation, sequence_number);
    state.session.upsert_transcript_item(item);
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

    for item_id in [
        transcript_assistant_reasoning_item_id(turn_id),
        transcript_assistant_text_item_id(turn_id),
    ] {
        if let Some(item) = state.session.find_transcript_item_mut(item_id) {
            item.stream_state = TranscriptStreamState::Committed;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::{RESTART_INTERRUPTED_TASK_RESULT, recover_interrupted_delegated_child};
    use crate::agent::TASK_TOOL_NAME;
    use crate::app::{AppState, AppStatus, Effect};
    use crate::session::model::{
        Role, RunRecord, RunStatus, Session, TaskDelegationRecord, TaskDelegationStatus,
        ToolApprovalState, ToolExecutionState, ToolInvocationRecord, ToolSource, Turn,
    };

    struct StartupRecoveryFixture {
        state: AppState,
        parent_run_id: Uuid,
        child_run_id: Uuid,
        preceding_turn_id: Uuid,
    }

    #[test]
    fn startup_recovery_marks_child_failed_and_resumes_parent_through_tool_execution_finished() {
        let mut fixture = interrupted_delegation_fixture();

        let effects = recover_interrupted_delegated_child(&mut fixture.state);

        assert_eq!(fixture.state.active_run_id, Some(fixture.parent_run_id));
        assert!(matches!(fixture.state.status, AppStatus::Generating));
        assert_eq!(
            fixture.state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Completed
        );
        assert_eq!(
            fixture.state.session.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert_eq!(
            fixture.state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Failed)
        );
        assert!(matches!(
            fixture
                .state
                .session
                .find_run(fixture.child_run_id)
                .map(|run| run.status),
            Some(RunStatus::Failed)
        ));
        assert!(matches!(
            effects.last(),
            Some(Effect::StartAssistant { run_id, request })
                if *run_id == fixture.parent_run_id
                    && request.messages.iter().any(|message| matches!(
                        message,
                        fluent_code_provider::ProviderMessage::ToolResult { content, .. }
                            if content == RESTART_INTERRUPTED_TASK_RESULT
                    ))
        ));
    }

    #[test]
    fn startup_recovery_is_noop_when_no_interrupted_delegated_child_exists() {
        let mut state = AppState::new(Session::new("no interrupted child"));

        let effects = recover_interrupted_delegated_child(&mut state);

        assert!(effects.is_empty());
        assert!(matches!(state.status, AppStatus::Idle));
        assert!(state.active_run_id.is_none());
    }

    #[test]
    fn startup_recovery_fails_closed_for_malformed_lineage() {
        let mut fixture = interrupted_delegation_fixture();
        fixture
            .state
            .session
            .runs
            .retain(|run| run.id != fixture.child_run_id);
        fixture.state.session.rebuild_run_indexes();

        let effects = recover_interrupted_delegated_child(&mut fixture.state);

        assert!(effects.is_empty());
        assert!(matches!(fixture.state.status, AppStatus::Error(_)));
        assert!(fixture.state.active_run_id.is_none());
        assert_eq!(
            fixture.state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Running
        );
        assert_eq!(
            fixture.state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Running)
        );
    }

    #[test]
    fn startup_recovery_fails_closed_for_ambiguous_lineage() {
        let mut fixture = interrupted_delegation_fixture();
        let second_child_run_id = Uuid::new_v4();
        let second_invocation_id = Uuid::new_v4();
        let second_invocation_sequence = fixture.state.session.allocate_replay_sequence();
        let second_run_sequence = fixture.state.session.allocate_replay_sequence();

        fixture.state.session.tool_invocations.push(ToolInvocationRecord {
            id: second_invocation_id,
            run_id: fixture.parent_run_id,
            tool_call_id: "task-call-2".to_string(),
            tool_name: TASK_TOOL_NAME.to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "agent": "explore", "prompt": "Inspect another file" }),
            preceding_turn_id: Some(fixture.preceding_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: Some(TaskDelegationRecord {
                child_run_id: Some(second_child_run_id),
                agent_name: Some("explore".to_string()),
                prompt: Some("Inspect another file".to_string()),
                status: TaskDelegationStatus::Running,
            }),
            sequence_number: second_invocation_sequence,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        fixture.state.session.runs.push(RunRecord {
            id: second_child_run_id,
            status: RunStatus::InProgress,
            parent_run_id: Some(fixture.parent_run_id),
            parent_tool_invocation_id: Some(second_invocation_id),
            created_sequence: second_run_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        fixture.state.session.rebuild_run_indexes();

        let effects = recover_interrupted_delegated_child(&mut fixture.state);

        assert!(effects.is_empty());
        assert!(matches!(fixture.state.status, AppStatus::Error(_)));
        assert!(fixture.state.active_run_id.is_none());
    }

    #[test]
    fn startup_recovery_preserves_batch_barrier_when_sibling_tool_is_still_nonterminal() {
        let mut fixture = interrupted_delegation_fixture();
        let sibling_invocation_sequence = fixture.state.session.allocate_replay_sequence();

        fixture
            .state
            .session
            .tool_invocations
            .push(ToolInvocationRecord {
                id: Uuid::new_v4(),
                run_id: fixture.parent_run_id,
                tool_call_id: "read-call-1".to_string(),
                tool_name: "read".to_string(),
                tool_source: ToolSource::BuiltIn,
                arguments: serde_json::json!({ "path": "Cargo.toml" }),
                preceding_turn_id: Some(fixture.preceding_turn_id),
                approval_state: ToolApprovalState::Approved,
                execution_state: ToolExecutionState::Running,
                result: None,
                error: None,
                delegation: None,
                sequence_number: sibling_invocation_sequence,
                requested_at: Utc::now(),
                approved_at: Some(Utc::now()),
                completed_at: None,
            });

        let effects = recover_interrupted_delegated_child(&mut fixture.state);

        assert_eq!(fixture.state.active_run_id, Some(fixture.parent_run_id));
        assert!(matches!(fixture.state.status, AppStatus::RunningTool));
        assert_eq!(
            fixture.state.session.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects.first(), Some(Effect::PersistSession)));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartAssistant { .. }))
        );
    }

    fn interrupted_delegation_fixture() -> StartupRecoveryFixture {
        let mut session = Session::new("startup recovery");
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let task_invocation_id = Uuid::new_v4();
        let user_turn_id = Uuid::new_v4();
        let preceding_turn_id = Uuid::new_v4();
        let parent_run_sequence = session.allocate_replay_sequence();
        let child_run_sequence = session.allocate_replay_sequence();
        let user_turn_sequence = session.allocate_replay_sequence();
        let assistant_turn_sequence = session.allocate_replay_sequence();
        let child_prompt_sequence = session.allocate_replay_sequence();
        let child_assistant_sequence = session.allocate_replay_sequence();
        let task_invocation_sequence = session.allocate_replay_sequence();

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
        session.turns.push(Turn {
            id: user_turn_id,
            run_id: parent_run_id,
            role: Role::User,
            content: "delegate work".to_string(),
            reasoning: String::new(),
            sequence_number: user_turn_sequence,
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: preceding_turn_id,
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "I will delegate that task.".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_turn_sequence,
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::User,
            content: "Inspect startup recovery".to_string(),
            reasoning: String::new(),
            sequence_number: child_prompt_sequence,
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::Assistant,
            content: "Partial child output that should not be summarized".to_string(),
            reasoning: String::new(),
            sequence_number: child_assistant_sequence,
            timestamp: Utc::now(),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: task_invocation_id,
            run_id: parent_run_id,
            tool_call_id: "task-call-1".to_string(),
            tool_name: TASK_TOOL_NAME.to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({
                "agent": "explore",
                "prompt": "Inspect startup recovery"
            }),
            preceding_turn_id: Some(preceding_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: Some(TaskDelegationRecord {
                child_run_id: Some(child_run_id),
                agent_name: Some("explore".to_string()),
                prompt: Some("Inspect startup recovery".to_string()),
                status: TaskDelegationStatus::Running,
            }),
            sequence_number: task_invocation_sequence,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        session.rebuild_run_indexes();

        StartupRecoveryFixture {
            state: AppState::new(session),
            parent_run_id,
            child_run_id,
            preceding_turn_id,
        }
    }
}
