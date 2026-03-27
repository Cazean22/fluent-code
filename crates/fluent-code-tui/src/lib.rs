pub mod conversation;
pub mod events;
pub mod markdown_render;
pub mod terminal;
pub mod theme;
pub mod ui_state;
pub mod view;

use fluent_code_app::agent::AgentRegistry;
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
    agent_registry: Arc<AgentRegistry>,
    tool_registry: Arc<ToolRegistry>,
    plugin_load_snapshot: PluginLoadSnapshot,
) -> Result<()> {
    info!(session_id = %session.id, "starting tui application");
    let mut terminal = terminal::init()?;
    let mut state = AppState::new_with_plugin_state(
        session,
        agent_registry,
        tool_registry,
        plugin_load_snapshot,
    );
    let mut ui_state = UiState::default();
    let (runtime_sender, mut runtime_receiver) = tokio::sync::mpsc::unbounded_channel();

    recover_startup_state(&mut state, &store, &runtime, runtime_sender.clone()).await?;

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

async fn recover_startup_state(
    state: &mut AppState,
    store: &FsSessionStore,
    runtime: &Runtime,
    runtime_sender: tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
) -> Result<()> {
    let effects = fluent_code_app::app::recover_startup_foreground(state);
    if effects.is_empty() {
        return Ok(());
    }

    apply_effects(
        state,
        store,
        runtime,
        runtime_sender,
        effects,
        &mut std::collections::VecDeque::new(),
    )
    .await
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

        // Drain all pending input events before the next render so that
        // accumulated mouse-wheel / arrow-key events are collapsed into a
        // single scroll adjustment instead of one-render-per-event.
        let mut scroll_delta: i16 = 0;

        while let Some(action) = events::next_action(&state.draft_input, &state.status)? {
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
                TuiAction::ScrollUp => scroll_delta = scroll_delta.saturating_sub(1),
                TuiAction::ScrollDown => scroll_delta = scroll_delta.saturating_add(1),
                TuiAction::PageUp => scroll_delta = scroll_delta.saturating_sub(10),
                TuiAction::PageDown => scroll_delta = scroll_delta.saturating_add(10),
                TuiAction::JumpTop => {
                    scroll_delta = 0;
                    ui_state.transcript_scroll_top = 0;
                    ui_state.transcript_follow_tail = false;
                }
                TuiAction::JumpBottom => {
                    scroll_delta = 0;
                    ui_state.reset_transcript_navigation();
                }
            }

            if state.should_quit {
                break;
            }
        }

        if scroll_delta != 0 {
            adjust_transcript_scroll(terminal, state, ui_state, scroll_delta)?;
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
        apply_effect(
            state,
            store,
            runtime,
            &runtime_sender,
            effect,
            "forwarding async effect from tui to runtime",
        )?;
    }

    while let Some(message) = pending_messages.pop_front() {
        let effects = update(state, message);
        for effect in effects {
            apply_effect(
                state,
                store,
                runtime,
                &runtime_sender,
                effect,
                "forwarding queued async effect from tui to runtime",
            )?;
        }
    }

    Ok(())
}

fn apply_effect(
    state: &mut AppState,
    store: &FsSessionStore,
    runtime: &Runtime,
    runtime_sender: &tokio::sync::mpsc::UnboundedSender<fluent_code_app::app::Msg>,
    effect: Effect,
    async_effect_log_context: &str,
) -> Result<()> {
    match effect {
        Effect::PersistSession => persist_session(state, store),
        Effect::PersistSessionIfDue => persist_session_if_due(state, store),
        Effect::StartAssistant { .. }
        | Effect::ExecuteTool { .. }
        | Effect::CancelAssistant { .. } => {
            log_tui_effect(async_effect_log_context, state, &effect);
            runtime.spawn_effect(effect, runtime_sender.clone());
            Ok(())
        }
    }
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
        fluent_code_app::app::Msg::ReplyToPendingTool(reply) => debug!(
            session_id = %state.session.id,
            message_kind = "reply_to_pending_tool",
            reply = ?reply,
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
        fluent_code_app::app::Msg::AssistantReasoningChunk { run_id, delta } => debug!(
            session_id = %state.session.id,
            message_kind = "assistant_reasoning_chunk",
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

    use chrono::Utc;
    use fluent_code_app::app::permissions::PermissionReply;
    use fluent_code_app::app::{AppState, AppStatus, Msg, RESTART_INTERRUPTED_TASK_RESULT};
    use fluent_code_app::runtime::Runtime;
    use fluent_code_app::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, Session,
        TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, Turn,
    };
    use fluent_code_app::session::store::{FsSessionStore, SessionStore};
    use fluent_code_provider::{MockProvider, ProviderClient};
    use tokio::sync::Mutex;

    use super::{drain_runtime_messages, handle_message, recover_startup_state};
    use crate::ui_state::UiState;

    struct StartupRecoveryFixture {
        session: Session,
        parent_run_id: uuid::Uuid,
        child_run_id: uuid::Uuid,
        preceding_turn_id: uuid::Uuid,
    }

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
            Msg::ReplyToPendingTool(PermissionReply::Once),
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
            std::sync::Arc::new(fluent_code_app::agent::AgentRegistry::built_in().clone()),
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
        assert!(ui_state.transcript_follow_tail);
        assert_eq!(ui_state.transcript_scroll_top, 0);
        assert_eq!(state.plugin_load_snapshot, plugin_snapshot);

        let latest = store
            .load_or_create_latest()
            .expect("load latest created session");
        assert_eq!(latest.id, state.session.id);

        cleanup(root);
    }

    #[tokio::test]
    async fn task_delegation_swaps_foreground_to_child_and_back_to_parent() {
        let _guard = test_lock().lock().await;
        let store = FsSessionStore::new(unique_test_dir());
        let session = Session::new("task foreground flow");
        let mut state = AppState::new_with_checkpoint_interval(session, Duration::from_millis(10));
        state.draft_input = "delegate".to_string();

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(5),
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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

        let parent_run_id = state.active_run_id.expect("parent run should be active");

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::AssistantToolCall {
                run_id: parent_run_id,
                tool_call: fluent_code_provider::ProviderToolCall {
                    id: "task-call-1".to_string(),
                    name: "task".to_string(),
                    arguments: serde_json::json!({
                        "agent": "explore",
                        "prompt": "Inspect session persistence"
                    }),
                },
            },
        )
        .await
        .expect("inject task tool call");

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::ReplyToPendingTool(PermissionReply::Once),
        )
        .await
        .expect("approve delegated task");

        let child_run_id = state
            .active_run_id
            .expect("child run should become foreground");
        assert_ne!(child_run_id, parent_run_id);

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::AssistantChunk {
                run_id: child_run_id,
                delta: "subagent answer".to_string(),
            },
        )
        .await
        .expect("child assistant chunk");

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::AssistantDone {
                run_id: child_run_id,
            },
        )
        .await
        .expect("complete child run");

        assert_eq!(state.active_run_id, Some(parent_run_id));
        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Generating
        ));
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some("Subagent finished: subagent answer")
        );

        let persisted = store
            .load(&state.session.id)
            .expect("load session after task delegation");
        assert_eq!(persisted.tool_invocations.len(), 1);
        assert_eq!(
            persisted.tool_invocations[0].child_run_id(),
            Some(child_run_id)
        );
        assert!(matches!(
            persisted
                .runs
                .iter()
                .find(|run| run.id == child_run_id)
                .map(|run| run.status),
            Some(fluent_code_app::session::model::RunStatus::Completed)
        ));

        drop(rx);
        cleanup(store_root(&store));
    }

    #[tokio::test]
    async fn cancelling_child_run_resumes_parent_with_cancelled_task_result() {
        let _guard = test_lock().lock().await;
        let store = FsSessionStore::new(unique_test_dir());
        let session = Session::new("task cancel flow");
        let mut state = AppState::new_with_checkpoint_interval(session, Duration::from_millis(10));
        state.draft_input = "delegate".to_string();

        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(5),
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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

        let parent_run_id = state.active_run_id.expect("parent run should be active");

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
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
        )
        .await
        .expect("inject task tool call");

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::ReplyToPendingTool(PermissionReply::Once),
        )
        .await
        .expect("approve delegated task");

        let child_run_id = state
            .active_run_id
            .expect("child run should become foreground");
        assert_ne!(child_run_id, parent_run_id);

        handle_message(
            &mut state,
            &store,
            &runtime,
            &mut ui_state,
            tx.clone(),
            Msg::CancelActiveRun,
        )
        .await
        .expect("cancel child run");

        assert_eq!(state.active_run_id, Some(parent_run_id));
        assert!(matches!(
            state.status,
            fluent_code_app::app::AppStatus::Generating
        ));
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some("Subagent cancelled by user.")
        );

        let persisted = store
            .load(&state.session.id)
            .expect("load session after child cancel");
        assert_eq!(persisted.tool_invocations.len(), 1);
        assert_eq!(
            persisted.tool_invocations[0].result.as_deref(),
            Some("Subagent cancelled by user.")
        );
        assert!(matches!(
            persisted
                .runs
                .iter()
                .find(|run| run.id == child_run_id)
                .map(|run| run.status),
            Some(fluent_code_app::session::model::RunStatus::Cancelled)
        ));

        drop(rx);
        cleanup(store_root(&store));
    }

    #[tokio::test]
    async fn startup_recovery_resumes_parent_and_persists_terminalized_child() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let fixture = interrupted_delegation_fixture();
        store
            .create(&fixture.session)
            .expect("persist startup fixture");

        let mut state = AppState::new(fixture.session);
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(5),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("recover interrupted delegated child on startup");

        assert_eq!(state.active_run_id, Some(fixture.parent_run_id));
        assert!(matches!(state.status, AppStatus::Generating));
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert_eq!(
            state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Failed)
        );
        assert!(matches!(
            state
                .session
                .find_run(fixture.child_run_id)
                .map(|run| run.status),
            Some(RunStatus::Failed)
        ));

        let persisted = store
            .load(&state.session.id)
            .expect("load recovered session");
        assert_eq!(
            persisted.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert!(matches!(
            persisted.runs.iter().find(|run| run.id == fixture.child_run_id),
            Some(run) if run.status == RunStatus::Failed
        ));

        let message = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("receive resumed parent runtime message")
            .expect("runtime message after startup recovery");
        assert!(matches!(message, Msg::AssistantChunk { .. }));

        cleanup(root);
    }

    #[tokio::test]
    async fn startup_recovery_restarts_root_generating_owner() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let session = root_generating_session();
        let run_id = session
            .foreground_owner
            .as_ref()
            .expect("foreground owner")
            .run_id;
        store
            .create(&session)
            .expect("persist root generating session");

        let mut state = AppState::new(session);
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            Duration::from_millis(5),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("restart root generating foreground owner");

        assert_eq!(state.active_run_id, Some(run_id));
        assert!(matches!(state.status, AppStatus::Generating));

        let message = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("receive resumed root runtime message")
            .expect("runtime message after root generating recovery");
        assert!(
            matches!(message, Msg::AssistantChunk { run_id: resumed_run_id, .. } if resumed_run_id == run_id)
        );

        cleanup(root);
    }

    #[tokio::test]
    async fn startup_recovery_is_noop_when_no_interrupted_child_exists() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let session = Session::new("startup no-op");
        store.create(&session).expect("persist clean session");

        let mut state = AppState::new(session);
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("startup recovery no-op");

        assert!(matches!(state.status, AppStatus::Idle));
        assert!(state.active_run_id.is_none());
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
            Ok(Some(_))
        ));

        cleanup(root);
    }

    #[tokio::test]
    async fn startup_recovery_fails_closed_for_malformed_lineage() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let mut fixture = interrupted_delegation_fixture();
        fixture
            .session
            .runs
            .retain(|run| run.id != fixture.child_run_id);
        store
            .create(&fixture.session)
            .expect("persist malformed startup fixture");

        let mut state = AppState::new(fixture.session.clone());
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("startup recovery fail-closed handling");

        assert!(matches!(state.status, AppStatus::Error(_)));
        assert!(state.active_run_id.is_none());
        assert_eq!(
            state.session.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Running)
        );
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
            Ok(Some(_))
        ));

        let persisted = store
            .load(&state.session.id)
            .expect("reload malformed session");
        assert_eq!(
            persisted.tool_invocations[0].delegation_status(),
            Some(TaskDelegationStatus::Running)
        );

        cleanup(root);
    }

    #[tokio::test]
    async fn startup_recovery_fails_closed_for_ambiguous_lineage() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let mut fixture = interrupted_delegation_fixture();
        let second_child_run_id = uuid::Uuid::new_v4();
        let second_invocation_id = uuid::Uuid::new_v4();

        fixture.session.tool_invocations.push(ToolInvocationRecord {
            id: second_invocation_id,
            run_id: fixture.parent_run_id,
            tool_call_id: "task-call-2".to_string(),
            tool_name: "task".to_string(),
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
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        fixture.session.runs.push(RunRecord {
            id: second_child_run_id,
            status: RunStatus::InProgress,
            parent_run_id: Some(fixture.parent_run_id),
            parent_tool_invocation_id: Some(second_invocation_id),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        store
            .create(&fixture.session)
            .expect("persist ambiguous startup fixture");

        let mut state = AppState::new(fixture.session);
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("startup recovery ambiguous handling");

        assert!(matches!(state.status, AppStatus::Error(_)));
        assert!(state.active_run_id.is_none());
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
            Ok(Some(_))
        ));

        cleanup(root);
    }

    #[tokio::test]
    async fn startup_recovery_preserves_batch_barrier_when_another_tool_is_still_running() {
        let _guard = test_lock().lock().await;
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let mut fixture = interrupted_delegation_fixture();
        fixture.session.tool_invocations.push(ToolInvocationRecord {
            id: uuid::Uuid::new_v4(),
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
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        store
            .create(&fixture.session)
            .expect("persist batch-barrier startup fixture");

        let mut state = AppState::new(fixture.session);
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        recover_startup_state(&mut state, &store, &runtime, tx)
            .await
            .expect("startup recovery batch barrier handling");

        assert_eq!(state.active_run_id, Some(fixture.parent_run_id));
        assert!(matches!(state.status, AppStatus::RunningTool));
        assert_eq!(
            state.session.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
            Ok(Some(_))
        ));

        let persisted = store
            .load(&state.session.id)
            .expect("reload batch-barrier session");
        assert_eq!(
            persisted.tool_invocations[0].result.as_deref(),
            Some(RESTART_INTERRUPTED_TASK_RESULT)
        );
        assert_eq!(
            persisted.tool_invocations[1].execution_state,
            ToolExecutionState::Running
        );

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

    fn interrupted_delegation_fixture() -> StartupRecoveryFixture {
        let mut session = Session::new("startup recovery");
        let parent_run_id = uuid::Uuid::new_v4();
        let child_run_id = uuid::Uuid::new_v4();
        let task_invocation_id = uuid::Uuid::new_v4();
        let user_turn_id = uuid::Uuid::new_v4();
        let preceding_turn_id = uuid::Uuid::new_v4();

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
            id: user_turn_id,
            run_id: parent_run_id,
            role: Role::User,
            content: "delegate work".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: preceding_turn_id,
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "I will delegate that task.".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: uuid::Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::User,
            content: "Inspect startup recovery".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.turns.push(Turn {
            id: uuid::Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::Assistant,
            content: "Partial child output that should not be summarized".to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: task_invocation_id,
            run_id: parent_run_id,
            tool_call_id: "task-call-1".to_string(),
            tool_name: "task".to_string(),
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
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });

        StartupRecoveryFixture {
            session,
            parent_run_id,
            child_run_id,
            preceding_turn_id,
        }
    }

    fn root_generating_session() -> Session {
        let mut session = Session::new("root generating startup");
        let run_id = uuid::Uuid::new_v4();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        session.turns.push(Turn {
            id: uuid::Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "resume the root run".to_string(),
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
}
