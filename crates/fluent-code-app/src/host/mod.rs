use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc::{self, error::TryRecvError};
use tracing::{debug, info};

use crate::Result;
use crate::agent::AgentRegistry;
use crate::app::{AppState, Effect, Msg, recover_startup_foreground, update};
use crate::plugin::{PluginLoadSnapshot, ToolRegistry};
use crate::runtime::Runtime;
use crate::session::model::{Session, SessionId};
use crate::session::store::{FsSessionStore, SessionStore};

pub struct SharedAppHost {
    state: AppState,
    store: FsSessionStore,
    runtime: Runtime,
    runtime_sender: mpsc::UnboundedSender<Msg>,
    runtime_receiver: mpsc::UnboundedReceiver<Msg>,
    queued_runtime_messages: VecDeque<Msg>,
}

impl SharedAppHost {
    pub fn new(
        session: Session,
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Self {
        let (runtime_sender, runtime_receiver) = mpsc::unbounded_channel();

        Self {
            state: AppState::new_with_plugin_state(
                session,
                agent_registry,
                tool_registry,
                plugin_load_snapshot,
            ),
            store,
            runtime,
            runtime_sender,
            runtime_receiver,
            queued_runtime_messages: VecDeque::new(),
        }
    }

    pub fn load_or_create(
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Result<Self> {
        let session = store.load_or_create_latest()?;
        Ok(Self::new(
            session,
            store,
            runtime,
            agent_registry,
            tool_registry,
            plugin_load_snapshot,
        ))
    }

    pub fn load(
        session_id: &SessionId,
        store: FsSessionStore,
        runtime: Runtime,
        agent_registry: Arc<AgentRegistry>,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Result<Self> {
        let session = store.load(session_id)?;
        Ok(Self::new(
            session,
            store,
            runtime,
            agent_registry,
            tool_registry,
            plugin_load_snapshot,
        ))
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn runtime_sender(&self) -> mpsc::UnboundedSender<Msg> {
        self.runtime_sender.clone()
    }

    pub fn close_runtime_activity_channel(&mut self) {
        self.runtime_receiver.close();
    }

    pub async fn recover_startup(&mut self) -> Result<()> {
        let effects = recover_startup_foreground(&mut self.state);
        if effects.is_empty() {
            return Ok(());
        }

        self.apply_effects(effects, &mut VecDeque::new()).await
    }

    pub async fn submit_prompt(&mut self, prompt: impl Into<String>) -> Result<()> {
        self.handle_message(Msg::InputChanged(prompt.into()))
            .await?;
        self.handle_message(Msg::SubmitPrompt).await
    }

    pub async fn cancel_active_run(&mut self) -> Result<()> {
        self.handle_message(Msg::CancelActiveRun).await
    }

    pub async fn handle_message(&mut self, msg: Msg) -> Result<()> {
        if matches!(msg, Msg::NewSession) {
            self.create_and_swap_session()?;
            return Ok(());
        }

        debug!(
            session_id = %self.state.session.id,
            message = ?msg,
            "handling shared host message"
        );

        let mut pending_messages = VecDeque::from([msg]);

        while let Some(message) = pending_messages.pop_front() {
            let effects = update(&mut self.state, message);
            self.apply_effects(effects, &mut pending_messages).await?;
        }

        Ok(())
    }

    pub async fn drain_runtime_messages(&mut self) -> Result<()> {
        while let Some(message) = self.next_runtime_message() {
            debug!(
                session_id = %self.state.session.id,
                message = ?message,
                "draining queued runtime message into shared host state"
            );
            self.handle_message(message).await?;
        }

        Ok(())
    }

    pub async fn wait_for_runtime_activity(&mut self) -> bool {
        if self.queue_runtime_message_if_available() {
            return true;
        }

        match self.runtime_receiver.recv().await {
            Some(message) => {
                debug!(
                    session_id = %self.state.session.id,
                    message = ?message,
                    "queued runtime message after runtime activity wake"
                );
                self.queued_runtime_messages.push_back(message);
                true
            }
            None => {
                debug!(
                    session_id = %self.state.session.id,
                    "runtime activity wait exited because the sender channel closed"
                );
                false
            }
        }
    }

    pub fn persist_now(&mut self) -> Result<()> {
        self.persist_session()
    }

    fn create_and_swap_session(&mut self) -> Result<()> {
        let session = self.store.create_new_session()?;
        info!(session_id = %session.id, "swapped shared host to new session");
        self.state.replace_session(session);
        Ok(())
    }

    fn next_runtime_message(&mut self) -> Option<Msg> {
        if let Some(message) = self.queued_runtime_messages.pop_front() {
            return Some(message);
        }

        self.runtime_receiver.try_recv().ok()
    }

    fn queue_runtime_message_if_available(&mut self) -> bool {
        if !self.queued_runtime_messages.is_empty() {
            return true;
        }

        match self.runtime_receiver.try_recv() {
            Ok(message) => {
                debug!(
                    session_id = %self.state.session.id,
                    message = ?message,
                    "queued already-available runtime message before waiting"
                );
                self.queued_runtime_messages.push_back(message);
                true
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
        }
    }

    async fn apply_effects(
        &mut self,
        effects: Vec<Effect>,
        pending_messages: &mut VecDeque<Msg>,
    ) -> Result<()> {
        for effect in effects {
            self.apply_effect(
                effect,
                "forwarding async effect from shared host to runtime",
            )?;
        }

        while let Some(message) = pending_messages.pop_front() {
            let effects = update(&mut self.state, message);
            for effect in effects {
                self.apply_effect(
                    effect,
                    "forwarding queued async effect from shared host to runtime",
                )?;
            }
        }

        Ok(())
    }

    fn apply_effect(&mut self, effect: Effect, async_effect_log_context: &str) -> Result<()> {
        match effect {
            Effect::PersistSession => self.persist_session(),
            Effect::PersistSessionIfDue => self.persist_session_if_due(),
            Effect::StartAssistant { .. }
            | Effect::ExecuteTool { .. }
            | Effect::CancelAssistant { .. } => {
                debug!(
                    session_id = %self.state.session.id,
                    effect = ?effect,
                    "{async_effect_log_context}"
                );
                self.runtime
                    .spawn_effect(effect, self.runtime_sender.clone());
                Ok(())
            }
        }
    }

    fn persist_session(&mut self) -> Result<()> {
        debug!(session_id = %self.state.session.id, "persisting session snapshot from shared host");
        self.store.save(&self.state.session)?;
        Ok(())
    }

    fn persist_session_if_due(&mut self) -> Result<()> {
        if self.state.should_checkpoint_now() {
            debug!(session_id = %self.state.session.id, "persisting due session checkpoint from shared host");
            self.store.save(&self.state.session)?;
            self.state.mark_checkpoint_saved();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use chrono::Utc;
    use fluent_code_provider::{MockProvider, ProviderClient, ProviderMessage};
    use uuid::Uuid;

    use super::SharedAppHost;
    use crate::agent::AgentRegistry;
    use crate::app::{AppState, AppStatus, Effect, Msg, recover_startup_foreground, update};
    use crate::plugin::{PluginLoadSnapshot, ToolRegistry};
    use crate::runtime::Runtime;
    use crate::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, RunTerminalStopReason,
        Session, ToolApprovalState, ToolExecutionState, ToolInvocationRecord, ToolSource, Turn,
    };
    use crate::session::store::{FsSessionStore, SessionStore};

    #[tokio::test]
    async fn shared_host_submit_prompt_matches_existing_update_semantics() {
        let root = unique_test_dir("shared-host-submit");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let session = Session::new("shared host submit semantics");
        let prompt = "shared host prompt";

        let mut expected_state = AppState::new_with_plugin_state(
            session.clone(),
            Arc::clone(&agent_registry),
            Arc::clone(&tool_registry),
            PluginLoadSnapshot::default(),
        );
        assert!(update(&mut expected_state, Msg::InputChanged(prompt.to_string())).is_empty());
        let expected_effects = update(&mut expected_state, Msg::SubmitPrompt);
        assert!(matches!(
            expected_effects.as_slice(),
            [Effect::PersistSession, Effect::StartAssistant { request, .. }]
                if request.messages.iter().any(|message| matches!(
                    message,
                    ProviderMessage::UserText { text } if text == prompt
                ))
        ));

        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::with_chunk_delay(Duration::from_millis(75))),
            Arc::clone(&tool_registry),
        );
        let mut host = SharedAppHost::new(
            session,
            store.clone(),
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.submit_prompt(prompt).await.expect("submit prompt");

        assert_eq!(host.state().draft_input, expected_state.draft_input);
        assert!(matches!(host.state().status, AppStatus::Generating));
        assert!(host.state().active_run_id.is_some());
        assert_eq!(
            host.state().session.turns.len(),
            expected_state.session.turns.len()
        );
        assert_eq!(host.state().session.turns[0].content, prompt);
        assert!(matches!(host.state().session.turns[0].role, Role::User));
        assert!(matches!(
            host.state().session.latest_run_status(),
            Some(RunStatus::InProgress)
        ));
        assert!(matches!(
            host.state()
                .session
                .foreground_owner
                .as_ref()
                .map(|owner| owner.phase),
            Some(ForegroundPhase::Generating)
        ));

        let persisted_after_submit = store
            .load(&host.state().session.id)
            .expect("persisted state after submit");
        assert_eq!(persisted_after_submit.turns.len(), 1);
        assert_eq!(persisted_after_submit.turns[0].content, prompt);
        assert!(matches!(
            persisted_after_submit.latest_run_status(),
            Some(RunStatus::InProgress)
        ));

        drain_host_until_idle(&mut host).await;

        let persisted_after_completion = store
            .load(&host.state().session.id)
            .expect("persisted state after completion");
        let assistant_turn = persisted_after_completion
            .turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn after completion");
        assert_eq!(
            assistant_turn.content,
            "Mock assistant response: shared host prompt"
        );

        cleanup(root);
    }

    #[tokio::test]
    async fn shared_host_cancel_active_run_preserves_existing_stop_reason() {
        let root = unique_test_dir("shared-host-cancel");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let session = running_tool_session();
        let run_id = session.runs[0].id;
        let invocation_id = session.tool_invocations[0].id;
        let batch_anchor_turn_id = session.tool_invocations[0].preceding_turn_id;
        let mut host = SharedAppHost::new(
            session,
            store.clone(),
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.state_mut()
            .set_foreground(run_id, ForegroundPhase::RunningTool, batch_anchor_turn_id);

        host.cancel_active_run().await.expect("cancel active run");

        assert!(host.state().active_run_id.is_none());
        assert!(matches!(host.state().status, AppStatus::Idle));
        assert_eq!(
            host.state()
                .session
                .find_run(run_id)
                .expect("cancelled run")
                .terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        host.runtime_sender()
            .send(Msg::ToolExecutionFinished {
                run_id,
                invocation_id,
                result: Ok("late tool result".to_string()),
            })
            .expect("queue stale tool result");
        host.drain_runtime_messages()
            .await
            .expect("drain stale tool result");

        let invocation = &host.state().session.tool_invocations[0];
        assert_eq!(invocation.result, None);
        assert_eq!(invocation.execution_state, ToolExecutionState::Running);
        assert_eq!(
            host.state()
                .session
                .find_run(run_id)
                .expect("cancelled run")
                .terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        let persisted = store
            .load(&host.state().session.id)
            .expect("persisted cancelled session");
        assert_eq!(
            persisted
                .find_run(run_id)
                .expect("persisted cancelled run")
                .terminal_stop_reason,
            Some(RunTerminalStopReason::Cancelled)
        );

        cleanup(root);
    }

    #[tokio::test]
    async fn shared_host_recover_startup_matches_existing_recovery_effects() {
        let root = unique_test_dir("shared-host-recovery");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let session = root_generating_session();
        store.create(&session).expect("persist recovery session");

        let mut expected_state = AppState::new_with_plugin_state(
            session.clone(),
            Arc::clone(&agent_registry),
            Arc::clone(&tool_registry),
            PluginLoadSnapshot::default(),
        );
        let expected_effects = recover_startup_foreground(&mut expected_state);
        let expected_run_id = expected_state.active_run_id.expect("expected active run");
        assert!(matches!(
            expected_effects.as_slice(),
            [Effect::StartAssistant { run_id, request }]
                if *run_id == expected_run_id
                    && request.messages.iter().any(|message| matches!(
                        message,
                        ProviderMessage::UserText { text } if text == "resume me"
                    ))
        ));

        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::with_chunk_delay(Duration::from_millis(75))),
            Arc::clone(&tool_registry),
        );
        let mut host = SharedAppHost::new(
            session,
            store.clone(),
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.recover_startup().await.expect("recover startup");

        assert_eq!(host.state().active_run_id, Some(expected_run_id));
        assert!(matches!(host.state().status, AppStatus::Generating));
        assert!(matches!(
            host.state()
                .session
                .foreground_owner
                .as_ref()
                .map(|owner| owner.phase),
            Some(ForegroundPhase::Generating)
        ));
        assert_eq!(
            host.state().session.turns.len(),
            expected_state.session.turns.len()
        );
        assert_eq!(host.state().session.turns[0].content, "resume me");

        drain_host_until_idle(&mut host).await;

        let persisted = store
            .load(&host.state().session.id)
            .expect("persisted recovered session");
        let assistant_turn = persisted
            .turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.role, Role::Assistant))
            .expect("assistant turn after recovery");
        assert_eq!(assistant_turn.content, "Mock assistant response: resume me");

        cleanup(root);
    }

    #[tokio::test]
    async fn shared_host_wait_for_runtime_activity_returns_immediately_for_queued_messages() {
        let root = unique_test_dir("shared-host-wait-queued");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let mut host = SharedAppHost::new(
            Session::new("shared host queued wait"),
            store,
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.runtime_sender()
            .send(Msg::InputChanged("queued runtime message".to_string()))
            .expect("queue runtime message");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), host.wait_for_runtime_activity())
                .await
                .expect("queued wait should complete immediately")
        );
        assert_eq!(host.state().draft_input, "");

        host.drain_runtime_messages()
            .await
            .expect("drain queued runtime message");

        assert_eq!(host.state().draft_input, "queued runtime message");

        cleanup(root);
    }

    #[tokio::test]
    async fn shared_host_wait_for_runtime_activity_wakes_when_runtime_message_arrives() {
        let root = unique_test_dir("shared-host-wait-idle");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let mut host = SharedAppHost::new(
            Session::new("shared host idle wait"),
            store,
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(20), host.wait_for_runtime_activity())
                .await
                .is_err()
        );

        let runtime_sender = host.runtime_sender();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            runtime_sender
                .send(Msg::InputChanged("wake after idle wait".to_string()))
                .expect("send wake message");
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(200), host.wait_for_runtime_activity())
                .await
                .expect("idle wait should wake after runtime message")
        );
        assert_eq!(host.state().draft_input, "");

        host.drain_runtime_messages()
            .await
            .expect("drain wake message");

        assert_eq!(host.state().draft_input, "wake after idle wait");

        cleanup(root);
    }

    #[tokio::test]
    async fn shared_host_wait_for_runtime_activity_exits_when_runtime_channel_closes() {
        let root = unique_test_dir("shared-host-wait-close");
        let store = FsSessionStore::new(root.clone());
        let agent_registry = Arc::new(AgentRegistry::built_in().clone());
        let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
        let runtime = Runtime::new_with_tool_registry(
            ProviderClient::Mock(MockProvider::new(None)),
            Arc::clone(&tool_registry),
        );
        let mut host = SharedAppHost::new(
            Session::new("shared host closed wait"),
            store,
            runtime,
            agent_registry,
            tool_registry,
            PluginLoadSnapshot::default(),
        );

        host.close_runtime_activity_channel();

        assert!(
            !tokio::time::timeout(Duration::from_millis(50), host.wait_for_runtime_activity())
                .await
                .expect("closed runtime channel should exit promptly")
        );

        cleanup(root);
    }

    async fn drain_host_until_idle(host: &mut SharedAppHost) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            host.drain_runtime_messages()
                .await
                .expect("drain runtime messages");

            if matches!(host.state().status, AppStatus::Idle)
                && host.state().active_run_id.is_none()
            {
                return;
            }
        }

        panic!("shared host did not reach idle before deadline");
    }

    fn root_generating_session() -> Session {
        let mut session = Session::new("shared host recovery");
        let run_id = Uuid::new_v4();
        session.upsert_run(run_id, RunStatus::InProgress);
        let sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "resume me".to_string(),
            reasoning: String::new(),
            sequence_number,
            timestamp: Utc::now(),
        });
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::Generating,
            batch_anchor_turn_id: None,
        });
        session
    }

    fn running_tool_session() -> Session {
        let mut session = Session::new("shared host running tool");
        let run_id = Uuid::new_v4();
        let assistant_turn_id = Uuid::new_v4();
        let run_created_sequence = session.allocate_replay_sequence();
        session.runs.push(RunRecord {
            id: run_id,
            status: RunStatus::InProgress,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_sequence: run_created_sequence,
            terminal_sequence: None,
            terminal_stop_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        let user_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::User,
            content: "read the file".to_string(),
            reasoning: String::new(),
            sequence_number: user_sequence_number,
            timestamp: Utc::now(),
        });
        let assistant_sequence_number = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "I will read the file".to_string(),
            reasoning: String::new(),
            sequence_number: assistant_sequence_number,
            timestamp: Utc::now(),
        });
        let invocation_sequence_number = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "read-call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: None,
            sequence_number: invocation_sequence_number,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        session.rebuild_run_indexes();
        session
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("fluent-code-app-{label}-{timestamp}"))
    }

    fn cleanup(path: PathBuf) {
        let _ = fs::remove_dir_all(path);
    }
}
