use tracing::warn;

use crate::app::delegation::{
    recover_interrupted_delegated_child, recover_interrupted_delegated_child_for_owner,
};
use crate::app::request_builder::build_provider_request;
use crate::app::{AppState, Effect};
use crate::session::model::{
    ForegroundOwnerRecord, ForegroundPhase, RunStatus, RunTerminalStopReason, ToolApprovalState,
    ToolExecutionState, TranscriptItemContent, TranscriptItemRecord, TranscriptPermissionState,
    TranscriptStreamState, transcript_assistant_reasoning_item_id,
    transcript_assistant_text_item_id, transcript_delegated_child_item_id,
    transcript_permission_item_id,
};

const INTERRUPTED_RUNNING_TOOL_MESSAGE: &str =
    "Tool execution was interrupted during restart recovery.";

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
        ForegroundPhase::RunningTool => interrupt_running_tool_recovery(state, owner),
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
    restore_generating_transcript_items(state, owner.run_id);
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
    restore_awaiting_tool_approval_transcript_items(
        state,
        owner.run_id,
        owner.batch_anchor_turn_id,
    );
    Vec::new()
}

fn interrupt_running_tool_recovery(
    state: &mut AppState,
    owner: ForegroundOwnerRecord,
) -> Vec<Effect> {
    let interrupted_at = chrono::Utc::now();
    let message = format!(
        "startup foreground recovery refuses to guess how to resume running tools for run {}",
        owner.run_id
    );

    for invocation in state
        .session
        .tool_invocations
        .iter_mut()
        .filter(|invocation| {
            invocation.run_id == owner.run_id
                && invocation.preceding_turn_id == owner.batch_anchor_turn_id
                && invocation.approval_state == ToolApprovalState::Approved
                && matches!(
                    invocation.execution_state,
                    ToolExecutionState::NotStarted | ToolExecutionState::Running
                )
        })
    {
        invocation.execution_state = ToolExecutionState::Failed;
        invocation.error = Some(INTERRUPTED_RUNNING_TOOL_MESSAGE.to_string());
        invocation.completed_at = Some(interrupted_at);

        if let Some(delegation) = invocation.delegation.as_mut()
            && delegation.status == crate::session::model::TaskDelegationStatus::Running
        {
            delegation.status = crate::session::model::TaskDelegationStatus::Failed;
        }
    }

    let affected_invocation_ids = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.run_id == owner.run_id
                && invocation.preceding_turn_id == owner.batch_anchor_turn_id
                && invocation.approval_state == ToolApprovalState::Approved
                && matches!(invocation.execution_state, ToolExecutionState::Failed)
        })
        .map(|invocation| invocation.id)
        .collect::<Vec<_>>();

    for invocation_id in affected_invocation_ids {
        upsert_tool_invocation_transcript_item(state, invocation_id);
        upsert_delegated_child_transcript_item(state, invocation_id);
    }

    state.session.upsert_run_with_stop_reason(
        owner.run_id,
        RunStatus::Failed,
        Some(RunTerminalStopReason::Interrupted),
    );
    upsert_run_terminal_transcript_item(state, owner.run_id);
    append_interrupted_marker(
        state,
        owner.run_id,
        owner.batch_anchor_turn_id,
        message.clone(),
    );
    state.session.updated_at = interrupted_at;
    state.clear_foreground();
    state.status = crate::app::AppStatus::Error(message);

    vec![Effect::PersistSession]
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

fn restore_generating_transcript_items(state: &mut AppState, run_id: uuid::Uuid) {
    let Some(turn_id) = state
        .session
        .turns
        .iter()
        .rev()
        .find(|turn| {
            turn.run_id == run_id
                && matches!(turn.role, crate::session::model::Role::Assistant)
                && !state
                    .session
                    .tool_invocations
                    .iter()
                    .any(|invocation| invocation.preceding_turn_id == Some(turn.id))
        })
        .map(|turn| turn.id)
    else {
        return;
    };

    for item_id in [
        transcript_assistant_reasoning_item_id(turn_id),
        transcript_assistant_text_item_id(turn_id),
    ] {
        if let Some(item) = state.session.find_transcript_item_mut(item_id) {
            item.stream_state = TranscriptStreamState::Open;
        }
    }
}

fn restore_awaiting_tool_approval_transcript_items(
    state: &mut AppState,
    run_id: uuid::Uuid,
    batch_anchor_turn_id: Option<uuid::Uuid>,
) {
    let invocation_ids = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.run_id == run_id
                && invocation.preceding_turn_id == batch_anchor_turn_id
                && invocation.approval_state == ToolApprovalState::Pending
        })
        .map(|invocation| invocation.id)
        .collect::<Vec<_>>();

    for invocation_id in invocation_ids {
        if let Some(item) = state.session.find_transcript_item_mut(invocation_id) {
            item.stream_state = TranscriptStreamState::Open;
        }

        let permission_item_id = transcript_permission_item_id(invocation_id);
        if let Some(item) = state.session.find_transcript_item_mut(permission_item_id) {
            item.stream_state = TranscriptStreamState::Open;
            if let TranscriptItemContent::Permission(content) = &mut item.content {
                content.state = TranscriptPermissionState::Pending;
                content.decision = None;
            }
        }
    }
}

fn upsert_tool_invocation_transcript_item(
    state: &mut AppState,
    invocation_id: crate::session::model::ToolInvocationId,
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

    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::from_tool_invocation(&invocation));
}

fn upsert_delegated_child_transcript_item(
    state: &mut AppState,
    invocation_id: crate::session::model::ToolInvocationId,
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
        .unwrap_or_else(|| state.session.allocate_replay_sequence());
    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::delegated_child(
            &invocation,
            sequence_number,
        ));
}

fn upsert_run_terminal_transcript_item(state: &mut AppState, run_id: uuid::Uuid) {
    let Some(run) = state.session.find_run(run_id).cloned() else {
        return;
    };

    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::run_terminal(&run));
}

fn append_interrupted_marker(
    state: &mut AppState,
    run_id: uuid::Uuid,
    batch_anchor_turn_id: Option<uuid::Uuid>,
    detail: String,
) {
    let parent_tool_invocation_id = state
        .session
        .tool_invocations
        .iter()
        .find(|invocation| {
            invocation.run_id == run_id && invocation.preceding_turn_id == batch_anchor_turn_id
        })
        .map(|invocation| invocation.id);
    let sequence_number = state.session.allocate_replay_sequence();
    state
        .session
        .upsert_transcript_item(TranscriptItemRecord::marker(
            run_id,
            sequence_number,
            "interrupted",
            Some(detail),
            parent_tool_invocation_id,
            None,
        ));
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::recover_startup_foreground;
    use crate::agent::TASK_TOOL_NAME;
    use crate::app::{AppState, AppStatus, Effect, RESTART_INTERRUPTED_TASK_RESULT};
    use crate::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, RunTerminalStopReason,
        Session, TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, TranscriptItemContent, TranscriptItemRecord,
        TranscriptPermissionState, TranscriptRunLifecycleEvent, TranscriptStreamState, Turn,
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

        assert!(matches!(effects.as_slice(), [Effect::PersistSession]));
        assert!(matches!(state.status, AppStatus::Error(_)));
        assert!(state.active_run_id.is_none());
        assert!(state.session.foreground_owner.is_none());
        assert_eq!(
            state.session.tool_invocations[0].execution_state,
            ToolExecutionState::Failed
        );
        assert_eq!(
            state.session.tool_invocations[0].error.as_deref(),
            Some(super::INTERRUPTED_RUNNING_TOOL_MESSAGE)
        );
        let run = state
            .session
            .find_run(state.session.tool_invocations[0].run_id)
            .expect("run persisted");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(
            run.terminal_stop_reason,
            Some(RunTerminalStopReason::Interrupted)
        );
        assert!(run.terminal_sequence.is_some());
    }

    #[test]
    fn startup_foreground_restores_open_transcript_item_or_marks_it_interrupted() {
        let mut generating_state = AppState::new(root_generating_session());
        let generating_run_id = generating_state
            .session
            .foreground_owner
            .as_ref()
            .expect("generating owner")
            .run_id;

        let generating_effects = recover_startup_foreground(&mut generating_state);

        assert!(matches!(
            generating_effects.as_slice(),
            [Effect::StartAssistant { .. }]
        ));
        let generating_items = ordered_transcript_items(&generating_state.session.transcript_items);
        assert!(generating_items.iter().any(|item| {
            item.run_id == generating_run_id
                && item.stream_state == TranscriptStreamState::Open
                && matches!(
                    &item.content,
                    TranscriptItemContent::Turn(content)
                        if matches!(content.role, Role::Assistant)
                            && content.content == "partial answer"
                )
        }));

        let mut awaiting_state = AppState::new(awaiting_tool_approval_session());
        let awaiting_owner = awaiting_state
            .session
            .foreground_owner
            .clone()
            .expect("awaiting owner");

        let awaiting_effects = recover_startup_foreground(&mut awaiting_state);

        assert!(awaiting_effects.is_empty());
        let awaiting_items = ordered_transcript_items(&awaiting_state.session.transcript_items);
        assert!(awaiting_items.iter().any(|item| {
            item.run_id == awaiting_owner.run_id
                && item.stream_state == TranscriptStreamState::Open
                && matches!(
                    &item.content,
                    TranscriptItemContent::ToolInvocation(content)
                        if content.tool_name == "read"
                            && matches!(content.execution_state, ToolExecutionState::NotStarted)
                )
        }));
        assert!(awaiting_items.iter().any(|item| {
            item.run_id == awaiting_owner.run_id
                && item.stream_state == TranscriptStreamState::Open
                && matches!(
                    &item.content,
                    TranscriptItemContent::Permission(content)
                        if content.state == TranscriptPermissionState::Pending
                )
        }));

        let mut running_state = AppState::new(running_tool_session());
        let running_run_id = running_state
            .session
            .foreground_owner
            .as_ref()
            .expect("running owner")
            .run_id;

        let running_effects = recover_startup_foreground(&mut running_state);

        assert!(matches!(
            running_effects.as_slice(),
            [Effect::PersistSession]
        ));
        let running_items = ordered_transcript_items(&running_state.session.transcript_items);
        assert!(running_items.iter().any(|item| {
            item.run_id == running_run_id
                && matches!(
                    &item.content,
                    TranscriptItemContent::RunLifecycle(content)
                        if content.event == TranscriptRunLifecycleEvent::Terminal
                            && content.stop_reason == Some(RunTerminalStopReason::Interrupted)
                )
        }));
        let interrupted_marker = running_items
            .iter()
            .find(|item| {
                item.run_id == running_run_id
                    && matches!(
                        &item.content,
                        TranscriptItemContent::Marker(content) if content.label == "interrupted"
                    )
            })
            .expect("interrupted marker item");
        assert_eq!(
            interrupted_marker.stream_state,
            TranscriptStreamState::Committed
        );
        assert!(matches!(
            &interrupted_marker.content,
            TranscriptItemContent::Marker(content)
                if content.detail.as_deref().is_some_and(|detail| detail.contains("refuses to guess"))
        ));
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
        let run_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: run_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.rebuild_run_indexes();
        let turn_sequence = session.allocate_replay_sequence();
        let user_turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "resume me".to_string(),
            reasoning: String::new(),
            sequence_number: turn_sequence,
            timestamp: Utc::now(),
        };
        session.turns.push(user_turn.clone());
        session
            .transcript_items
            .push(TranscriptItemRecord::run_started(&session.runs[0]));
        session
            .transcript_items
            .push(TranscriptItemRecord::from_turn(&user_turn));
        let assistant_turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "partial answer".to_string(),
            reasoning: String::new(),
            sequence_number: session.allocate_replay_sequence(),
            timestamp: Utc::now(),
        };
        session.turns.push(assistant_turn.clone());
        session
            .transcript_items
            .push(TranscriptItemRecord::assistant_text(
                run_id,
                assistant_turn.id,
                assistant_turn.sequence_number,
                assistant_turn.content.clone(),
                TranscriptStreamState::Open,
            ));
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
        let run_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: run_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.rebuild_run_indexes();
        let user_turn_sequence = session.allocate_replay_sequence();
        let user_turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "read the file".to_string(),
            reasoning: String::new(),
            sequence_number: user_turn_sequence,
            timestamp: Utc::now(),
        };
        session.turns.push(user_turn.clone());
        let assistant_turn_sequence = session.allocate_replay_sequence();
        let assistant_turn = Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "I'll use read".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_turn_sequence,
            timestamp: Utc::now(),
        };
        session.turns.push(assistant_turn.clone());
        let invocation_sequence = session.allocate_replay_sequence();
        let invocation = ToolInvocationRecord {
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
            sequence_number: invocation_sequence,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        };
        session.tool_invocations.push(invocation.clone());
        session
            .transcript_items
            .push(TranscriptItemRecord::run_started(&session.runs[0]));
        session
            .transcript_items
            .push(TranscriptItemRecord::from_turn(&user_turn));
        session
            .transcript_items
            .push(TranscriptItemRecord::assistant_text(
                run_id,
                assistant_turn.id,
                assistant_turn.sequence_number,
                assistant_turn.content.clone(),
                TranscriptStreamState::Committed,
            ));
        session
            .transcript_items
            .push(TranscriptItemRecord::from_tool_invocation(&invocation));
        let permission_sequence = session.allocate_replay_sequence();
        session
            .transcript_items
            .push(TranscriptItemRecord::permission(
                &invocation,
                permission_sequence,
                TranscriptPermissionState::Pending,
                None,
            ));
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
        let invocation = session.tool_invocations[0].clone();
        session.upsert_transcript_item(TranscriptItemRecord::from_tool_invocation(&invocation));
        let permission_item_id =
            crate::session::model::transcript_permission_item_id(invocation.id);
        if let Some(item) = session.find_transcript_item_mut(permission_item_id) {
            item.stream_state = TranscriptStreamState::Committed;
            if let TranscriptItemContent::Permission(content) = &mut item.content {
                content.state = TranscriptPermissionState::Approved;
            }
        }
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
        let user_turn_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: parent_run_id,
            role: Role::User,
            content: "delegate".to_string(),
            reasoning: String::new(),
            sequence_number: user_turn_sequence,
            timestamp: Utc::now(),
        });
        let assistant_turn_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "I will delegate".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_turn_sequence,
            timestamp: Utc::now(),
        });
        let invocation_sequence = session.allocate_replay_sequence();
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
            sequence_number: invocation_sequence,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        session.rebuild_run_indexes();
        if include_owner {
            session.foreground_owner = Some(ForegroundOwnerRecord {
                run_id: child_run_id,
                phase: ForegroundPhase::Generating,
                batch_anchor_turn_id: None,
            });
        }
        session
    }

    fn ordered_transcript_items(items: &[TranscriptItemRecord]) -> Vec<&TranscriptItemRecord> {
        let mut ordered = items.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|item| item.sequence_number);
        ordered
    }
}
