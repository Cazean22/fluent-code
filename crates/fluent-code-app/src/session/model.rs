use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::plugin::DiscoveryScope;

pub type SessionId = Uuid;
pub type TurnId = Uuid;
pub type RunId = Uuid;
pub type ToolInvocationId = Uuid;
pub type ReplaySequence = u64;

const FIRST_REPLAY_SEQUENCE: ReplaySequence = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default = "default_next_replay_sequence")]
    pub next_replay_sequence: ReplaySequence,
    #[serde(default)]
    pub permissions: SessionPermissionState,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub tool_invocations: Vec<ToolInvocationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_owner: Option<ForegroundOwnerRecord>,
}

impl Session {
    pub fn new(title: impl Into<String>) -> Self {
        let now = Utc::now();

        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            next_replay_sequence: default_next_replay_sequence(),
            permissions: SessionPermissionState::default(),
            runs: Vec::new(),
            turns: Vec::new(),
            tool_invocations: Vec::new(),
            foreground_owner: None,
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
        self.upsert_run_with_parent_and_stop_reason(run_id, status, None, None, None);
    }

    pub fn upsert_run_with_stop_reason(
        &mut self,
        run_id: RunId,
        status: RunStatus,
        stop_reason: Option<RunTerminalStopReason>,
    ) {
        self.upsert_run_with_parent_and_stop_reason(run_id, status, None, None, stop_reason);
    }

    fn upsert_run_with_parent_and_stop_reason(
        &mut self,
        run_id: RunId,
        status: RunStatus,
        parent_run_id: Option<RunId>,
        parent_tool_invocation_id: Option<ToolInvocationId>,
        stop_reason: Option<RunTerminalStopReason>,
    ) {
        let resolved_stop_reason = stop_reason.or_else(|| status.default_terminal_stop_reason());
        let now = Utc::now();

        if let Some(run_index) = self.runs.iter().position(|run| run.id == run_id) {
            let needs_terminal_sequence = {
                let run = &self.runs[run_index];
                status.is_terminal()
                    && (run.terminal_sequence.is_none()
                        || run.terminal_stop_reason != resolved_stop_reason)
            };
            let terminal_sequence =
                needs_terminal_sequence.then(|| self.allocate_replay_sequence());

            let run = &mut self.runs[run_index];
            run.status = status;
            if parent_run_id.is_some() || run.parent_run_id.is_none() {
                run.parent_run_id = parent_run_id;
            }
            if parent_tool_invocation_id.is_some() || run.parent_tool_invocation_id.is_none() {
                run.parent_tool_invocation_id = parent_tool_invocation_id;
            }
            run.updated_at = now;

            if status.is_terminal() {
                if let Some(terminal_sequence) = terminal_sequence {
                    run.terminal_sequence = Some(terminal_sequence);
                }
                run.terminal_stop_reason = resolved_stop_reason;
            } else {
                run.terminal_sequence = None;
                run.terminal_stop_reason = None;
            }

            return;
        }

        let created_sequence = self.allocate_replay_sequence();
        let terminal_sequence = status
            .is_terminal()
            .then(|| self.allocate_replay_sequence());

        self.runs.push(RunRecord {
            id: run_id,
            status,
            parent_run_id,
            parent_tool_invocation_id,
            created_sequence,
            terminal_sequence,
            terminal_stop_reason: resolved_stop_reason,
            created_at: now,
            updated_at: now,
        });
    }

    pub fn upsert_run_with_parent(
        &mut self,
        run_id: RunId,
        status: RunStatus,
        parent_run_id: Option<RunId>,
        parent_tool_invocation_id: Option<ToolInvocationId>,
    ) {
        self.upsert_run_with_parent_and_stop_reason(
            run_id,
            status,
            parent_run_id,
            parent_tool_invocation_id,
            None,
        );
    }

    pub fn latest_run_status(&self) -> Option<RunStatus> {
        self.runs
            .iter()
            .max_by_key(|run| run.latest_replay_sequence())
            .map(|run| run.status)
    }

    pub fn allocate_replay_sequence(&mut self) -> ReplaySequence {
        let sequence = self.next_replay_sequence.max(FIRST_REPLAY_SEQUENCE);
        self.next_replay_sequence = sequence.saturating_add(1);
        sequence
    }

    pub fn normalize_persistence(&mut self) {
        if self.has_legacy_replay_metadata() {
            self.resequence_from_legacy_timestamps();
        }

        for run in &mut self.runs {
            run.normalize_terminal_stop_reason();
        }

        self.next_replay_sequence = self
            .max_replay_sequence()
            .map(|sequence| sequence.saturating_add(1))
            .unwrap_or(FIRST_REPLAY_SEQUENCE);
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

    pub fn pending_tool_invocation_for_batch(
        &self,
        run_id: RunId,
        preceding_turn_id: Option<TurnId>,
    ) -> Option<&ToolInvocationRecord> {
        self.tool_invocations.iter().rev().find(|invocation| {
            invocation.run_id == run_id
                && invocation.preceding_turn_id == preceding_turn_id
                && invocation.approval_state == ToolApprovalState::Pending
        })
    }

    pub fn set_foreground_owner(
        &mut self,
        run_id: RunId,
        phase: ForegroundPhase,
        batch_anchor_turn_id: Option<TurnId>,
    ) {
        self.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase,
            batch_anchor_turn_id,
        });
    }

    pub fn clear_foreground_owner(&mut self) {
        self.foreground_owner = None;
    }

    pub fn find_tool_invocation_mut(
        &mut self,
        invocation_id: ToolInvocationId,
    ) -> Option<&mut ToolInvocationRecord> {
        self.tool_invocations
            .iter_mut()
            .find(|invocation| invocation.id == invocation_id)
    }

    fn has_legacy_replay_metadata(&self) -> bool {
        self.runs.iter().any(|run| run.created_sequence == 0)
            || self.turns.iter().any(|turn| turn.sequence_number == 0)
            || self
                .tool_invocations
                .iter()
                .any(|invocation| invocation.sequence_number == 0)
            || self.runs.iter().any(|run| {
                run.status.is_terminal()
                    && (run.terminal_sequence.is_none() || run.terminal_stop_reason.is_none())
            })
    }

    fn max_replay_sequence(&self) -> Option<ReplaySequence> {
        self.runs
            .iter()
            .map(|run| run.created_sequence)
            .chain(self.runs.iter().filter_map(|run| run.terminal_sequence))
            .chain(self.turns.iter().map(|turn| turn.sequence_number))
            .chain(
                self.tool_invocations
                    .iter()
                    .map(|invocation| invocation.sequence_number),
            )
            .filter(|sequence| *sequence >= FIRST_REPLAY_SEQUENCE)
            .max()
    }

    fn resequence_from_legacy_timestamps(&mut self) {
        #[derive(Clone, Copy)]
        enum ReplayEvent {
            RunCreated(usize),
            Turn(usize),
            ToolInvocation(usize),
            RunTerminal(usize),
        }

        let mut events = Vec::new();

        for (index, run) in self.runs.iter().enumerate() {
            events.push((run.created_at, 0_u8, index, ReplayEvent::RunCreated(index)));

            if run.status.is_terminal() {
                events.push((run.updated_at, 3_u8, index, ReplayEvent::RunTerminal(index)));
            }
        }

        for (index, turn) in self.turns.iter().enumerate() {
            events.push((turn.timestamp, 1_u8, index, ReplayEvent::Turn(index)));
        }

        for (index, invocation) in self.tool_invocations.iter().enumerate() {
            events.push((
                invocation.requested_at,
                2_u8,
                index,
                ReplayEvent::ToolInvocation(index),
            ));
        }

        events.sort_by_key(|(timestamp, priority, index, _)| (*timestamp, *priority, *index));

        let mut next_sequence = FIRST_REPLAY_SEQUENCE;

        for (_, _, _, event) in events {
            match event {
                ReplayEvent::RunCreated(index) => {
                    self.runs[index].created_sequence = next_sequence;
                }
                ReplayEvent::Turn(index) => {
                    self.turns[index].sequence_number = next_sequence;
                }
                ReplayEvent::ToolInvocation(index) => {
                    self.tool_invocations[index].sequence_number = next_sequence;
                }
                ReplayEvent::RunTerminal(index) => {
                    self.runs[index].terminal_sequence = Some(next_sequence);
                }
            }

            next_sequence = next_sequence.saturating_add(1);
        }
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
    #[serde(default)]
    pub created_sequence: ReplaySequence,
    #[serde(default)]
    pub terminal_sequence: Option<ReplaySequence>,
    #[serde(default)]
    pub terminal_stop_reason: Option<RunTerminalStopReason>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunRecord {
    pub fn latest_replay_sequence(&self) -> ReplaySequence {
        self.terminal_sequence.unwrap_or(self.created_sequence)
    }

    fn normalize_terminal_stop_reason(&mut self) {
        if self.status.is_terminal() && self.terminal_stop_reason.is_none() {
            self.terminal_stop_reason = self.status.default_terminal_stop_reason();
        }

        if !self.status.is_terminal() {
            self.terminal_sequence = None;
            self.terminal_stop_reason = None;
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::InProgress)
    }

    pub fn default_terminal_stop_reason(self) -> Option<RunTerminalStopReason> {
        match self {
            Self::InProgress => None,
            Self::Completed => Some(RunTerminalStopReason::Completed),
            Self::Failed => Some(RunTerminalStopReason::Failed),
            Self::Cancelled => Some(RunTerminalStopReason::Cancelled),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminalStopReason {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundPhase {
    Generating,
    AwaitingToolApproval,
    RunningTool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForegroundOwnerRecord {
    pub run_id: RunId,
    pub phase: ForegroundPhase,
    #[serde(default)]
    pub batch_anchor_turn_id: Option<TurnId>,
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
    #[serde(default)]
    pub sequence_number: ReplaySequence,
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

    pub fn delegation_status(&self) -> Option<TaskDelegationStatus> {
        self.delegation.as_ref().map(|delegation| delegation.status)
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
            status: TaskDelegationStatus::Running,
        });
    }

    pub fn set_task_delegation_status(&mut self, status: TaskDelegationStatus) {
        if let Some(delegation) = self.delegation.as_mut() {
            delegation.status = status;
        }
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
            sequence_number: compat.sequence_number,
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
    #[serde(default)]
    pub status: TaskDelegationStatus,
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
                status: TaskDelegationStatus::default(),
            },
        };

        delegation.normalize_legacy_status().into_option()
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

    fn normalize_legacy_status(mut self) -> Self {
        if self.status == TaskDelegationStatus::Pending && self.child_run_id.is_some() {
            self.status = TaskDelegationStatus::Running;
        }

        self
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskDelegationStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
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
    #[serde(default)]
    sequence_number: ReplaySequence,
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
    #[serde(default)]
    pub sequence_number: ReplaySequence,
    pub timestamp: DateTime<Utc>,
}

const fn default_next_replay_sequence() -> ReplaySequence {
    FIRST_REPLAY_SEQUENCE
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}
