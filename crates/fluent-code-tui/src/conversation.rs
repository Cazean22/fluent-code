use chrono::{DateTime, Utc};
use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{
    Role, RunStatus, Session, ToolApprovalState, ToolExecutionState, ToolInvocationRecord,
    ToolSource,
};
use uuid::Uuid;

const SUMMARY_LIMIT: usize = 72;

#[derive(Debug, Clone)]
pub(crate) enum ConversationRow {
    Turn(TurnRow),
    Reasoning(ReasoningRow),
    Tool(Box<ToolRow>),
    ToolGroup(ToolGroupRow),
    RunMarker(RunMarkerRow),
}

#[derive(Debug, Clone)]
pub(crate) struct TurnRow {
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) is_streaming: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ReasoningRow {
    pub(crate) content: String,
    pub(crate) is_streaming: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolRow {
    pub(crate) tool_name: String,
    pub(crate) display_name: String,
    pub(crate) summary: String,
    pub(crate) provenance_compact: Option<String>,
    pub(crate) provenance_expanded: Option<String>,
    pub(crate) arguments_preview: String,
    pub(crate) delegated_task: Option<DelegatedTaskRow>,
    pub(crate) approval_state: ToolApprovalState,
    pub(crate) execution_state: ToolExecutionState,
    pub(crate) result_preview: Option<String>,
    pub(crate) error_preview: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DelegatedTaskRow {
    pub(crate) agent_name: Option<String>,
    pub(crate) prompt_preview: Option<String>,
    pub(crate) child_run_id: Option<Uuid>,
    pub(crate) child_run_status: Option<RunStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolGroupKind {
    ReadLike,
    SearchLike,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolGroupRow {
    pub(crate) kind: ToolGroupKind,
    pub(crate) items: Vec<ToolRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunMarkerKind {
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub(crate) struct RunMarkerRow {
    pub(crate) kind: RunMarkerKind,
    pub(crate) label: String,
}

pub(crate) fn derive_conversation_rows(state: &AppState) -> Vec<ConversationRow> {
    let session = &state.session;

    // Build a flat timeline of all events sorted by timestamp so that
    // reasoning, turns, and tool calls appear in the order they actually
    // happened rather than being grouped by turn.
    let mut timeline = build_timeline(state);
    timeline.sort_by_key(|e| e.sort_key);

    let mut rows = Vec::new();
    let mut pending_tools: Vec<ToolRow> = Vec::new();

    for entry in &timeline {
        match entry.kind {
            TimelineKind::Reasoning(turn_idx) => {
                flush_pending_tools(&mut rows, &mut pending_tools);
                let turn = &session.turns[turn_idx];
                let is_streaming = matches!(turn.role, Role::Assistant)
                    && state.active_run_id == Some(turn.run_id)
                    && matches!(state.status, AppStatus::Generating);
                rows.push(ConversationRow::Reasoning(ReasoningRow {
                    content: turn.reasoning.clone(),
                    is_streaming,
                }));
            }
            TimelineKind::Turn(turn_idx) => {
                flush_pending_tools(&mut rows, &mut pending_tools);
                let turn = &session.turns[turn_idx];
                let is_streaming = matches!(turn.role, Role::Assistant)
                    && state.active_run_id == Some(turn.run_id)
                    && matches!(state.status, AppStatus::Generating);
                rows.push(ConversationRow::Turn(TurnRow {
                    role: turn.role,
                    content: turn.content.clone(),
                    is_streaming,
                }));
            }
            TimelineKind::Tool(inv_idx) => {
                let invocation = &session.tool_invocations[inv_idx];
                pending_tools.push(derive_tool_row(session, invocation));
            }
        }
    }

    flush_pending_tools(&mut rows, &mut pending_tools);

    if let Some(marker) = derive_run_marker(state) {
        rows.push(ConversationRow::RunMarker(marker));
    }

    rows
}

// ---------------------------------------------------------------------------
// Timeline construction
// ---------------------------------------------------------------------------

/// Priority within the same timestamp: reasoning (0) < turn content (1) < tool (2).
const PRIORITY_REASONING: u8 = 0;
const PRIORITY_TURN: u8 = 1;
const PRIORITY_TOOL: u8 = 2;

enum TimelineKind {
    Reasoning(usize),
    Turn(usize),
    Tool(usize),
}

struct TimelineEntry {
    /// `(timestamp, priority, sequence)` – used for a single `sort_by_key`.
    sort_key: (DateTime<Utc>, u8, usize),
    kind: TimelineKind,
}

fn build_timeline(state: &AppState) -> Vec<TimelineEntry> {
    let session = &state.session;
    let mut entries = Vec::new();
    let mut seq = 0usize;

    for (i, turn) in session.turns.iter().enumerate() {
        if matches!(turn.role, Role::Assistant) && !turn.reasoning.is_empty() {
            entries.push(TimelineEntry {
                sort_key: (turn.timestamp, PRIORITY_REASONING, seq),
                kind: TimelineKind::Reasoning(i),
            });
            seq += 1;
        }

        entries.push(TimelineEntry {
            sort_key: (turn.timestamp, PRIORITY_TURN, seq),
            kind: TimelineKind::Turn(i),
        });
        seq += 1;
    }

    for (i, invocation) in session.tool_invocations.iter().enumerate() {
        entries.push(TimelineEntry {
            sort_key: (invocation.requested_at, PRIORITY_TOOL, seq),
            kind: TimelineKind::Tool(i),
        });
        seq += 1;
    }

    entries
}

fn flush_pending_tools(rows: &mut Vec<ConversationRow>, pending: &mut Vec<ToolRow>) {
    if pending.is_empty() {
        return;
    }
    rows.extend(group_tool_rows(std::mem::take(pending)));
}

fn derive_run_marker(state: &AppState) -> Option<RunMarkerRow> {
    if state.active_run_id.is_some() {
        let active_child_suffix = active_child_run_suffix(state);
        return match &state.status {
            AppStatus::AwaitingToolApproval => Some(RunMarkerRow {
                kind: RunMarkerKind::AwaitingApproval,
                label: format_run_marker_label("awaiting approval", active_child_suffix.as_deref()),
            }),
            AppStatus::Generating | AppStatus::RunningTool => Some(RunMarkerRow {
                kind: RunMarkerKind::Running,
                label: format_run_marker_label("running", active_child_suffix.as_deref()),
            }),
            _ => None,
        };
    }

    match state.session.latest_run_status() {
        Some(RunStatus::Completed) => Some(RunMarkerRow {
            kind: RunMarkerKind::Completed,
            label: "completed".to_string(),
        }),
        Some(RunStatus::Failed) => Some(RunMarkerRow {
            kind: RunMarkerKind::Failed,
            label: "failed".to_string(),
        }),
        Some(RunStatus::Cancelled) => Some(RunMarkerRow {
            kind: RunMarkerKind::Cancelled,
            label: "cancelled".to_string(),
        }),
        _ => None,
    }
}

fn format_run_marker_label(base: &str, child_suffix: Option<&str>) -> String {
    match child_suffix {
        Some(child_suffix) => format!("{base} · {child_suffix}"),
        None => base.to_string(),
    }
}

fn active_child_run_suffix(state: &AppState) -> Option<String> {
    let active_run_id = state.active_run_id?;
    let invocation = state.session.tool_invocations.iter().find(|invocation| {
        invocation.tool_name == "task" && invocation.child_run_id() == Some(active_run_id)
    })?;

    Some(
        match invocation
            .delegation_agent_name()
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
        {
            Some(agent) => format!("subagent {agent}"),
            None => "subagent".to_string(),
        },
    )
}

fn group_tool_rows(tool_rows: Vec<ToolRow>) -> Vec<ConversationRow> {
    let mut grouped_rows = Vec::new();
    let mut buffer = Vec::new();
    let mut current_kind = None;

    for tool_row in tool_rows {
        let next_kind = classify_group_kind(&tool_row);

        if buffer.is_empty() {
            buffer.push(tool_row);
            current_kind = next_kind;
            continue;
        }

        if next_kind.is_some() && next_kind == current_kind {
            buffer.push(tool_row);
            continue;
        }

        flush_tool_buffer(&mut grouped_rows, &mut buffer, current_kind);
        buffer.push(tool_row);
        current_kind = next_kind;
    }

    flush_tool_buffer(&mut grouped_rows, &mut buffer, current_kind);
    grouped_rows
}

fn flush_tool_buffer(
    grouped_rows: &mut Vec<ConversationRow>,
    buffer: &mut Vec<ToolRow>,
    kind: Option<ToolGroupKind>,
) {
    if buffer.is_empty() {
        return;
    }

    if let Some(kind) = kind
        && buffer.len() > 1
    {
        grouped_rows.push(ConversationRow::ToolGroup(ToolGroupRow {
            kind,
            items: std::mem::take(buffer),
        }));
        return;
    }

    grouped_rows.extend(
        buffer
            .drain(..)
            .map(|tool| ConversationRow::Tool(Box::new(tool))),
    );
}

fn classify_group_kind(tool: &ToolRow) -> Option<ToolGroupKind> {
    let tool_name = tool.tool_name.to_ascii_lowercase();

    if tool_name.contains("read") {
        return Some(ToolGroupKind::ReadLike);
    }

    if tool_name.contains("search") || tool_name.contains("grep") {
        return Some(ToolGroupKind::SearchLike);
    }

    None
}

pub(crate) fn derive_tool_row(session: &Session, invocation: &ToolInvocationRecord) -> ToolRow {
    let delegated_task = derive_delegated_task_row(session, invocation);
    let display_name = delegated_task_display_name(invocation, delegated_task.as_ref());

    ToolRow {
        tool_name: invocation.tool_name.clone(),
        display_name,
        summary: summarize_tool(invocation, delegated_task.as_ref()),
        provenance_compact: summarize_tool_provenance_compact(&invocation.tool_source),
        provenance_expanded: summarize_tool_provenance_expanded(&invocation.tool_source),
        arguments_preview: summarize_json(&invocation.arguments),
        delegated_task,
        approval_state: invocation.approval_state,
        execution_state: invocation.execution_state,
        result_preview: invocation.result.as_deref().map(summarize_text),
        error_preview: invocation.error.as_deref().map(summarize_text),
    }
}

fn derive_delegated_task_row(
    session: &Session,
    invocation: &ToolInvocationRecord,
) -> Option<DelegatedTaskRow> {
    if invocation.tool_name != "task" {
        return None;
    }

    let agent_name = invocation
        .delegation_agent_name()
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned);
    let prompt_preview = invocation
        .delegation_prompt()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(summarize_text);
    let child_run_status = invocation
        .child_run_id()
        .and_then(|child_run_id| session.find_run(child_run_id).map(|run| run.status));

    if agent_name.is_none() && prompt_preview.is_none() && child_run_status.is_none() {
        return None;
    }

    Some(DelegatedTaskRow {
        agent_name,
        prompt_preview,
        child_run_id: invocation.child_run_id(),
        child_run_status,
    })
}

fn delegated_task_display_name(
    invocation: &ToolInvocationRecord,
    delegated_task: Option<&DelegatedTaskRow>,
) -> String {
    if invocation.tool_name != "task" {
        return invocation.tool_name.clone();
    }

    match delegated_task.and_then(|delegated_task| delegated_task.agent_name.as_deref()) {
        Some(agent) => format!("task {agent}"),
        None => invocation.tool_name.clone(),
    }
}

fn summarize_tool_provenance_compact(tool_source: &ToolSource) -> Option<String> {
    match tool_source {
        ToolSource::BuiltIn => None,
        ToolSource::Plugin { plugin_name, .. } => Some(format!("plugin {plugin_name}")),
    }
}

fn summarize_tool_provenance_expanded(tool_source: &ToolSource) -> Option<String> {
    match tool_source {
        ToolSource::BuiltIn => None,
        ToolSource::Plugin {
            plugin_id,
            plugin_name,
            plugin_version,
            scope,
        } => Some(format!(
            "plugin {plugin_name} v{plugin_version} · {} · {plugin_id}",
            format_discovery_scope(*scope)
        )),
    }
}

fn format_discovery_scope(scope: fluent_code_app::plugin::DiscoveryScope) -> &'static str {
    match scope {
        fluent_code_app::plugin::DiscoveryScope::Global => "global",
        fluent_code_app::plugin::DiscoveryScope::Project => "project",
    }
}

fn summarize_tool(
    invocation: &ToolInvocationRecord,
    delegated_task: Option<&DelegatedTaskRow>,
) -> String {
    if invocation.tool_name == "task" {
        let display_name = delegated_task_display_name(invocation, delegated_task);

        if let Some(prompt_preview) = delegated_task.and_then(|delegated_task| {
            delegated_task
                .prompt_preview
                .as_deref()
                .filter(|prompt| !prompt.is_empty())
        }) {
            return format!("{display_name} · {prompt_preview}");
        }

        return display_name;
    }

    if let Some(path) = invocation
        .arguments
        .get("filePath")
        .or_else(|| invocation.arguments.get("path"))
        .and_then(serde_json::Value::as_str)
        && !path.trim().is_empty()
    {
        return format!("{} {}", invocation.tool_name, path);
    }

    if let Some(query) = invocation
        .arguments
        .get("query")
        .or_else(|| invocation.arguments.get("pattern"))
        .and_then(serde_json::Value::as_str)
        && !query.trim().is_empty()
    {
        return format!("{} {}", invocation.tool_name, query);
    }

    invocation.tool_name.clone()
}

fn summarize_json(value: &serde_json::Value) -> String {
    summarize_text(&value.to_string())
}

fn summarize_text(text: &str) -> String {
    let condensed = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if condensed.is_empty() {
        return "(empty)".to_string();
    }

    let mut chars = condensed.chars();
    let summary: String = chars.by_ref().take(SUMMARY_LIMIT).collect();

    if chars.next().is_some() {
        format!("{summary}...")
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use fluent_code_app::app::{AppState, AppStatus};
    use fluent_code_app::session::model::{
        Role, RunStatus, Session, TaskDelegationRecord, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, Turn,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{ConversationRow, RunMarkerKind, ToolGroupKind, derive_conversation_rows};

    #[test]
    fn derive_conversation_rows_keeps_turn_order() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("ordered turns");
        let first_turn = make_turn(run_id, Role::User, "first");
        let second_turn = make_turn(run_id, Role::Assistant, "second");

        session.turns = vec![first_turn.clone(), second_turn.clone()];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            ConversationRow::Turn(turn) if turn.content == first_turn.content
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Turn(turn) if turn.content == second_turn.content
        ));
    }

    #[test]
    fn derive_conversation_rows_inserts_reasoning_row_before_assistant_turn() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("assistant reasoning");
        let mut turn = make_turn(run_id, Role::Assistant, "answer");
        turn.reasoning = "plan".to_string();

        session.turns = vec![turn.clone()];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            ConversationRow::Reasoning(row) if row.content == "plan"
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Turn(row) if row.content == "answer"
        ));
    }

    #[test]
    fn derive_conversation_rows_interleaves_turns_and_tools_chronologically() {
        let run_id = Uuid::new_v4();
        let base = Utc::now();
        let mut session = Session::new("chronological tools");

        let mut first_turn = make_turn(run_id, Role::User, "inspect");
        first_turn.timestamp = base;

        let mut second_turn = make_turn(run_id, Role::Assistant, "working");
        second_turn.timestamp = base + Duration::seconds(3);

        let early = make_tool_invocation(
            run_id,
            Some(first_turn.id),
            "search",
            json!({"query": "PersistSession"}),
            base + Duration::seconds(1),
        );
        let later = make_tool_invocation(
            run_id,
            Some(first_turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            base + Duration::seconds(2),
        );

        session.turns = vec![first_turn.clone(), second_turn.clone()];
        session.tool_invocations = vec![later.clone(), early.clone()];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 4);
        assert!(matches!(
            &rows[0],
            ConversationRow::Turn(turn) if turn.content == first_turn.content
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Tool(tool) if tool.summary.contains("PersistSession")
        ));
        assert!(matches!(
            &rows[2],
            ConversationRow::Tool(tool) if tool.summary.contains("src/main.rs")
        ));
        assert!(matches!(
            &rows[3],
            ConversationRow::Turn(turn) if turn.content == second_turn.content
        ));
    }

    #[test]
    fn derive_conversation_rows_preserves_orphan_tools() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("orphan tools");
        let turn = make_turn(run_id, Role::User, "hello");
        let orphan = make_tool_invocation(
            run_id,
            None,
            "read",
            json!({"filePath": "README.md"}),
            Utc::now(),
        );

        session.turns = vec![turn.clone()];
        session.tool_invocations = vec![orphan.clone()];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            ConversationRow::Turn(turn_row) if turn_row.content == turn.content
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Tool(tool) if tool.summary.contains("README.md")
        ));
    }

    #[test]
    fn derive_conversation_rows_groups_same_kind_tools_for_same_turn() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("grouped tools");
        let turn = make_turn(run_id, Role::Assistant, "reading files");

        session.turns = vec![turn.clone()];
        session.tool_invocations = vec![
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "read",
                json!({"path": "src/main.rs"}),
                Utc::now(),
            ),
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "read",
                json!({"path": "src/lib.rs"}),
                Utc::now() + Duration::seconds(1),
            ),
        ];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], ConversationRow::Turn(_)));
        assert!(matches!(
            &rows[1],
            ConversationRow::ToolGroup(group)
                if group.kind == ToolGroupKind::ReadLike && group.items.len() == 2
        ));
    }

    #[test]
    fn derive_conversation_rows_does_not_group_mixed_tool_kinds() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("mixed tools");
        let turn = make_turn(run_id, Role::Assistant, "mixed work");

        session.turns = vec![turn.clone()];
        session.tool_invocations = vec![
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "read",
                json!({"path": "src/main.rs"}),
                Utc::now(),
            ),
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "search",
                json!({"query": "main"}),
                Utc::now() + Duration::seconds(1),
            ),
        ];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[0], ConversationRow::Turn(_)));
        assert!(matches!(&rows[1], ConversationRow::Tool(_)));
        assert!(matches!(&rows[2], ConversationRow::Tool(_)));
    }

    #[test]
    fn derive_conversation_rows_inserts_approval_marker_after_grouped_tool_batch() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("approval marker");
        let turn = make_turn(run_id, Role::Assistant, "checking files");
        session.turns = vec![turn.clone()];
        session.tool_invocations = vec![
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "read",
                json!({"path": "src/main.rs"}),
                Utc::now(),
            ),
            make_tool_invocation(
                run_id,
                Some(turn.id),
                "read",
                json!({"path": "src/lib.rs"}),
                Utc::now() + Duration::seconds(1),
            ),
        ];

        let mut state = AppState::new(session);
        state.active_run_id = Some(run_id);
        state.status = AppStatus::AwaitingToolApproval;

        let rows = derive_conversation_rows(&state);

        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[1], ConversationRow::ToolGroup(_)));
        assert!(matches!(
            &rows[2],
            ConversationRow::RunMarker(marker)
                if marker.kind == RunMarkerKind::AwaitingApproval
        ));
    }

    #[test]
    fn derive_conversation_rows_inserts_running_marker_for_active_run_tail() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("running marker");
        session.turns = vec![make_turn(run_id, Role::Assistant, "working")];

        let mut state = AppState::new(session);
        state.active_run_id = Some(run_id);
        state.status = AppStatus::RunningTool;

        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Running
        ));
    }

    #[test]
    fn derive_conversation_rows_inserts_completed_marker_for_terminal_run_tail() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("completed marker");
        session.turns = vec![make_turn(run_id, Role::Assistant, "done")];
        session.upsert_run(run_id, RunStatus::Completed);

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Completed
        ));
    }

    #[test]
    fn derive_conversation_rows_inserts_failed_marker_for_terminal_run_tail() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("failed marker");
        session.turns = vec![make_turn(run_id, Role::Assistant, "boom")];
        session.upsert_run(run_id, RunStatus::Failed);

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Failed
        ));
    }

    #[test]
    fn derive_conversation_rows_does_not_emit_markers_for_historical_inferred_states() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("no inferred marker");
        let turn = make_turn(run_id, Role::Assistant, "pending tool snapshot");
        session.turns = vec![turn.clone()];
        session.tool_invocations = vec![make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            Utc::now(),
        )];

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, ConversationRow::RunMarker(_)))
        );
    }

    #[test]
    fn derive_conversation_rows_summarizes_delegated_task_with_agent_and_prompt() {
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let mut session = Session::new("delegated task summary");
        let turn = make_turn(parent_run_id, Role::Assistant, "delegating now");
        let mut invocation = make_tool_invocation(
            parent_run_id,
            Some(turn.id),
            "task",
            json!({"agent": "explore", "prompt": "Inspect session persistence state"}),
            Utc::now(),
        );
        invocation.delegation = Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect session persistence state".to_string()),
            status: fluent_code_app::session::model::TaskDelegationStatus::Running,
        });
        invocation.approval_state = ToolApprovalState::Approved;
        invocation.execution_state = ToolExecutionState::Running;

        session.turns.push(turn);
        session.tool_invocations.push(invocation);
        session.upsert_run(parent_run_id, RunStatus::InProgress);
        session.upsert_run_with_parent(
            child_run_id,
            RunStatus::InProgress,
            Some(parent_run_id),
            Some(session.tool_invocations[0].id),
        );

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            &rows[1],
            ConversationRow::Tool(tool)
                if tool.display_name == "task explore"
                    && tool.summary.contains("task explore")
                    && tool.summary.contains("Inspect session persistence state")
                    && tool
                        .delegated_task
                        .as_ref()
                        .and_then(|delegated_task| delegated_task.agent_name.as_deref())
                        == Some("explore")
                    && tool
                        .delegated_task
                        .as_ref()
                        .and_then(|delegated_task| delegated_task.child_run_status)
                        == Some(RunStatus::InProgress)
        ));
    }

    #[test]
    fn derive_conversation_rows_labels_active_child_marker_as_subagent() {
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let mut session = Session::new("child marker label");
        let turn = make_turn(parent_run_id, Role::Assistant, "delegating now");
        let mut invocation = make_tool_invocation(
            parent_run_id,
            Some(turn.id),
            "task",
            json!({"agent": "explore", "prompt": "Inspect child flow"}),
            Utc::now(),
        );
        invocation.delegation = Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect child flow".to_string()),
            status: fluent_code_app::session::model::TaskDelegationStatus::Running,
        });
        invocation.approval_state = ToolApprovalState::Approved;
        invocation.execution_state = ToolExecutionState::Running;

        session.turns.push(turn);
        session.tool_invocations.push(invocation);
        session.upsert_run(parent_run_id, RunStatus::InProgress);
        session.upsert_run_with_parent(
            child_run_id,
            RunStatus::InProgress,
            Some(parent_run_id),
            Some(session.tool_invocations[0].id),
        );

        let mut state = AppState::new(session);
        state.active_run_id = Some(child_run_id);
        state.status = AppStatus::Generating;

        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Running
                    && marker.label == "running · subagent explore"
        ));
    }

    fn make_turn(run_id: uuid::Uuid, role: Role, content: &str) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            run_id,
            role,
            content: content.to_string(),
            reasoning: String::new(),
            timestamp: Utc::now(),
        }
    }

    fn make_tool_invocation(
        run_id: uuid::Uuid,
        preceding_turn_id: Option<uuid::Uuid>,
        tool_name: &str,
        arguments: serde_json::Value,
        requested_at: chrono::DateTime<Utc>,
    ) -> ToolInvocationRecord {
        ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: format!("call-{}", Uuid::new_v4()),
            tool_name: tool_name.to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments,
            preceding_turn_id,
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            delegation: None,
            requested_at,
            approved_at: None,
            completed_at: None,
        }
    }
}
