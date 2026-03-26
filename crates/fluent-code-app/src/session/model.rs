use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
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
    pub permissions: SessionPermissionState,
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
            permissions: SessionPermissionState::default(),
            runs: Vec::new(),
            turns: Vec::new(),
            tool_invocations: Vec::new(),
        }
    }

    pub fn remember_tool_permission_rule(&mut self, rule: ToolPermissionRule) {
        self.permissions.rules.retain(|existing| {
            existing.subject.tool_name != rule.subject.tool_name
                || existing.subject.tool_scope != rule.subject.tool_scope
        });
        self.permissions.rules.push(rule);
    }

    pub fn remembered_tool_permission_action(
        &self,
        tool_name: &str,
        tool_source: &ToolSource,
    ) -> Option<ToolPermissionAction> {
        self.permissions
            .rules
            .iter()
            .rev()
            .find(|rule| rule.matches(tool_name, tool_source))
            .map(|rule| rule.action)
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

#[derive(Debug, Clone, Serialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<TaskDelegationRecord>,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl ToolInvocationRecord {
    pub fn task_delegation(&self) -> Option<&TaskDelegationRecord> {
        self.delegation.as_ref()
    }

    pub fn child_run_id(&self) -> Option<RunId> {
        self.delegation
            .as_ref()
            .and_then(|delegation| delegation.child_run_id)
    }

    pub fn delegation_agent_name(&self) -> Option<&str> {
        self.delegation
            .as_ref()
            .and_then(|delegation| delegation.agent_name.as_deref())
    }

    pub fn delegation_prompt(&self) -> Option<&str> {
        self.delegation
            .as_ref()
            .and_then(|delegation| delegation.prompt.as_deref())
    }

    pub fn set_task_delegation(
        &mut self,
        child_run_id: RunId,
        agent_name: impl Into<String>,
        prompt: impl Into<String>,
    ) {
        self.delegation = Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some(agent_name.into()),
            prompt: Some(prompt.into()),
        });
    }
}

impl<'de> Deserialize<'de> for ToolInvocationRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let compat = ToolInvocationRecordCompat::deserialize(deserializer)?;

        Ok(Self {
            id: compat.id,
            run_id: compat.run_id,
            tool_call_id: compat.tool_call_id,
            tool_name: compat.tool_name,
            tool_source: compat.tool_source,
            arguments: compat.arguments,
            preceding_turn_id: compat.preceding_turn_id,
            approval_state: compat.approval_state,
            execution_state: compat.execution_state,
            result: compat.result,
            error: compat.error,
            delegation: TaskDelegationRecord::from_compat(
                compat.delegation,
                compat.child_run_id,
                compat.delegation_agent_name,
                compat.delegation_prompt,
            ),
            requested_at: compat.requested_at,
            approved_at: compat.approved_at,
            completed_at: compat.completed_at,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskDelegationRecord {
    #[serde(default)]
    pub child_run_id: Option<RunId>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

impl TaskDelegationRecord {
    fn from_compat(
        delegation: Option<Self>,
        legacy_child_run_id: Option<RunId>,
        legacy_agent_name: Option<String>,
        legacy_prompt: Option<String>,
    ) -> Option<Self> {
        let delegation = match delegation {
            Some(delegation) => {
                delegation.with_legacy_fields(legacy_child_run_id, legacy_agent_name, legacy_prompt)
            }
            None => Self {
                child_run_id: legacy_child_run_id,
                agent_name: legacy_agent_name,
                prompt: legacy_prompt,
            },
        };

        delegation.into_option()
    }

    fn with_legacy_fields(
        mut self,
        legacy_child_run_id: Option<RunId>,
        legacy_agent_name: Option<String>,
        legacy_prompt: Option<String>,
    ) -> Self {
        self.child_run_id = self.child_run_id.or(legacy_child_run_id);
        self.agent_name = self.agent_name.or(legacy_agent_name);
        self.prompt = self.prompt.or(legacy_prompt);
        self
    }

    fn into_option(self) -> Option<Self> {
        if self.child_run_id.is_none() && self.agent_name.is_none() && self.prompt.is_none() {
            None
        } else {
            Some(self)
        }
    }
}

#[derive(Debug, Deserialize)]
struct ToolInvocationRecordCompat {
    id: ToolInvocationId,
    run_id: RunId,
    tool_call_id: String,
    tool_name: String,
    #[serde(default)]
    tool_source: ToolSource,
    arguments: serde_json::Value,
    #[serde(default)]
    preceding_turn_id: Option<TurnId>,
    #[serde(default)]
    approval_state: ToolApprovalState,
    #[serde(default)]
    execution_state: ToolExecutionState,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    delegation: Option<TaskDelegationRecord>,
    #[serde(default)]
    child_run_id: Option<RunId>,
    #[serde(default)]
    delegation_agent_name: Option<String>,
    #[serde(default)]
    delegation_prompt: Option<String>,
    requested_at: DateTime<Utc>,
    #[serde(default)]
    approved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPermissionState {
    #[serde(default)]
    pub rules: Vec<ToolPermissionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPermissionRule {
    pub subject: ToolPermissionSubject,
    pub action: ToolPermissionAction,
}

impl ToolPermissionRule {
    pub fn matches(&self, tool_name: &str, tool_source: &ToolSource) -> bool {
        self.subject.tool_name == tool_name && self.subject.matches(tool_source)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPermissionSubject {
    pub tool_name: String,
    pub tool_scope: ToolPermissionScope,
}

impl ToolPermissionSubject {
    pub fn from_tool(tool_name: impl Into<String>, tool_source: &ToolSource) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_scope: ToolPermissionScope::from_tool_source(tool_source),
        }
    }

    fn matches(&self, tool_source: &ToolSource) -> bool {
        self.tool_scope == ToolPermissionScope::from_tool_source(tool_source)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionScope {
    BuiltIn,
    Plugin { plugin_id: String },
}

impl ToolPermissionScope {
    fn from_tool_source(tool_source: &ToolSource) -> Self {
        match tool_source {
            ToolSource::BuiltIn => Self::BuiltIn,
            ToolSource::Plugin { plugin_id, .. } => Self::Plugin {
                plugin_id: plugin_id.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionAction {
    Allow,
    Ask,
    Deny,
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
