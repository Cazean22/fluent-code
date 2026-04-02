use chrono::Utc;
use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{
    Role, RunStatus, Session, TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState,
    ToolExecutionState, ToolInvocationRecord, ToolSource, TranscriptItemRecord,
    TranscriptStreamState, Turn,
};
use serde_json::json;
use uuid::Uuid;

#[allow(dead_code)]
#[path = "../src/conversation.rs"]
mod conversation_regression;

use conversation_regression::{ConversationRow, RunMarkerKind, derive_history_cells};

#[test]
fn derive_history_cells_separates_committed_and_active_items() {
    let run_id = Uuid::new_v4();
    let mut session = Session::new("history cells active split");

    let first_user_turn = make_turn(run_id, Role::User, "first prompt", 1);
    let first_assistant_turn = make_turn(run_id, Role::Assistant, "first answer", 2);
    let follow_up_turn = make_turn(run_id, Role::User, "follow-up", 3);
    let active_assistant_turn = make_turn(run_id, Role::Assistant, "partial second answer", 4);

    session.turns.extend([
        first_user_turn.clone(),
        first_assistant_turn.clone(),
        follow_up_turn.clone(),
        active_assistant_turn.clone(),
    ]);
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&first_user_turn),
        TranscriptItemRecord::from_turn(&first_assistant_turn),
        TranscriptItemRecord::from_turn(&follow_up_turn),
        TranscriptItemRecord::assistant_text(
            run_id,
            active_assistant_turn.id,
            active_assistant_turn.sequence_number,
            active_assistant_turn.content.clone(),
            TranscriptStreamState::Open,
        ),
    ];

    let mut state = AppState::new(session);
    state.active_run_id = Some(run_id);
    state.status = AppStatus::Generating;

    let history_cells = derive_history_cells(&state);

    assert_eq!(history_cells.history.len(), 3);
    assert!(matches!(
        history_cells.history[0].rows.as_slice(),
        [ConversationRow::Turn(turn)] if turn.content == "first prompt"
    ));
    assert!(matches!(
        history_cells.history[1].rows.as_slice(),
        [ConversationRow::Turn(turn)] if turn.content == "first answer"
    ));
    assert!(matches!(
        history_cells.history[2].rows.as_slice(),
        [ConversationRow::Turn(turn)] if turn.content == "follow-up"
    ));

    let active_cell = history_cells.active.expect("active cell to exist");
    assert!(matches!(
        active_cell.rows.as_slice(),
        [ConversationRow::Turn(turn), ConversationRow::RunMarker(marker)]
            if turn.content == "partial second answer"
                && turn.is_streaming
                && marker.kind == RunMarkerKind::Running
    ));
}

#[test]
fn derive_history_cells_preserves_delegated_child_markers() {
    let parent_run_id = Uuid::new_v4();
    let child_run_id = Uuid::new_v4();
    let mut session = Session::new("history cells delegation");

    let parent_turn = make_turn(parent_run_id, Role::Assistant, "delegate then inspect", 1);
    session.turns.push(parent_turn.clone());
    session.upsert_run(parent_run_id, RunStatus::Completed);
    session.upsert_run_with_parent(
        child_run_id,
        RunStatus::Completed,
        Some(parent_run_id),
        None,
    );

    let mut task_invocation = make_tool_invocation(
        parent_run_id,
        Some(parent_turn.id),
        "task",
        json!({"agent": "explore", "prompt": "Inspect child flow"}),
        2,
    );
    task_invocation.approval_state = ToolApprovalState::Approved;
    task_invocation.execution_state = ToolExecutionState::Completed;
    task_invocation.delegation = Some(TaskDelegationRecord {
        child_run_id: Some(child_run_id),
        agent_name: Some("explore".to_string()),
        prompt: Some("Inspect child flow".to_string()),
        status: TaskDelegationStatus::Completed,
    });

    session.upsert_run_with_parent(
        child_run_id,
        RunStatus::Completed,
        Some(parent_run_id),
        Some(task_invocation.id),
    );

    let mut parent_read = make_tool_invocation(
        parent_run_id,
        Some(parent_turn.id),
        "read",
        json!({"path": "src/main.rs"}),
        4,
    );
    parent_read.approval_state = ToolApprovalState::Approved;
    parent_read.execution_state = ToolExecutionState::Completed;

    let mut child_read =
        make_tool_invocation(child_run_id, None, "read", json!({"path": "src/lib.rs"}), 5);
    child_read.approval_state = ToolApprovalState::Approved;
    child_read.execution_state = ToolExecutionState::Completed;

    session.tool_invocations = vec![
        task_invocation.clone(),
        parent_read.clone(),
        child_read.clone(),
    ];
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&parent_turn),
        TranscriptItemRecord::from_tool_invocation(&task_invocation),
        TranscriptItemRecord::delegated_child(&task_invocation, 3),
        TranscriptItemRecord::from_tool_invocation(&parent_read),
        TranscriptItemRecord::from_tool_invocation(&child_read),
    ];

    let state = AppState::new(session);
    let history_cells = derive_history_cells(&state);
    let rows = history_cells.iter_rows().collect::<Vec<_>>();

    let delegated_tool = rows.iter().find_map(|row| match row {
        ConversationRow::Tool(tool) if tool.tool_name == "task" => Some(tool.as_ref()),
        _ => None,
    });
    let delegated_tool = delegated_tool.expect("task row to exist");
    assert_eq!(delegated_tool.display_name, "task explore");
    assert_eq!(
        delegated_tool
            .delegated_task
            .as_ref()
            .and_then(|delegated_task| delegated_task.child_run_id),
        Some(child_run_id)
    );
    assert_eq!(
        delegated_tool
            .delegated_task
            .as_ref()
            .and_then(|delegated_task| delegated_task.agent_name.as_deref()),
        Some("explore")
    );

    let read_summaries = rows
        .iter()
        .filter_map(|row| match row {
            ConversationRow::Tool(tool) if tool.tool_name == "read" => Some(tool.summary.as_str()),
            ConversationRow::ToolGroup(_) => Some("grouped"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(read_summaries, vec!["read src/main.rs", "read src/lib.rs"]);
}

fn make_turn(run_id: Uuid, role: Role, content: &str, sequence_number: u64) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        run_id,
        role,
        content: content.to_string(),
        reasoning: String::new(),
        sequence_number,
        timestamp: Utc::now(),
    }
}

fn make_tool_invocation(
    run_id: Uuid,
    preceding_turn_id: Option<Uuid>,
    tool_name: &str,
    arguments: serde_json::Value,
    sequence_number: u64,
) -> ToolInvocationRecord {
    ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: format!("call-{}", Uuid::new_v4()),
        tool_name: tool_name.to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments,
        preceding_turn_id,
        approval_state: ToolApprovalState::Pending,
        execution_state: ToolExecutionState::NotStarted,
        result: None,
        error: None,
        delegation: None,
        sequence_number,
        requested_at: Utc::now(),
        approved_at: None,
        completed_at: None,
    }
}
