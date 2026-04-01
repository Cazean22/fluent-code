use chrono::Utc;
use fluent_code_app::session::model::{RunRecord, RunStatus, Session};
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
