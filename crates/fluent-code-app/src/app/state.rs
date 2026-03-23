use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent_code_provider::ProviderRequest;
use uuid::Uuid;

use crate::plugin::{PluginLoadSnapshot, ToolRegistry};
use crate::session::model::Session;

const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct AppState {
    pub session: Session,
    pub tool_registry: Arc<ToolRegistry>,
    pub plugin_load_snapshot: PluginLoadSnapshot,
    pub draft_input: String,
    pub status: AppStatus,
    pub should_quit: bool,
    pub active_run_id: Option<Uuid>,
    pub pending_resume_request: Option<ProviderRequest>,
    last_checkpoint_at: Option<Instant>,
    checkpoint_interval: Duration,
}

impl AppState {
    pub fn new(session: Session) -> Self {
        Self::new_with_tool_registry(session, Arc::new(ToolRegistry::built_in()))
    }

    pub fn new_with_tool_registry(session: Session, tool_registry: Arc<ToolRegistry>) -> Self {
        Self::new_with_plugin_state(session, tool_registry, PluginLoadSnapshot::default())
    }

    pub fn new_with_checkpoint_interval(session: Session, checkpoint_interval: Duration) -> Self {
        Self::new_with_dependencies(
            session,
            checkpoint_interval,
            Arc::new(ToolRegistry::built_in()),
            PluginLoadSnapshot::default(),
        )
    }

    pub fn new_with_plugin_state(
        session: Session,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Self {
        Self::new_with_dependencies(
            session,
            CHECKPOINT_INTERVAL,
            tool_registry,
            plugin_load_snapshot,
        )
    }

    pub fn new_with_dependencies(
        session: Session,
        checkpoint_interval: Duration,
        tool_registry: Arc<ToolRegistry>,
        plugin_load_snapshot: PluginLoadSnapshot,
    ) -> Self {
        Self {
            session,
            tool_registry,
            plugin_load_snapshot,
            draft_input: String::new(),
            status: AppStatus::Idle,
            should_quit: false,
            active_run_id: None,
            pending_resume_request: None,
            last_checkpoint_at: None,
            checkpoint_interval,
        }
    }

    pub fn should_checkpoint_now(&self) -> bool {
        match self.last_checkpoint_at {
            Some(last_checkpoint_at) => last_checkpoint_at.elapsed() >= self.checkpoint_interval,
            None => true,
        }
    }

    pub fn mark_checkpoint_saved(&mut self) {
        self.last_checkpoint_at = Some(Instant::now());
    }

    pub fn replace_session(&mut self, session: Session) {
        self.session = session;
        self.draft_input.clear();
        self.status = AppStatus::Idle;
        self.should_quit = false;
        self.active_run_id = None;
        self.pending_resume_request = None;
        self.last_checkpoint_at = None;
    }
}

#[derive(Debug, Clone, Default)]
pub enum AppStatus {
    #[default]
    Idle,
    Generating,
    AwaitingToolApproval,
    RunningTool,
    Error(String),
}
