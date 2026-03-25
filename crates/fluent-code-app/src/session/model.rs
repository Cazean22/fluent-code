use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::plugin::DiscoveryScope;

pub type SessionId = Uuid;
pub type TurnId = Uuid;
pub type RunId = Uuid;
pub type ToolInvocationId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub tool_invocations: Vec<ToolInvocationRecord>,
}

impl Session {
    pub fn new(title: impl Into<String>) -> Self {
        let now = Utc::now();

        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            runs: Vec::new(),
            turns: Vec::new(),
            tool_invocations: Vec::new(),
        }
    }

    pub fn upsert_run(&mut self, run_id: RunId, status: RunStatus) {
        if let Some(run) = self.runs.iter_mut().find(|run| run.id == run_id) {
            run.status = status;
            run.updated_at = Utc::now();
            return;
        }

        self.runs.push(RunRecord {
            id: run_id,
            status,
            parent_run_id: None,
            parent_tool_invocation_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
    }

    pub fn upsert_run_with_parent(
        &mut self,
        run_id: RunId,
        status: RunStatus,
        parent_run_id: Option<RunId>,
        parent_tool_invocation_id: Option<ToolInvocationId>,
    ) {
        if let Some(run) = self.runs.iter_mut().find(|run| run.id == run_id) {
            run.status = status;
            run.parent_run_id = parent_run_id;
            run.parent_tool_invocation_id = parent_tool_invocation_id;
            run.updated_at = Utc::now();
            return;
        }

        self.runs.push(RunRecord {
            id: run_id,
            status,
            parent_run_id,
            parent_tool_invocation_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
    }

    pub fn latest_run_status(&self) -> Option<RunStatus> {
        self.runs.last().map(|run| run.status)
    }

    pub fn find_run(&self, run_id: RunId) -> Option<&RunRecord> {
        self.runs.iter().find(|run| run.id == run_id)
    }

    pub fn find_run_mut(&mut self, run_id: RunId) -> Option<&mut RunRecord> {
        self.runs.iter_mut().find(|run| run.id == run_id)
    }

    pub fn pending_tool_invocation(&self) -> Option<&ToolInvocationRecord> {
        self.tool_invocations
            .iter()
            .rev()
            .find(|invocation| invocation.approval_state == ToolApprovalState::Pending)
    }

    pub fn pending_tool_invocation_mut(&mut self) -> Option<&mut ToolInvocationRecord> {
        self.tool_invocations
            .iter_mut()
            .rev()
            .find(|invocation| invocation.approval_state == ToolApprovalState::Pending)
    }

    pub fn find_tool_invocation_mut(
        &mut self,
        invocation_id: ToolInvocationId,
    ) -> Option<&mut ToolInvocationRecord> {
        self.tool_invocations
            .iter_mut()
            .find(|invocation| invocation.id == invocation_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub status: RunStatus,
    #[serde(default)]
    pub parent_run_id: Option<RunId>,
    #[serde(default)]
    pub parent_tool_invocation_id: Option<ToolInvocationId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocationRecord {
    pub id: ToolInvocationId,
    pub run_id: RunId,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_source: ToolSource,
    pub arguments: serde_json::Value,
    #[serde(default)]
    pub preceding_turn_id: Option<TurnId>,
    #[serde(default)]
    pub approval_state: ToolApprovalState,
    #[serde(default)]
    pub execution_state: ToolExecutionState,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub child_run_id: Option<RunId>,
    #[serde(default)]
    pub delegation_agent_name: Option<String>,
    #[serde(default)]
    pub delegation_prompt: Option<String>,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    #[default]
    BuiltIn,
    Plugin {
        plugin_id: String,
        plugin_name: String,
        plugin_version: String,
        scope: DiscoveryScope,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalState {
    #[default]
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionState {
    #[default]
    NotStarted,
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub run_id: RunId,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}
