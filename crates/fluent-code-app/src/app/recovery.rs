use tracing::warn;

use crate::app::delegation::{
    recover_interrupted_delegated_child, recover_interrupted_delegated_child_for_owner,
};
use crate::app::request_builder::build_provider_request;
use crate::app::{AppState, Effect};
use crate::session::model::{
    ForegroundOwnerRecord, ForegroundPhase, RunStatus, ToolApprovalState, ToolExecutionState,
};

pub fn recover_startup_foreground(state: &mut AppState) -> Vec<Effect> {
    let Some(owner) = state.session.foreground_owner.clone() else {
        return recover_interrupted_delegated_child(state);
    };

    recover_from_foreground_owner(state, owner)
}

fn recover_from_foreground_owner(
    state: &mut AppState,
    owner: ForegroundOwnerRecord,
) -> Vec<Effect> {
    let Some(run) = state.session.find_run(owner.run_id) else {
        return fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery found missing run {} for persisted owner",
                owner.run_id
            ),
        );
    };

    if run.status != RunStatus::InProgress {
        return fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery expected run {} to remain in progress",
                owner.run_id
            ),
        );
    }

    let is_child_run = match (run.parent_run_id, run.parent_tool_invocation_id) {
        (None, None) => false,
        (Some(_), Some(_)) => true,
        _ => {
            return fail_closed_startup_recovery(
                state,
                format!(
                    "startup foreground recovery found run {} with incomplete parent linkage",
                    owner.run_id
                ),
            );
        }
    };

    match owner.phase {
        ForegroundPhase::Generating => {
            if is_child_run {
                recover_interrupted_delegated_child_for_owner(state, Some(owner.run_id))
            } else {
                recover_root_generating(state, owner)
            }
        }
        ForegroundPhase::AwaitingToolApproval => recover_awaiting_tool_approval(state, owner),
        ForegroundPhase::RunningTool => fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery refuses to guess how to resume running tools for run {}",
                owner.run_id
            ),
        ),
    }
}

fn recover_root_generating(state: &mut AppState, owner: ForegroundOwnerRecord) -> Vec<Effect> {
    if owner.batch_anchor_turn_id.is_some() {
        return fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery expected generating root run {} to have no batch anchor",
                owner.run_id
            ),
        );
    }

    if state.session.tool_invocations.iter().any(|invocation| {
        invocation.run_id == owner.run_id
            && (invocation.approval_state == ToolApprovalState::Pending
                || (invocation.approval_state == ToolApprovalState::Approved
                    && matches!(
                        invocation.execution_state,
                        ToolExecutionState::NotStarted | ToolExecutionState::Running
                    )))
    }) {
        return fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery found nonterminal tool work while root run {} was marked generating",
                owner.run_id
            ),
        );
    }

    state.set_foreground(owner.run_id, ForegroundPhase::Generating, None);
    let request = build_provider_request(state, owner.run_id);
    vec![Effect::StartAssistant {
        run_id: owner.run_id,
        request,
    }]
}

fn recover_awaiting_tool_approval(
    state: &mut AppState,
    owner: ForegroundOwnerRecord,
) -> Vec<Effect> {
    if state
        .session
        .pending_tool_invocation_for_batch(owner.run_id, owner.batch_anchor_turn_id)
        .is_none()
    {
        return fail_closed_startup_recovery(
            state,
            format!(
                "startup foreground recovery expected pending tool approvals for run {} and batch {:?}",
                owner.run_id, owner.batch_anchor_turn_id
            ),
        );
    }

    state.set_foreground(
        owner.run_id,
        ForegroundPhase::AwaitingToolApproval,
        owner.batch_anchor_turn_id,
    );
    Vec::new()
}

fn fail_closed_startup_recovery(state: &mut AppState, message: String) -> Vec<Effect> {
    warn!(
        session_id = %state.session.id,
        error = %message,
        "startup foreground recovery failed closed"
    );
    state.active_run_id = None;
    state.status = crate::app::AppStatus::Error(message);
    Vec::new()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::recover_startup_foreground;
    use crate::agent::TASK_TOOL_NAME;
    use crate::app::{AppState, AppStatus, Effect, RESTART_INTERRUPTED_TASK_RESULT};
    use crate::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, Session,
        TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, Turn,
    };

    #[test]
    fn startup_foreground_restarts_root_generating_run() {
        let mut state = AppState::new(root_generating_session());
        let run_id = state
            .session
            .foreground_owner
            .as_ref()
            .expect("owner")
            .run_id;

        let effects = recover_startup_foreground(&mut state);

        assert_eq!(state.active_run_id, Some(run_id));
        assert!(matches!(state.status, AppStatus::Generating));
        assert!(matches!(
            effects.as_slice(),
            [Effect::StartAssistant { run_id: resumed_run_id, request }]
                if *resumed_run_id == run_id
                    && request.messages.iter().any(|message| matches!(
                        message,
                        fluent_code_provider::ProviderMessage::UserText { text }
                            if text == "resume me"
                    ))
        ));
    }

    #[test]
    fn startup_foreground_restores_awaiting_tool_approval_without_runtime_effects() {
        let mut state = AppState::new(awaiting_tool_approval_session());
        let owner = state
            .session
            .foreground_owner
            .clone()
            .expect("owner present");

        let effects = recover_startup_foreground(&mut state);

        assert!(effects.is_empty());
        assert_eq!(state.active_run_id, Some(owner.run_id));
        assert!(matches!(state.status, AppStatus::AwaitingToolApproval));
    }

    #[test]
    fn startup_foreground_fails_closed_for_running_tool_owner() {
        let mut state = AppState::new(running_tool_session());

        let effects = recover_startup_foreground(&mut state);

        assert!(effects.is_empty());
        assert!(matches!(state.status, AppStatus::Error(_)));
        assert!(state.active_run_id.is_none());
        assert!(state.session.foreground_owner.is_some());
    }

    #[test]
    fn startup_foreground_falls_back_to_legacy_interrupted_child_recovery_when_owner_absent() {
        let mut state = AppState::new(interrupted_child_session(false));

        let effects = recover_startup_foreground(&mut state);

        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::PersistSession))
        );
    }

    fn root_generating_session() -> Session {
        let mut session = Session::new("root generating");
        let run_id = Uuid::new_v4();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "resume me".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::Generating,
            batch_anchor_turn_id: None,
        });
        session
    }

    fn awaiting_tool_approval_session() -> Session {
        let mut session = Session::new("awaiting approval");
        let run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "read the file".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "I'll use read".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "read-call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            delegation: None,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::AwaitingToolApproval,
            batch_anchor_turn_id: Some(assistant_turn_id),
        });
        session
    }

    fn running_tool_session() -> Session {
        let mut session = awaiting_tool_approval_session();
        let run_id = session.foreground_owner.as_ref().expect("owner").run_id;
        session.tool_invocations[0].approval_state = ToolApprovalState::Approved;
        session.tool_invocations[0].execution_state = ToolExecutionState::Running;
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::RunningTool,
            batch_anchor_turn_id: session.tool_invocations[0].preceding_turn_id,
        });
        session
    }

    fn interrupted_child_session(include_owner: bool) -> Session {
        let mut session = Session::new("interrupted child");
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let task_invocation_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        session.runs.push(RunRecord {
            id: parent_run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.runs.push(RunRecord {
            id: child_run_id,
            status: RunStatus::InProgress,
            parent_run_id: Some(parent_run_id),
            parent_tool_invocation_id: Some(task_invocation_id),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: parent_run_id,
            role: Role::User,
            content: "delegate".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "I will delegate".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: task_invocation_id,
            run_id: parent_run_id,
            tool_call_id: "task-call-1".to_string(),
            tool_name: TASK_TOOL_NAME.to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "agent": "explore", "prompt": "Inspect startup recovery" }),
            preceding_turn_id: Some(assistant_turn_id),
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
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        if include_owner {
            session.foreground_owner = Some(ForegroundOwnerRecord {
                run_id: child_run_id,
                phase: ForegroundPhase::Generating,
                batch_anchor_turn_id: None,
            });
        }
        session
    }
}
