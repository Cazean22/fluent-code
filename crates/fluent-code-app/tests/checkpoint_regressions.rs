use std::{thread, time::Duration};

use fluent_code_app::app::{AppState, AppStatus, Effect, Msg, update};
use fluent_code_app::session::model::{Role, RunStatus, Session};
use uuid::Uuid;

fn start_submitted_run(state: &mut AppState) -> Uuid {
    state.draft_input = "hello".to_string();
    let effects = update(state, Msg::SubmitPrompt);

    match effects.get(1) {
        Some(Effect::StartAssistant { run_id, .. }) => *run_id,
        _ => panic!("expected assistant start effect"),
    }
}

#[test]
fn long_stream_checkpointing_remains_throttled_while_chunks_continue() {
    let checkpoint_interval = Duration::from_millis(5);
    let mut state = AppState::new_with_checkpoint_interval(
        Session::new("long stream checkpoint flow"),
        checkpoint_interval,
    );
    let run_id = start_submitted_run(&mut state);

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
fn assistant_completion_clears_foreground_after_streaming_chunks() {
    let mut state = AppState::new_with_checkpoint_interval(
        Session::new("streaming completion flow"),
        Duration::from_secs(60),
    );
    let run_id = start_submitted_run(&mut state);

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
