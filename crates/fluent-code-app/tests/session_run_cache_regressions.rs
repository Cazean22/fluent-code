use chrono::Utc;
use fluent_code_app::session::model::{
    ReplaySequence, Role, RunRecord, RunStatus, Session, ToolApprovalState, ToolExecutionState,
    ToolInvocationRecord, ToolSource, TranscriptItemContent, TranscriptItemRecord,
    TranscriptStreamState,
};
use uuid::Uuid;

#[test]
fn rebuilds_run_indexes_after_normalize_and_preserves_first_match_semantics() {
    let mut session = Session::new("duplicate runs");
    let duplicated_run_id = Uuid::new_v4();
    let child_run_id = Uuid::new_v4();

    let first_duplicate_run =
        run_record(&mut session, duplicated_run_id, RunStatus::InProgress, None);
    let second_duplicate_run = run_record(
        &mut session,
        duplicated_run_id,
        RunStatus::Failed,
        Some(Uuid::new_v4()),
    );
    let child_run = run_record(
        &mut session,
        child_run_id,
        RunStatus::InProgress,
        Some(duplicated_run_id),
    );

    session.runs.push(first_duplicate_run);
    session.runs.push(second_duplicate_run);
    session.runs.push(child_run);

    assert!(session.find_run(duplicated_run_id).is_none());

    session.normalize_persistence();

    assert_eq!(
        session
            .find_run(duplicated_run_id)
            .expect("normalize rebuilds run index")
            .parent_run_id,
        None
    );
    session
        .find_run_mut(duplicated_run_id)
        .expect("find_run_mut uses rebuilt first-match index")
        .status = RunStatus::Completed;
    assert_eq!(session.runs[0].status, RunStatus::Completed);
    assert_eq!(session.runs[1].status, RunStatus::Failed);
    assert_eq!(session.root_run_id(child_run_id), Some(duplicated_run_id));
}

#[test]
fn root_run_id_returns_none_for_missing_and_cyclic_lineage() {
    let mut session = Session::new("broken ancestry");
    let root_run_id = Uuid::new_v4();
    let child_run_id = Uuid::new_v4();
    let missing_parent_child_run_id = Uuid::new_v4();
    let missing_parent_run_id = Uuid::new_v4();
    let self_cycle_run_id = Uuid::new_v4();
    let cycle_a_run_id = Uuid::new_v4();
    let cycle_b_run_id = Uuid::new_v4();

    let root_run = run_record(&mut session, root_run_id, RunStatus::InProgress, None);
    let child_run = run_record(
        &mut session,
        child_run_id,
        RunStatus::InProgress,
        Some(root_run_id),
    );
    let missing_parent_child_run = run_record(
        &mut session,
        missing_parent_child_run_id,
        RunStatus::InProgress,
        Some(missing_parent_run_id),
    );
    let self_cycle_run = run_record(
        &mut session,
        self_cycle_run_id,
        RunStatus::InProgress,
        Some(self_cycle_run_id),
    );
    let cycle_a_run = run_record(
        &mut session,
        cycle_a_run_id,
        RunStatus::InProgress,
        Some(cycle_b_run_id),
    );
    let cycle_b_run = run_record(
        &mut session,
        cycle_b_run_id,
        RunStatus::InProgress,
        Some(cycle_a_run_id),
    );

    session.runs.push(root_run);
    session.runs.push(child_run);
    session.runs.push(missing_parent_child_run);
    session.runs.push(self_cycle_run);
    session.runs.push(cycle_a_run);
    session.runs.push(cycle_b_run);

    session.rebuild_run_indexes();

    assert_eq!(session.root_run_id(root_run_id), Some(root_run_id));
    assert_eq!(session.root_run_id(child_run_id), Some(root_run_id));
    assert_eq!(session.root_run_id(missing_parent_child_run_id), None);
    assert_eq!(session.root_run_id(self_cycle_run_id), None);
    assert_eq!(session.root_run_id(cycle_a_run_id), None);
    assert_eq!(session.root_run_id(cycle_b_run_id), None);
}

#[test]
fn parent_change_rebuilds_descendant_root_cache() {
    let mut session = Session::new("parent changes");
    let first_root_run_id = Uuid::new_v4();
    let second_root_run_id = Uuid::new_v4();
    let child_run_id = Uuid::new_v4();
    let grandchild_run_id = Uuid::new_v4();

    session.upsert_run(first_root_run_id, RunStatus::InProgress);
    session.upsert_run(second_root_run_id, RunStatus::InProgress);
    session.upsert_run_with_parent(
        child_run_id,
        RunStatus::InProgress,
        Some(first_root_run_id),
        None,
    );
    session.upsert_run_with_parent(
        grandchild_run_id,
        RunStatus::InProgress,
        Some(child_run_id),
        None,
    );

    assert_eq!(
        session.root_run_id(grandchild_run_id),
        Some(first_root_run_id)
    );

    session.upsert_run_with_parent(
        child_run_id,
        RunStatus::InProgress,
        Some(second_root_run_id),
        None,
    );

    assert_eq!(session.root_run_id(child_run_id), Some(second_root_run_id));
    assert_eq!(
        session.root_run_id(grandchild_run_id),
        Some(second_root_run_id)
    );
}

#[test]
fn transcript_item_index_rebuilds_after_serde_round_trip() {
    let mut session = Session::new("serde transcript round trip");
    let run_id = Uuid::new_v4();
    let first_turn_id = Uuid::new_v4();
    let second_turn_id = Uuid::new_v4();

    let first_item = TranscriptItemRecord::assistant_text(
        run_id,
        first_turn_id,
        1,
        "first answer",
        TranscriptStreamState::Committed,
    );
    let second_item = TranscriptItemRecord::assistant_text(
        run_id,
        second_turn_id,
        3,
        "second answer",
        TranscriptStreamState::Committed,
    );
    let invocation = tool_invocation_record(run_id, 2, Some(first_turn_id));
    let tool_item = TranscriptItemRecord::from_tool_invocation(&invocation);

    session.upsert_transcript_item(second_item.clone());
    session.upsert_transcript_item(tool_item.clone());
    session.upsert_transcript_item(first_item.clone());
    session.tool_invocations.push(invocation.clone());

    let serialized = serde_json::to_string(&session).expect("serialize session");
    let mut round_tripped: Session =
        serde_json::from_str(&serialized).expect("deserialize session");

    assert_eq!(
        round_tripped
            .transcript_items
            .iter()
            .map(|item| item.sequence_number)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        transcript_turn_text(
            round_tripped
                .find_transcript_item(first_item.item_id)
                .expect("transcript lookup survives serde round trip"),
        ),
        "first answer"
    );
    assert_eq!(
        round_tripped
            .find_transcript_item(tool_item.item_id)
            .expect("tool transcript lookup survives serde round trip")
            .tool_invocation_id,
        Some(invocation.id)
    );

    let round_tripped_invocation = round_tripped
        .find_tool_invocation_mut(invocation.id)
        .expect("tool lookup survives serde round trip");
    round_tripped_invocation.execution_state = ToolExecutionState::Completed;
    round_tripped_invocation.result = Some("ok".to_string());

    assert_eq!(
        round_tripped.tool_invocations[0].execution_state,
        ToolExecutionState::Completed
    );
    assert_eq!(
        round_tripped.tool_invocations[0].result.as_deref(),
        Some("ok")
    );
}

#[test]
fn upsert_transcript_item_preserves_canonical_order_without_global_resort() {
    let mut session = Session::new("ordered transcript upsert");
    let run_id = Uuid::new_v4();
    let first_turn_id = Uuid::new_v4();
    let second_turn_id = Uuid::new_v4();
    let third_turn_id = Uuid::new_v4();

    let third_item = TranscriptItemRecord::assistant_text(
        run_id,
        third_turn_id,
        30,
        "third",
        TranscriptStreamState::Committed,
    );
    let first_item = TranscriptItemRecord::assistant_text(
        run_id,
        first_turn_id,
        10,
        "first",
        TranscriptStreamState::Committed,
    );
    let second_item = TranscriptItemRecord::assistant_text(
        run_id,
        second_turn_id,
        20,
        "second",
        TranscriptStreamState::Committed,
    );

    session.upsert_transcript_item(third_item.clone());
    session.upsert_transcript_item(first_item.clone());
    session.upsert_transcript_item(second_item.clone());

    assert_eq!(
        transcript_turn_snapshot(&session),
        vec![
            (10, "first".to_string()),
            (20, "second".to_string()),
            (30, "third".to_string()),
        ]
    );

    session.upsert_transcript_item(TranscriptItemRecord::assistant_text(
        run_id,
        second_turn_id,
        20,
        "second updated",
        TranscriptStreamState::Committed,
    ));

    assert_eq!(
        transcript_turn_snapshot(&session),
        vec![
            (10, "first".to_string()),
            (20, "second updated".to_string()),
            (30, "third".to_string()),
        ]
    );
    assert_eq!(
        transcript_turn_text(
            session
                .find_transcript_item(second_item.item_id)
                .expect("replacement keeps transcript lookup stable"),
        ),
        "second updated"
    );
}

#[test]
fn tool_invocation_index_tracks_insert_and_replace_paths() {
    let mut session = Session::new("tool invocation lookups");
    let run_id = Uuid::new_v4();
    let first_invocation = tool_invocation_record(run_id, 1, None);
    let second_invocation = tool_invocation_record(run_id, 2, Some(Uuid::new_v4()));

    session.tool_invocations.push(first_invocation.clone());
    {
        let first = session
            .find_tool_invocation_mut(first_invocation.id)
            .expect("first inserted invocation remains reachable");
        first.approval_state = ToolApprovalState::Approved;
        first.execution_state = ToolExecutionState::Running;
    }

    session.tool_invocations.push(second_invocation.clone());
    {
        let second = session
            .find_tool_invocation_mut(second_invocation.id)
            .expect("second inserted invocation remains reachable");
        second.approval_state = ToolApprovalState::Approved;
        second.execution_state = ToolExecutionState::Completed;
        second.result = Some("second result".to_string());
    }
    {
        let first = session
            .find_tool_invocation_mut(first_invocation.id)
            .expect("first invocation remains reachable after later inserts");
        first.execution_state = ToolExecutionState::Failed;
        first.error = Some("first failed".to_string());
    }

    assert_eq!(
        session.tool_invocations[0].execution_state,
        ToolExecutionState::Failed
    );
    assert_eq!(
        session.tool_invocations[0].error.as_deref(),
        Some("first failed")
    );
    assert_eq!(
        session.tool_invocations[1].execution_state,
        ToolExecutionState::Completed
    );
    assert_eq!(
        session.tool_invocations[1].result.as_deref(),
        Some("second result")
    );
}

fn transcript_turn_snapshot(session: &Session) -> Vec<(ReplaySequence, String)> {
    session
        .transcript_items
        .iter()
        .map(|item| (item.sequence_number, transcript_turn_text(item).to_string()))
        .collect()
}

fn transcript_turn_text(item: &TranscriptItemRecord) -> &str {
    match &item.content {
        TranscriptItemContent::Turn(content) => match content.role {
            Role::Assistant => content.content.as_str(),
            other => panic!("expected assistant turn transcript item, found {other:?}"),
        },
        other => panic!("expected turn transcript item, found {other:?}"),
    }
}

fn tool_invocation_record(
    run_id: Uuid,
    sequence_number: ReplaySequence,
    preceding_turn_id: Option<Uuid>,
) -> ToolInvocationRecord {
    ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: format!("call-{sequence_number}"),
        tool_name: "read".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({
            "path": format!("file-{sequence_number}.txt"),
        }),
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

fn run_record(
    session: &mut Session,
    id: Uuid,
    status: RunStatus,
    parent_run_id: Option<Uuid>,
) -> RunRecord {
    let now = Utc::now();
    let created_sequence = session.allocate_replay_sequence();
    let terminal_sequence = status
        .is_terminal()
        .then(|| session.allocate_replay_sequence());

    RunRecord {
        id,
        status,
        parent_run_id,
        parent_tool_invocation_id: None,
        created_sequence,
        terminal_sequence,
        terminal_stop_reason: status.default_terminal_stop_reason(),
        created_at: now,
        updated_at: now,
    }
}
