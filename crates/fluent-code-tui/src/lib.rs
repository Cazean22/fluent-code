pub mod conversation;
pub mod events;
pub mod markdown_render;
pub mod terminal;
pub mod theme;
pub mod ui_state;
pub mod view;

use fluent_code_app::app::{AppState, Effect, update};
use fluent_code_app::error::Result;
use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry};
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::Session;
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use ratatui::layout::Rect;
use std::sync::Arc;
use tracing::{debug, info};

use crate::events::TuiAction;
use crate::ui_state::UiState;

pub async fn run_app(
    session: Session,
    store: FsSessionStore,
    runtime: Runtime,
    tool_registry: Arc<ToolRegistry>,
    plugin_load_snapshot: PluginLoadSnapshot,
) -> Result<()> {
    info!(session_id = %session.id, "starting tui application");
    let mut terminal = terminal::init()?;
    let mut state = AppState::new_with_plugin_state(session, tool_registry, plugin_load_snapshot);
    let mut ui_state = UiState::default();
    let (runtime_sender, mut runtime_receiver) = tokio::sync::mpsc::unbounded_channel();

    let app_result = run_loop(
        &mut terminal,
        &mut state,
        &mut ui_state,
        &store,
        &runtime,
        runtime_sender,
        &mut runtime_receiver,
    )
    .await;
    let restore_result = terminal::restore(terminal);

    info!(session_id = %state.session.id, "tui application finished");
    app_result?;
    restore_result
}

async fn run_loop(
    terminal: &mut terminal::AppTerminal,
    state: &mut AppState,
    ui_state: &mut UiState,
    store: &FsSessionStore,
    runtime: &Runtime,
    runtime_sender: tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
    runtime_receiver: &mut tokio::sync::mpsc::UnboundedReceiver<fluent_code_app::app::Msg>,
) -> Result<()> {
    debug!(session_id = %state.session.id, "entered tui run loop");
    loop {
        drain_runtime_messages(
            state,
            store,
            runtime,
            ui_state,
            runtime_sender.clone(),
            runtime_receiver,
        )
        .await?;
        terminal.draw(|frame| view::render(frame, state, ui_state))?;

        if state.should_quit {
            break;
        }

        if let Some(action) = events::next_action(&state.draft_input, &state.status)? {
            match action {
                TuiAction::Message(msg) => {
                    handle_message(state, store, runtime, ui_state, runtime_sender.clone(), msg)
                        .await?;
                }
                TuiAction::ToggleToolDetails => {
                    ui_state.show_tool_details = !ui_state.show_tool_details;
                }
                TuiAction::ToggleHelpOverlay => {
                    ui_state.show_help_overlay = !ui_state.show_help_overlay;
                }
                TuiAction::ScrollUp => adjust_transcript_scroll(terminal, state, ui_state, -1)?,
                TuiAction::ScrollDown => adjust_transcript_scroll(terminal, state, ui_state, 1)?,
                TuiAction::PageUp => adjust_transcript_scroll(terminal, state, ui_state, -10)?,
                TuiAction::PageDown => adjust_transcript_scroll(terminal, state, ui_state, 10)?,
                TuiAction::JumpTop => {
                    ui_state.transcript_scroll_top = 0;
                    ui_state.transcript_follow_tail = false;
                }
                TuiAction::JumpBottom => ui_state.reset_transcript_navigation(),
            }
        }
    }

    Ok(())
}

async fn drain_runtime_messages(
    state: &mut AppState,
    store: &FsSessionStore,
    runtime: &Runtime,
    ui_state: &mut UiState,
    runtime_sender: tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
    runtime_receiver: &mut tokio::sync::mpsc::UnboundedReceiver<fluent_code_app::app::Msg>,
) -> Result<()> {
    while let Ok(message) = runtime_receiver.try_recv() {
        log_tui_message(
            "draining queued runtime message into tui state",
            state,
            &message,
        );
        handle_message(
            state,
            store,
            runtime,
            ui_state,
            runtime_sender.clone(),
            message,
        )
        .await?;
    }

    Ok(())
}

async fn handle_message(
    state: &mut AppState,
    store: &FsSessionStore,
    runtime: &Runtime,
    ui_state: &mut UiState,
    runtime_sender: tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
    msg: fluent_code_app::app::Msg,
) -> Result<()> {
    if matches!(msg, fluent_code_app::app::Msg::NewSession) {
        info!(session_id = %state.session.id, "creating new session from tui command");
        create_and_swap_session(state, ui_state, store)?;
        return Ok(());
    }

    log_tui_message("handling tui message", state, &msg);

    let mut pending_messages = std::collections::VecDeque::from([msg]);

    while let Some(message) = pending_messages.pop_front() {
        let effects = update(state, message);
        apply_effects(
            state,
            store,
            runtime,
            runtime_sender.clone(),
            effects,
            &mut pending_messages,
        )
        .await?;
    }

    Ok(())
}

fn create_and_swap_session(
    state: &mut AppState,
    ui_state: &mut UiState,
    store: &FsSessionStore,
) -> Result<()> {
    let session = store.create_new_session()?;
    info!(session_id = %session.id, "swapped tui state to new session");
    state.replace_session(session);
    ui_state.reset_transcript_navigation();
    Ok(())
}

fn adjust_transcript_scroll(
    terminal: &terminal::AppTerminal,
    state: &AppState,
    ui_state: &mut UiState,
    delta: i16,
) -> Result<()> {
    let area = terminal.size()?;
    let transcript_area = view::transcript_area(Rect::new(0, 0, area.width, area.height));
    let max_scroll = view::transcript_max_scroll(
        &view::conversation_lines(state, ui_state.show_tool_details),
        transcript_area.width,
        transcript_area.height,
    );

    let base = if ui_state.transcript_follow_tail {
        max_scroll
    } else {
        ui_state.transcript_scroll_top
    };

    let next = if delta.is_negative() {
        base.saturating_sub(delta.unsigned_abs())
    } else {
        base.saturating_add(delta as u16)
    }
    .min(max_scroll);

    ui_state.transcript_scroll_top = next;
    ui_state.transcript_follow_tail = next >= max_scroll;
    Ok(())
}

async fn apply_effects(
    state: &mut AppState,
    store: &FsSessionStore,
    runtime: &Runtime,
    runtime_sender: tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
    effects: Vec<Effect>,
    pending_messages: &mut std::collections::VecDeque<fluent_code_app::app::Msg>,
) -> Result<()> {
    for effect in effects {
        match effect {
            Effect::PersistSession => persist_session(state, store)?,
            Effect::PersistSessionIfDue => persist_session_if_due(state, store)?,
            Effect::StartAssistant { .. }
            | Effect::ExecuteTool { .. }
            | Effect::CancelAssistant { .. } => {
                log_tui_effect(
                    "forwarding async effect from tui to runtime",
                    state,
                    &effect,
                );
                runtime.spawn_effect(effect, runtime_sender.clone());
            }
        }
    }

    while let Some(message) = pending_messages.pop_front() {
        let effects = update(state, message);
        for effect in effects {
            match effect {
                Effect::PersistSession => persist_session(state, store)?,
                Effect::PersistSessionIfDue => persist_session_if_due(state, store)?,
                Effect::StartAssistant { .. }
                | Effect::ExecuteTool { .. }
                | Effect::CancelAssistant { .. } => {
                    log_tui_effect(
                        "forwarding queued async effect from tui to runtime",
                        state,
                        &effect,
                    );
                    runtime.spawn_effect(effect, runtime_sender.clone())
                }
            }
        }
    }

    Ok(())
}

fn persist_session(state: &mut AppState, store: &FsSessionStore) -> Result<()> {
    debug!(session_id = %state.session.id, "persisting session snapshot from tui");
    store.save(&state.session)?;
    Ok(())
}

fn persist_session_if_due(state: &mut AppState, store: &FsSessionStore) -> Result<()> {
    if state.should_checkpoint_now() {
        debug!(session_id = %state.session.id, "persisting due session checkpoint from tui");
        store.save(&state.session)?;
        state.mark_checkpoint_saved();
    }

    Ok(())
}

fn log_tui_message(context: &str, state: &AppState, message: &fluent_code_app::app::Msg) {
    match message {
        fluent_code_app::app::Msg::InputChanged(input) => debug!(
            session_id = %state.session.id,
            message_kind = "input_changed",
            input_bytes = input.len(),
            "{context}"
        ),
        fluent_code_app::app::Msg::SubmitPrompt => debug!(
            session_id = %state.session.id,
            message_kind = "submit_prompt",
            "{context}"
        ),
        fluent_code_app::app::Msg::NewSession => debug!(
            session_id = %state.session.id,
            message_kind = "new_session",
            "{context}"
        ),
        fluent_code_app::app::Msg::ApprovePendingTool => debug!(
            session_id = %state.session.id,
            message_kind = "approve_pending_tool",
            "{context}"
        ),
        fluent_code_app::app::Msg::DenyPendingTool => debug!(
            session_id = %state.session.id,
            message_kind = "deny_pending_tool",
            "{context}"
        ),
        fluent_code_app::app::Msg::CancelActiveRun => debug!(
            session_id = %state.session.id,
            message_kind = "cancel_active_run",
            active_run_id = ?state.active_run_id,
            "{context}"
        ),
        fluent_code_app::app::Msg::AssistantChunk { run_id, delta } => debug!(
            session_id = %state.session.id,
            message_kind = "assistant_chunk",
            run_id = %run_id,
            chunk_bytes = delta.len(),
            "{context}"
        ),
        fluent_code_app::app::Msg::AssistantToolCall { run_id, tool_call } => debug!(
            session_id = %state.session.id,
            message_kind = "assistant_tool_call",
            run_id = %run_id,
            tool_name = %tool_call.name,
            tool_call_id = %tool_call.id,
            "{context}"
        ),
        fluent_code_app::app::Msg::AssistantDone { run_id } => debug!(
            session_id = %state.session.id,
            message_kind = "assistant_done",
            run_id = %run_id,
            "{context}"
        ),
        fluent_code_app::app::Msg::AssistantFailed { run_id, error } => debug!(
            session_id = %state.session.id,
            message_kind = "assistant_failed",
            run_id = %run_id,
            error = %error,
            "{context}"
        ),
        fluent_code_app::app::Msg::ToolExecutionFinished {
            run_id,
            invocation_id,
            result,
        } => debug!(
            session_id = %state.session.id,
            message_kind = "tool_execution_finished",
            run_id = %run_id,
            invocation_id = %invocation_id,
            result_status = if result.is_ok() { "ok" } else { "error" },
            "{context}"
        ),
        fluent_code_app::app::Msg::Quit => debug!(
            session_id = %state.session.id,
            message_kind = "quit",
            "{context}"
        ),
    }
}

fn log_tui_effect(context: &str, state: &AppState, effect: &Effect) {
    match effect {
        Effect::PersistSession => debug!(
            session_id = %state.session.id,
            effect_kind = "persist_session",
            "{context}"
        ),
        Effect::PersistSessionIfDue => debug!(
            session_id = %state.session.id,
            effect_kind = "persist_session_if_due",
            "{context}"
        ),
        Effect::StartAssistant { run_id, request } => debug!(
            session_id = %state.session.id,
            effect_kind = "start_assistant",
            run_id = %run_id,
            request_message_count = request.messages.len(),
            request_tool_count = request.tools.len(),
            "{context}"
        ),
        Effect::ExecuteTool {
            run_id,
            invocation_id,
            tool_call,
        } => debug!(
            session_id = %state.session.id,
            effect_kind = "execute_tool",
            run_id = %run_id,
            invocation_id = %invocation_id,
            tool_name = %tool_call.name,
            tool_call_id = %tool_call.id,
            "{context}"
        ),
        Effect::CancelAssistant { run_id } => debug!(
            session_id = %state.session.id,
            effect_kind = "cancel_assistant",
            run_id = %run_id,
            "{context}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use fluent_code_app::app::{AppState, Msg};
    use fluent_code_app::runtime::Runtime;
    use fluent_code_app::session::model::{Role, Session};
    use fluent_code_app::session::store::{FsSessionStore, SessionStore};
    use fluent_code_provider::{MockProvider, ProviderClient};
    use tokio::sync::Mutex;

    use super::{drain_runtime_messages, handle_message};
    use crate::ui_state::UiState;

    #[tokio::test]
    async fn new_session_swaps_app_state_and_updates_latest_pointer() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let initial_session = store.create_new_session().expect("create initial session");
        let initial_session_id = initial_session.id;
        let mut state = AppState::new(initial_session);
        state.draft_input = "stale draft".to_string();
        state.status = fluent_code_app::app::AppStatus::Error("old error".to_string());

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ui_state = UiState::default();

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx,
            Msg::NewSession,
        )
        .await
        .expect("create new session from tui");

        assert_ne!(state.session.id, initial_session_id);
        assert_eq!(state.session.title, "New Session");
        assert!(state.session.turns.is_empty());
        assert!(state.session.runs.is_empty());
        assert!(state.session.tool_invocations.is_empty());
        assert!(state.draft_input.is_empty());
        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Idle
        ));
        assert!(state.active_run_id.is_none());
        assert!(state.pending_resume_request.is_none());
        assert!(ui_state.transcript_follow_tail);
        assert_eq!(ui_state.transcript_scroll_top, 0);

        let latest = store
            .load_or_create_latest()
            .expect("load latest created session");
        assert_eq!(latest.id, state.session.id);

        cleanup(root);
    }

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn persists_partial_assistant_content_before_completion() {
        let _guard = test_lock().lock().await;
        let store = FsSessionStore::new(unique_test_dir());
        let session = Session::new("checkpoint test");
        let mut state = AppState::new_with_checkpoint_interval(session, Duration::from_millis(10));
        state.draft_input = "checkpoint proof".to_string();

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(180),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ui_state = UiState::default();
        let expected_final = "Mock assistant response: checkpoint proof";

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::SubmitPrompt,
        )
        .await
        .expect("submit prompt");

        let start = tokio::time::Instant::now();
        let mut saw_partial = false;

        while start.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages");

            let persisted = store
                .load(&state.session.id)
                .expect("load persisted session");
            if let Some(assistant_turn) = persisted
                .turns
                .iter()
                .find(|turn| matches!(turn.role, Role::Assistant))
                && !assistant_turn.content.is_empty()
                && assistant_turn.content.len() < expected_final.len()
            {
                saw_partial = true;
                break;
            }
        }

        assert!(
            saw_partial,
            "expected a partial assistant checkpoint before completion"
        );

        let finish_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < finish_deadline {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages to completion");

            if !matches!(state.status, fluent_code_app::app::AppStatus::Generating) {
                break;
            }
        }

        let persisted = store
            .load(&state.session.id)
            .expect("load final persisted session");
        let final_assistant = persisted
            .turns
            .iter()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn to exist");

        assert_eq!(final_assistant.content, expected_final);

        cleanup(store_root(&store));
    }

    #[tokio::test]
    async fn cancel_stops_persisted_assistant_growth() {
        let _guard = test_lock().lock().await;
        let store = FsSessionStore::new(unique_test_dir());
        let session = Session::new("cancel test");
        let mut state = AppState::new_with_checkpoint_interval(session, Duration::from_millis(10));
        state.draft_input = "cancel proof".to_string();

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(120),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ui_state = UiState::default();

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::SubmitPrompt,
        )
        .await
        .expect("submit prompt");

        let start = tokio::time::Instant::now();
        let partial_before_cancel = loop {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "expected partial assistant turn before timeout"
            );

            tokio::time::sleep(Duration::from_millis(30)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages before cancel");

            let persisted = store
                .load(&state.session.id)
                .expect("load persisted session");
            if let Some(assistant_turn) = persisted
                .turns
                .iter()
                .find(|turn| matches!(turn.role, Role::Assistant))
                && !assistant_turn.content.is_empty()
            {
                break assistant_turn.content.clone();
            }
        };

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::CancelActiveRun,
        )
        .await
        .expect("cancel active run");

        let cancel_deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        while tokio::time::Instant::now() < cancel_deadline {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages after cancel");
        }

        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Idle
        ));
        assert!(state.active_run_id.is_none());

        let persisted = store
            .load(&state.session.id)
            .expect("load persisted session after cancel");
        let assistant_after_cancel = persisted
            .turns
            .iter()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn after cancel");

        assert_eq!(assistant_after_cancel.content, partial_before_cancel);

        cleanup(store_root(&store));
    }

    #[tokio::test]
    async fn approve_tool_executes_and_resumes_run() {
        let _guard = test_lock().lock().await;
        let store = FsSessionStore::new(unique_test_dir());
        let session = Session::new("tool approval test");
        let mut state = AppState::new_with_checkpoint_interval(session, Duration::from_millis(10));
        state.draft_input = "please use uppercase_text: hello tool".to_string();

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(10),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ui_state = UiState::default();

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::SubmitPrompt,
        )
        .await
        .expect("submit prompt");

        let approval_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < approval_deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages to approval");

            if matches!(
                state.status,
                fluent_code_app::app::AppStatus::AwaitingToolApproval
            ) {
                break;
            }
        }

        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::AwaitingToolApproval
        ));
        assert_eq!(state.session.tool_invocations.len(), 1);

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::ApprovePendingTool,
        )
        .await
        .expect("approve pending tool");

        let completion_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < completion_deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drain_runtime_messages(
                &mut state,
                &store,
                &runtime,
                &mut ui_state,
                tx.clone(),
                &mut rx,
            )
            .await
            .expect("drain runtime messages to completion");

            if matches!(state.status, fluent_code_app::app::AppStatus::Idle) {
                break;
            }
        }

        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Idle
        ));
        assert!(state.active_run_id.is_none());

        let persisted = store
            .load(&state.session.id)
            .expect("load persisted session after tool flow");
        assert_eq!(persisted.tool_invocations.len(), 1);
        assert_eq!(
            persisted.tool_invocations[0].result.as_deref(),
            Some("HELLO TOOL")
        );

        let final_assistant = persisted
            .turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("final assistant turn after tool resume");
        assert!(
            final_assistant.content.contains("HELLO TOOL"),
            "expected final assistant content to include tool result, got: {}",
            final_assistant.content
        );

        cleanup(store_root(&store));
    }

    #[tokio::test]
    async fn new_session_preserves_plugin_snapshot() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let initial_session = store.create_new_session().expect("create initial session");

        let plugin_snapshot = fluent_code_app::plugin::PluginLoadSnapshot {
            accepted_plugins: vec![fluent_code_app::plugin::LoadedPluginMetadata {
                name: "Docs Plugin".to_string(),
                id: "docs.plugin".to_string(),
                version: "0.2.0".to_string(),
                scope: fluent_code_app::plugin::DiscoveryScope::Global,
                description: Some("Indexes documentation.".to_string()),
                tool_names: vec!["docs_search".to_string()],
                tool_count: 1,
            }],
            warnings: vec!["plugin validation warning".to_string()],
        };

        let mut state = AppState::new_with_plugin_state(
            initial_session,
            std::sync::Arc::new(fluent_code_app::plugin::ToolRegistry::built_in()),
            plugin_snapshot.clone(),
        );
        state.draft_input = "stale draft".to_string();
        state.status = fluent_code_app::app::AppStatus::Error("old error".to_string());

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ui_state = UiState::default();

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx,
            Msg::NewSession,
        )
        .await
        .expect("create new session from tui");

        assert_eq!(state.session.title, "New Session");
        assert!(state.session.turns.is_empty());
        assert!(state.session.runs.is_empty());
        assert!(state.session.tool_invocations.is_empty());
        assert!(state.draft_input.is_empty());
        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Idle
        ));
        assert!(state.active_run_id.is_none());
        assert!(state.pending_resume_request.is_none());
        assert!(ui_state.transcript_follow_tail);
        assert_eq!(ui_state.transcript_scroll_top, 0);
        assert_eq!(state.plugin_load_snapshot, plugin_snapshot);

        let latest = store
            .load_or_create_latest()
            .expect("load latest created session");
        assert_eq!(latest.id, state.session.id);

        cleanup(root);
    }

    fn unique_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();

        std::env::temp_dir().join(format!("fluent-code-tui-test-{nanos}"))
    }

    fn store_root(store: &FsSessionStore) -> PathBuf {
        let debug = format!("{store:?}");
        let prefix = "FsSessionStore { root: \"";
        let suffix = "\" }";
        let path = debug
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .expect("debug store root format");
        PathBuf::from(path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_dir_all(path);
    }
}
