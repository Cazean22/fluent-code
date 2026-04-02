use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::plugin::DiscoveryScope;

pub type SessionId = Uuid;
pub type TurnId = Uuid;
pub type RunId = Uuid;
pub type ToolInvocationId = Uuid;
pub type TranscriptItemId = Uuid;
pub type ReplaySequence = u64;

const FIRST_REPLAY_SEQUENCE: ReplaySequence = 1;

#[derive(Debug, Clone, Serialize)]
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
    pub transcript_fidelity: TranscriptFidelity,
    #[serde(default)]
    pub transcript_items: Vec<TranscriptItemRecord>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub tool_invocations: Vec<ToolInvocationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_owner: Option<ForegroundOwnerRecord>,
    #[serde(skip)]
    run_index: HashMap<RunId, usize>,
    #[serde(skip)]
    root_run_map: HashMap<RunId, RunId>,
    #[serde(skip)]
    transcript_item_index: HashMap<TranscriptItemId, usize>,
    #[serde(skip)]
    tool_invocation_index: HashMap<ToolInvocationId, usize>,
}

impl Session {
    pub fn new(title: impl Into<String>) -> Self {
        let now = Utc::now();

        let mut session = Self {
            id: Uuid::new_v4(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            next_replay_sequence: default_next_replay_sequence(),
            permissions: SessionPermissionState::default(),
            transcript_fidelity: TranscriptFidelity::Exact,
            transcript_items: Vec::new(),
            runs: Vec::new(),
            turns: Vec::new(),
            tool_invocations: Vec::new(),
            foreground_owner: None,
            run_index: HashMap::new(),
            root_run_map: HashMap::new(),
            transcript_item_index: HashMap::new(),
            tool_invocation_index: HashMap::new(),
        };
        session.rebuild_ephemeral_indexes();
        session
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

        if let Some(run_index) = self.run_index.get(&run_id).copied() {
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

            self.rebuild_run_indexes();
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
        self.rebuild_run_indexes();
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

        self.transcript_items
            .sort_by_key(|item| item.sequence_number);

        for run in &mut self.runs {
            run.normalize_terminal_stop_reason();
        }

        self.next_replay_sequence = self
            .max_replay_sequence()
            .map(|sequence| sequence.saturating_add(1))
            .unwrap_or(FIRST_REPLAY_SEQUENCE);
        self.rebuild_ephemeral_indexes();
    }

    pub fn has_replay_visible_legacy_items(&self) -> bool {
        !self.runs.is_empty() || !self.turns.is_empty() || !self.tool_invocations.is_empty()
    }

    pub fn requires_approximate_transcript_synthesis(&self) -> bool {
        self.transcript_items.is_empty() && self.has_replay_visible_legacy_items()
    }

    pub fn find_run(&self, run_id: RunId) -> Option<&RunRecord> {
        let run_index = self.run_index.get(&run_id).copied()?;
        self.runs.get(run_index)
    }

    pub fn find_run_mut(&mut self, run_id: RunId) -> Option<&mut RunRecord> {
        let run_index = self.run_index.get(&run_id).copied()?;
        self.runs.get_mut(run_index)
    }

    pub fn rebuild_run_indexes(&mut self) {
        self.run_index.clear();
        for (run_index, run) in self.runs.iter().enumerate() {
            self.run_index.entry(run.id).or_insert(run_index);
        }

        self.root_run_map.clear();
        let run_ids = self.runs.iter().map(|run| run.id).collect::<Vec<_>>();
        for run_id in run_ids {
            if self.root_run_map.contains_key(&run_id) {
                continue;
            }

            if let Some(root_run_id) = self.resolve_root_run_id(run_id) {
                self.root_run_map.entry(run_id).or_insert(root_run_id);
            }
        }
    }

    fn rebuild_ephemeral_indexes(&mut self) {
        self.rebuild_run_indexes();
        self.rebuild_transcript_item_index();
        self.rebuild_tool_invocation_index();
    }

    fn rebuild_transcript_item_index(&mut self) {
        self.transcript_item_index.clear();
        for (item_index, item) in self.transcript_items.iter().enumerate() {
            self.transcript_item_index
                .entry(item.item_id)
                .or_insert(item_index);
        }
    }

    fn rebuild_tool_invocation_index(&mut self) {
        self.tool_invocation_index.clear();
        for (invocation_index, invocation) in self.tool_invocations.iter().enumerate() {
            self.tool_invocation_index
                .entry(invocation.id)
                .or_insert(invocation_index);
        }
    }

    pub fn root_run_id(&self, run_id: RunId) -> Option<RunId> {
        self.root_run_map.get(&run_id).copied()
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

    pub fn find_tool_invocation(
        &self,
        invocation_id: ToolInvocationId,
    ) -> Option<&ToolInvocationRecord> {
        if let Some(invocation_index) = self.tool_invocation_index_position(invocation_id) {
            return self.tool_invocations.get(invocation_index);
        }

        self.tool_invocations
            .iter()
            .find(|invocation| invocation.id == invocation_id)
    }

    pub fn find_tool_invocation_mut(
        &mut self,
        invocation_id: ToolInvocationId,
    ) -> Option<&mut ToolInvocationRecord> {
        let invocation_index = self.find_tool_invocation_index(invocation_id)?;
        self.tool_invocations.get_mut(invocation_index)
    }

    pub fn find_transcript_item(&self, item_id: TranscriptItemId) -> Option<&TranscriptItemRecord> {
        if let Some(item_index) = self.transcript_item_index_position(item_id) {
            return self.transcript_items.get(item_index);
        }

        self.transcript_items
            .iter()
            .find(|item| item.item_id == item_id)
    }

    pub fn find_transcript_item_mut(
        &mut self,
        item_id: TranscriptItemId,
    ) -> Option<&mut TranscriptItemRecord> {
        let item_index = self.find_transcript_item_index(item_id)?;
        self.transcript_items.get_mut(item_index)
    }

    pub fn upsert_transcript_item(&mut self, item: TranscriptItemRecord) {
        if let Some(existing_item_index) = self.find_transcript_item_index(item.item_id) {
            if self.transcript_items[existing_item_index].sequence_number == item.sequence_number {
                self.transcript_items[existing_item_index] = item;
                return;
            }

            self.transcript_items.remove(existing_item_index);
            self.insert_transcript_item_in_order(item);
            self.rebuild_transcript_item_index();
            return;
        }

        self.insert_transcript_item_in_order(item);
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
        self.transcript_items
            .iter()
            .map(|item| item.sequence_number)
            .chain(
                self.runs
                    .iter()
                    .map(|run| run.created_sequence)
                    .chain(self.runs.iter().filter_map(|run| run.terminal_sequence))
                    .chain(self.turns.iter().map(|turn| turn.sequence_number))
                    .chain(
                        self.tool_invocations
                            .iter()
                            .map(|invocation| invocation.sequence_number),
                    ),
            )
            .filter(|sequence| *sequence >= FIRST_REPLAY_SEQUENCE)
            .max()
    }

    pub fn synthesize_approximate_transcript_items(&mut self) {
        let mut transcript_items = Vec::new();

        for run in &self.runs {
            transcript_items.push(TranscriptItemRecord::run_started(run));

            if run.status.is_terminal() {
                transcript_items.push(TranscriptItemRecord::run_terminal(run));
            }
        }

        transcript_items.extend(self.turns.iter().map(TranscriptItemRecord::from_turn));
        transcript_items.extend(
            self.tool_invocations
                .iter()
                .map(TranscriptItemRecord::from_tool_invocation),
        );
        transcript_items.sort_by_key(|item| item.sequence_number);

        self.transcript_fidelity = TranscriptFidelity::Approximate;
        self.transcript_items = transcript_items;
        self.rebuild_transcript_item_index();
    }

    fn tool_invocation_index_position(&self, invocation_id: ToolInvocationId) -> Option<usize> {
        let invocation_index = self.tool_invocation_index.get(&invocation_id).copied()?;
        self.tool_invocations
            .get(invocation_index)
            .filter(|invocation| invocation.id == invocation_id)
            .map(|_| invocation_index)
    }

    fn find_tool_invocation_index(&mut self, invocation_id: ToolInvocationId) -> Option<usize> {
        if let Some(invocation_index) = self.tool_invocation_index_position(invocation_id) {
            return Some(invocation_index);
        }

        self.rebuild_tool_invocation_index();
        self.tool_invocation_index_position(invocation_id)
    }

    fn transcript_item_index_position(&self, item_id: TranscriptItemId) -> Option<usize> {
        let item_index = self.transcript_item_index.get(&item_id).copied()?;
        self.transcript_items
            .get(item_index)
            .filter(|item| item.item_id == item_id)
            .map(|_| item_index)
    }

    fn find_transcript_item_index(&mut self, item_id: TranscriptItemId) -> Option<usize> {
        if let Some(item_index) = self.transcript_item_index_position(item_id) {
            return Some(item_index);
        }

        self.rebuild_transcript_item_index();
        self.transcript_item_index_position(item_id)
    }

    fn insert_transcript_item_in_order(&mut self, item: TranscriptItemRecord) {
        let insertion_index = self
            .transcript_items
            .partition_point(|existing| existing.sequence_number <= item.sequence_number);

        if insertion_index == self.transcript_items.len() {
            self.transcript_items.push(item);
            let item_index = self.transcript_items.len() - 1;
            let item_id = self.transcript_items[item_index].item_id;
            self.transcript_item_index.insert(item_id, item_index);
            return;
        }

        self.transcript_items.insert(insertion_index, item);
        self.rebuild_transcript_item_index();
    }

    fn resolve_root_run_id(&mut self, run_id: RunId) -> Option<RunId> {
        let mut lineage = Vec::new();
        let mut seen_run_ids = HashSet::new();
        let mut current_run_id = run_id;

        let resolved_root_run_id = loop {
            if let Some(cached_root_run_id) = self.root_run_map.get(&current_run_id).copied() {
                break Some(cached_root_run_id);
            }

            if !seen_run_ids.insert(current_run_id) {
                break None;
            }

            let Some(run_index) = self.run_index.get(&current_run_id).copied() else {
                break None;
            };

            lineage.push(current_run_id);
            match self.runs[run_index].parent_run_id {
                Some(parent_run_id) => current_run_id = parent_run_id,
                None => break Some(current_run_id),
            }
        };

        if let Some(root_run_id) = resolved_root_run_id {
            for lineage_run_id in lineage {
                self.root_run_map
                    .entry(lineage_run_id)
                    .or_insert(root_run_id);
            }
        }

        resolved_root_run_id
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

impl<'de> Deserialize<'de> for Session {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let compat = SessionCompat::deserialize(deserializer)?;

        let mut session = Self {
            id: compat.id,
            title: compat.title,
            created_at: compat.created_at,
            updated_at: compat.updated_at,
            next_replay_sequence: compat.next_replay_sequence,
            permissions: compat.permissions,
            transcript_fidelity: compat.transcript_fidelity,
            transcript_items: compat.transcript_items,
            runs: compat.runs,
            turns: compat.turns,
            tool_invocations: compat.tool_invocations,
            foreground_owner: compat.foreground_owner,
            run_index: HashMap::new(),
            root_run_map: HashMap::new(),
            transcript_item_index: HashMap::new(),
            tool_invocation_index: HashMap::new(),
        };
        session.rebuild_ephemeral_indexes();
        Ok(session)
    }
}

#[derive(Debug, Deserialize)]
struct SessionCompat {
    id: SessionId,
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default = "default_next_replay_sequence")]
    next_replay_sequence: ReplaySequence,
    #[serde(default)]
    permissions: SessionPermissionState,
    #[serde(default)]
    transcript_fidelity: TranscriptFidelity,
    #[serde(default)]
    transcript_items: Vec<TranscriptItemRecord>,
    #[serde(default)]
    runs: Vec<RunRecord>,
    #[serde(default)]
    turns: Vec<Turn>,
    #[serde(default)]
    tool_invocations: Vec<ToolInvocationRecord>,
    #[serde(default)]
    foreground_owner: Option<ForegroundOwnerRecord>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFidelity {
    #[default]
    Exact,
    Approximate,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStreamState {
    Open,
    #[default]
    Committed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptItemKind {
    Turn,
    ToolInvocation,
    RunLifecycle,
    Permission,
    DelegatedChild,
    Marker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptItemRecord {
    pub item_id: TranscriptItemId,
    pub sequence_number: ReplaySequence,
    pub run_id: RunId,
    pub kind: TranscriptItemKind,
    #[serde(default)]
    pub stream_state: TranscriptStreamState,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    #[serde(default)]
    pub tool_invocation_id: Option<ToolInvocationId>,
    #[serde(default)]
    pub parent_item_id: Option<TranscriptItemId>,
    #[serde(default)]
    pub parent_tool_invocation_id: Option<ToolInvocationId>,
    #[serde(default)]
    pub child_run_id: Option<RunId>,
    pub content: TranscriptItemContent,
}

impl TranscriptItemRecord {
    pub fn from_turn(turn: &Turn) -> Self {
        Self {
            item_id: turn.id,
            sequence_number: turn.sequence_number,
            run_id: turn.run_id,
            kind: TranscriptItemKind::Turn,
            stream_state: TranscriptStreamState::Committed,
            turn_id: Some(turn.id),
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id: None,
            child_run_id: None,
            content: TranscriptItemContent::Turn(TranscriptTurnContent {
                role: turn.role,
                content: turn.content.clone(),
                reasoning: turn.reasoning.clone(),
            }),
        }
    }

    pub fn from_tool_invocation(invocation: &ToolInvocationRecord) -> Self {
        Self {
            item_id: invocation.id,
            sequence_number: invocation.sequence_number,
            run_id: invocation.run_id,
            kind: TranscriptItemKind::ToolInvocation,
            stream_state: if matches!(
                invocation.execution_state,
                ToolExecutionState::NotStarted | ToolExecutionState::Running
            ) {
                TranscriptStreamState::Open
            } else {
                TranscriptStreamState::Committed
            },
            turn_id: invocation.preceding_turn_id,
            tool_invocation_id: Some(invocation.id),
            parent_item_id: invocation.preceding_turn_id,
            parent_tool_invocation_id: None,
            child_run_id: invocation.child_run_id(),
            content: TranscriptItemContent::ToolInvocation(TranscriptToolInvocationContent {
                tool_call_id: invocation.tool_call_id.clone(),
                tool_name: invocation.tool_name.clone(),
                tool_source: invocation.tool_source.clone(),
                arguments: invocation.arguments.clone(),
                preceding_turn_id: invocation.preceding_turn_id,
                approval_state: invocation.approval_state,
                execution_state: invocation.execution_state,
                result: invocation.result.clone(),
                error: invocation.error.clone(),
                delegation: invocation.delegation.clone(),
            }),
        }
    }

    pub fn run_started(run: &RunRecord) -> Self {
        Self {
            item_id: transcript_run_marker_id(run.id, TranscriptRunLifecycleEvent::Started),
            sequence_number: run.created_sequence,
            run_id: run.id,
            kind: TranscriptItemKind::RunLifecycle,
            stream_state: TranscriptStreamState::Committed,
            turn_id: None,
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id: run.parent_tool_invocation_id,
            child_run_id: None,
            content: TranscriptItemContent::RunLifecycle(TranscriptRunLifecycleContent {
                event: TranscriptRunLifecycleEvent::Started,
                status: RunStatus::InProgress,
                stop_reason: None,
            }),
        }
    }

    pub fn run_terminal(run: &RunRecord) -> Self {
        Self {
            item_id: transcript_run_marker_id(run.id, TranscriptRunLifecycleEvent::Terminal),
            sequence_number: run.terminal_sequence.unwrap_or(run.created_sequence),
            run_id: run.id,
            kind: TranscriptItemKind::RunLifecycle,
            stream_state: TranscriptStreamState::Committed,
            turn_id: None,
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id: run.parent_tool_invocation_id,
            child_run_id: None,
            content: TranscriptItemContent::RunLifecycle(TranscriptRunLifecycleContent {
                event: TranscriptRunLifecycleEvent::Terminal,
                status: run.status,
                stop_reason: run.terminal_stop_reason,
            }),
        }
    }

    pub fn assistant_reasoning(
        run_id: RunId,
        turn_id: TurnId,
        sequence_number: ReplaySequence,
        reasoning: impl Into<String>,
        stream_state: TranscriptStreamState,
    ) -> Self {
        Self {
            item_id: transcript_assistant_reasoning_item_id(turn_id),
            sequence_number,
            run_id,
            kind: TranscriptItemKind::Turn,
            stream_state,
            turn_id: Some(turn_id),
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id: None,
            child_run_id: None,
            content: TranscriptItemContent::Turn(TranscriptTurnContent {
                role: Role::Assistant,
                content: String::new(),
                reasoning: reasoning.into(),
            }),
        }
    }

    pub fn assistant_text(
        run_id: RunId,
        turn_id: TurnId,
        sequence_number: ReplaySequence,
        content: impl Into<String>,
        stream_state: TranscriptStreamState,
    ) -> Self {
        Self {
            item_id: transcript_assistant_text_item_id(turn_id),
            sequence_number,
            run_id,
            kind: TranscriptItemKind::Turn,
            stream_state,
            turn_id: Some(turn_id),
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id: None,
            child_run_id: None,
            content: TranscriptItemContent::Turn(TranscriptTurnContent {
                role: Role::Assistant,
                content: content.into(),
                reasoning: String::new(),
            }),
        }
    }

    pub fn permission(
        invocation: &ToolInvocationRecord,
        sequence_number: ReplaySequence,
        state: TranscriptPermissionState,
        decision: Option<ToolPermissionAction>,
    ) -> Self {
        Self {
            item_id: transcript_permission_item_id(invocation.id),
            sequence_number,
            run_id: invocation.run_id,
            kind: TranscriptItemKind::Permission,
            stream_state: if state == TranscriptPermissionState::Pending {
                TranscriptStreamState::Open
            } else {
                TranscriptStreamState::Committed
            },
            turn_id: invocation.preceding_turn_id,
            tool_invocation_id: Some(invocation.id),
            parent_item_id: invocation.preceding_turn_id,
            parent_tool_invocation_id: Some(invocation.id),
            child_run_id: invocation.child_run_id(),
            content: TranscriptItemContent::Permission(TranscriptPermissionContent {
                tool_name: invocation.tool_name.clone(),
                tool_source: invocation.tool_source.clone(),
                state,
                decision,
            }),
        }
    }

    pub fn delegated_child(
        invocation: &ToolInvocationRecord,
        sequence_number: ReplaySequence,
    ) -> Self {
        let delegation = invocation.task_delegation().cloned().unwrap_or_default();
        let status = delegation.status;
        Self {
            item_id: transcript_delegated_child_item_id(invocation.id),
            sequence_number,
            run_id: invocation.run_id,
            kind: TranscriptItemKind::DelegatedChild,
            stream_state: if matches!(
                status,
                TaskDelegationStatus::Pending | TaskDelegationStatus::Running
            ) {
                TranscriptStreamState::Open
            } else {
                TranscriptStreamState::Committed
            },
            turn_id: invocation.preceding_turn_id,
            tool_invocation_id: Some(invocation.id),
            parent_item_id: invocation.preceding_turn_id,
            parent_tool_invocation_id: Some(invocation.id),
            child_run_id: delegation.child_run_id,
            content: TranscriptItemContent::DelegatedChild(TranscriptDelegatedChildContent {
                child_run_id: delegation.child_run_id,
                agent_name: delegation.agent_name,
                prompt: delegation.prompt,
                status,
            }),
        }
    }

    pub fn marker(
        run_id: RunId,
        sequence_number: ReplaySequence,
        label: impl Into<String>,
        detail: Option<String>,
        parent_tool_invocation_id: Option<ToolInvocationId>,
        child_run_id: Option<RunId>,
    ) -> Self {
        Self {
            item_id: Uuid::new_v4(),
            sequence_number,
            run_id,
            kind: TranscriptItemKind::Marker,
            stream_state: TranscriptStreamState::Committed,
            turn_id: None,
            tool_invocation_id: None,
            parent_item_id: None,
            parent_tool_invocation_id,
            child_run_id,
            content: TranscriptItemContent::Marker(TranscriptMarkerContent {
                label: label.into(),
                detail,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItemContent {
    Turn(TranscriptTurnContent),
    ToolInvocation(TranscriptToolInvocationContent),
    RunLifecycle(TranscriptRunLifecycleContent),
    Permission(TranscriptPermissionContent),
    DelegatedChild(TranscriptDelegatedChildContent),
    Marker(TranscriptMarkerContent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptTurnContent {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptToolInvocationContent {
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRunLifecycleEvent {
    Started,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptRunLifecycleContent {
    pub event: TranscriptRunLifecycleEvent,
    pub status: RunStatus,
    #[serde(default)]
    pub stop_reason: Option<RunTerminalStopReason>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptPermissionState {
    #[default]
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptPermissionContent {
    pub tool_name: String,
    #[serde(default)]
    pub tool_source: ToolSource,
    #[serde(default)]
    pub state: TranscriptPermissionState,
    #[serde(default)]
    pub decision: Option<ToolPermissionAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptDelegatedChildContent {
    #[serde(default)]
    pub child_run_id: Option<RunId>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub status: TaskDelegationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptMarkerContent {
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
}

fn transcript_run_marker_id(run_id: RunId, event: TranscriptRunLifecycleEvent) -> TranscriptItemId {
    let salt = match event {
        TranscriptRunLifecycleEvent::Started => 0xfeed_face_feed_face_feed_face_feed_face_u128,
        TranscriptRunLifecycleEvent::Terminal => 0xdead_beef_dead_beef_dead_beef_dead_beef_u128,
    };

    Uuid::from_u128(run_id.as_u128() ^ salt)
}

pub fn transcript_assistant_reasoning_item_id(turn_id: TurnId) -> TranscriptItemId {
    Uuid::from_u128(turn_id.as_u128() ^ 0xa11c_e001_a11c_e001_a11c_e001_a11c_e001_u128)
}

pub fn transcript_assistant_text_item_id(turn_id: TurnId) -> TranscriptItemId {
    Uuid::from_u128(turn_id.as_u128() ^ 0xa11c_e002_a11c_e002_a11c_e002_a11c_e002_u128)
}

pub fn transcript_permission_item_id(tool_invocation_id: ToolInvocationId) -> TranscriptItemId {
    Uuid::from_u128(tool_invocation_id.as_u128() ^ 0xc0de_0001_c0de_0001_c0de_0001_c0de_0001_u128)
}

pub fn transcript_delegated_child_item_id(
    tool_invocation_id: ToolInvocationId,
) -> TranscriptItemId {
    Uuid::from_u128(tool_invocation_id.as_u128() ^ 0xde1e_0002_de1e_0002_de1e_0002_de1e_0002_u128)
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[cfg(test)]
mod tests {
    use super::{
        ReplaySequence, Role, RunRecord, RunStatus, Session, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, TranscriptItemContent, TranscriptItemRecord,
        TranscriptStreamState,
    };
    use chrono::Utc;
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

        let first_round_tripped_item = round_tripped
            .find_transcript_item_mut(first_item.item_id)
            .expect("mutable transcript lookup survives serde round trip");
        match &mut first_round_tripped_item.content {
            TranscriptItemContent::Turn(content) => {
                content.content = "first answer updated after serde".to_string();
            }
            other => panic!("expected turn transcript item, found {other:?}"),
        }

        round_tripped.upsert_transcript_item(TranscriptItemRecord::assistant_text(
            run_id,
            second_turn_id,
            3,
            "second answer updated after serde",
            TranscriptStreamState::Committed,
        ));

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
                    .expect("updated first transcript item remains reachable"),
            ),
            "first answer updated after serde"
        );
        assert_eq!(
            transcript_turn_text(
                round_tripped
                    .find_transcript_item(second_item.item_id)
                    .expect("replacement transcript item remains reachable after serde"),
            ),
            "second answer updated after serde"
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
        assert_eq!(
            round_tripped
                .find_tool_invocation(invocation.id)
                .expect("immutable tool lookup survives serde round trip")
                .result
                .as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn upsert_transcript_item_preserves_canonical_order_without_global_resort() {
        let mut session = Session::new("ordered transcript upsert");
        let run_id = Uuid::new_v4();
        let first_turn_id = Uuid::new_v4();
        let inserted_turn_id = Uuid::new_v4();
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
        let inserted_item = TranscriptItemRecord::assistant_text(
            run_id,
            inserted_turn_id,
            15,
            "between",
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

        session.upsert_transcript_item(inserted_item.clone());

        assert_eq!(
            transcript_turn_snapshot(&session),
            vec![
                (10, "first".to_string()),
                (15, "between".to_string()),
                (20, "second updated".to_string()),
                (30, "third".to_string()),
            ]
        );
        assert_eq!(
            transcript_turn_text(
                session
                    .find_transcript_item(inserted_item.item_id)
                    .expect("inserted transcript item remains reachable after reordering"),
            ),
            "between"
        );
    }

    #[test]
    fn tool_invocation_index_tracks_insert_and_replace_paths() {
        let mut session = Session::new("tool invocation lookups");
        let run_id = Uuid::new_v4();
        let inserted_invocation = tool_invocation_record(run_id, 0, None);
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

        session
            .tool_invocations
            .insert(0, inserted_invocation.clone());
        {
            let first = session
                .find_tool_invocation_mut(first_invocation.id)
                .expect("first invocation remains reachable after front insert");
            first.execution_state = ToolExecutionState::Failed;
            first.error = Some("first failed".to_string());
        }

        session.tool_invocations.push(second_invocation.clone());
        session.tool_invocations[2] = ToolInvocationRecord {
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: Some("replacement result".to_string()),
            ..session.tool_invocations[2].clone()
        };
        {
            let second = session
                .find_tool_invocation_mut(second_invocation.id)
                .expect("second replaced invocation remains reachable");
            second.execution_state = ToolExecutionState::Completed;
            second.result = Some("second result".to_string());
            second.error = None;
        }

        assert_eq!(
            session.tool_invocations[0].execution_state,
            ToolExecutionState::NotStarted
        );
        assert_eq!(
            session.tool_invocations[1].execution_state,
            ToolExecutionState::Failed
        );
        assert_eq!(
            session.tool_invocations[1].error.as_deref(),
            Some("first failed")
        );
        assert_eq!(
            session.tool_invocations[2].execution_state,
            ToolExecutionState::Completed
        );
        assert_eq!(
            session.tool_invocations[2].result.as_deref(),
            Some("second result")
        );
        assert_eq!(
            session
                .find_tool_invocation(second_invocation.id)
                .expect("immutable tool lookup tracks latest replacement")
                .result
                .as_deref(),
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
}
