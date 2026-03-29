use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{
    ReplaySequence, Role, RunStatus, RunTerminalStopReason, Session, ToolApprovalState,
    ToolExecutionState, ToolInvocationRecord, ToolSource,
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
    Interrupted,
}

#[derive(Debug, Clone)]
pub(crate) struct RunMarkerRow {
    pub(crate) kind: RunMarkerKind,
    pub(crate) label: String,
}

pub(crate) fn derive_conversation_rows(state: &AppState) -> Vec<ConversationRow> {
    let session = &state.session;

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

const PRIORITY_REASONING: u8 = 0;
const PRIORITY_TURN: u8 = 1;
const PRIORITY_TOOL: u8 = 2;

enum TimelineKind {
    Reasoning(usize),
    Turn(usize),
    Tool(usize),
}

struct TimelineEntry {
    sort_key: (ReplaySequence, u8),
    kind: TimelineKind,
}

fn build_timeline(state: &AppState) -> Vec<TimelineEntry> {
    let session = &state.session;
    let mut entries = Vec::new();
    for (i, turn) in session.turns.iter().enumerate() {
        if matches!(turn.role, Role::Assistant) && !turn.reasoning.is_empty() {
            entries.push(TimelineEntry {
                sort_key: (turn.sequence_number, PRIORITY_REASONING),
                kind: TimelineKind::Reasoning(i),
            });
        }

        entries.push(TimelineEntry {
            sort_key: (turn.sequence_number, PRIORITY_TURN),
            kind: TimelineKind::Turn(i),
        });
    }

    for (i, invocation) in session.tool_invocations.iter().enumerate() {
        entries.push(TimelineEntry {
            sort_key: (invocation.sequence_number, PRIORITY_TOOL),
            kind: TimelineKind::Tool(i),
        });
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

    latest_root_run_marker(state)
}

fn latest_root_run_marker(state: &AppState) -> Option<RunMarkerRow> {
    let latest_root_run = state
        .session
        .runs
        .iter()
        .filter(|run| run.parent_run_id.is_none())
        .max_by_key(|run| run.latest_replay_sequence())?;
    let stop_reason = latest_root_run
        .terminal_stop_reason
        .or_else(|| latest_root_run.status.default_terminal_stop_reason())?;

    Some(match stop_reason {
        RunTerminalStopReason::Completed => RunMarkerRow {
            kind: RunMarkerKind::Completed,
            label: "completed".to_string(),
        },
        RunTerminalStopReason::Failed => RunMarkerRow {
            kind: RunMarkerKind::Failed,
            label: "failed".to_string(),
        },
        RunTerminalStopReason::Cancelled => RunMarkerRow {
            kind: RunMarkerKind::Cancelled,
            label: "cancelled".to_string(),
        },
        RunTerminalStopReason::Interrupted => RunMarkerRow {
            kind: RunMarkerKind::Interrupted,
            label: "interrupted".to_string(),
        },
    })
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
        Role, RunStatus, RunTerminalStopReason, Session, TaskDelegationRecord, ToolApprovalState,
        ToolExecutionState, ToolInvocationRecord, ToolSource, Turn,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{ConversationRow, RunMarkerKind, ToolGroupKind, derive_conversation_rows};

    #[test]
    fn derive_conversation_rows_keeps_turn_order() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("ordered turns");
        let mut first_turn = make_turn(run_id, Role::User, "first");
        first_turn.sequence_number = 1;
        let mut second_turn = make_turn(run_id, Role::Assistant, "second");
        second_turn.sequence_number = 2;

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
        turn.sequence_number = 1;

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
    fn derive_conversation_rows_interleaves_turns_and_tools_by_replay_sequence() {
        let run_id = Uuid::new_v4();
        let base = Utc::now();
        let mut session = Session::new("chronological tools");

        let mut first_turn = make_turn(run_id, Role::User, "inspect");
        first_turn.timestamp = base + Duration::seconds(3);
        first_turn.sequence_number = 1;

        let mut second_turn = make_turn(run_id, Role::Assistant, "working");
        second_turn.timestamp = base - Duration::seconds(10);
        second_turn.sequence_number = 4;

        let mut early = make_tool_invocation(
            run_id,
            Some(first_turn.id),
            "search",
            json!({"query": "PersistSession"}),
            base - Duration::seconds(20),
        );
        early.sequence_number = 2;
        let mut later = make_tool_invocation(
            run_id,
            Some(first_turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            base - Duration::seconds(30),
        );
        later.sequence_number = 3;

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
        let mut turn = make_turn(run_id, Role::User, "hello");
        turn.sequence_number = 1;
        let mut orphan = make_tool_invocation(
            run_id,
            None,
            "read",
            json!({"filePath": "README.md"}),
            Utc::now(),
        );
        orphan.sequence_number = 2;

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
        let mut turn = make_turn(run_id, Role::Assistant, "reading files");
        turn.sequence_number = 1;

        session.turns = vec![turn.clone()];
        let mut first_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            Utc::now(),
        );
        first_invocation.sequence_number = 2;
        let mut second_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/lib.rs"}),
            Utc::now() + Duration::seconds(1),
        );
        second_invocation.sequence_number = 3;
        session.tool_invocations = vec![first_invocation, second_invocation];

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
        let mut turn = make_turn(run_id, Role::Assistant, "mixed work");
        turn.sequence_number = 1;

        session.turns = vec![turn.clone()];
        let mut first_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            Utc::now(),
        );
        first_invocation.sequence_number = 2;
        let mut second_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "search",
            json!({"query": "main"}),
            Utc::now() + Duration::seconds(1),
        );
        second_invocation.sequence_number = 3;
        session.tool_invocations = vec![first_invocation, second_invocation];

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
        let mut turn = make_turn(run_id, Role::Assistant, "checking files");
        turn.sequence_number = 1;
        session.turns = vec![turn.clone()];
        let mut first_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            Utc::now(),
        );
        first_invocation.sequence_number = 2;
        let mut second_invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/lib.rs"}),
            Utc::now() + Duration::seconds(1),
        );
        second_invocation.sequence_number = 3;
        session.tool_invocations = vec![first_invocation, second_invocation];

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
        let mut turn = make_turn(run_id, Role::Assistant, "working");
        turn.sequence_number = 1;
        session.turns = vec![turn];

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
        let mut turn = make_turn(run_id, Role::Assistant, "done");
        turn.sequence_number = 1;
        session.turns = vec![turn];
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
        let mut turn = make_turn(run_id, Role::Assistant, "boom");
        turn.sequence_number = 1;
        session.turns = vec![turn];
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
    fn derive_conversation_rows_inserts_interrupted_marker_for_terminal_run_tail() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("interrupted marker");
        let mut turn = make_turn(run_id, Role::Assistant, "stopped");
        turn.sequence_number = 1;
        session.turns = vec![turn];
        session.upsert_run_with_stop_reason(
            run_id,
            RunStatus::Failed,
            Some(RunTerminalStopReason::Interrupted),
        );

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Interrupted && marker.label == "interrupted"
        ));
    }

    #[test]
    fn derive_conversation_rows_prefers_latest_root_terminal_marker_over_child_run_status() {
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let mut session = Session::new("root marker wins");
        let mut parent_turn = make_turn(parent_run_id, Role::Assistant, "parent failed");
        parent_turn.sequence_number = 1;
        session.turns = vec![parent_turn];
        session.upsert_run(parent_run_id, RunStatus::Failed);
        session.upsert_run_with_parent(
            child_run_id,
            RunStatus::Completed,
            Some(parent_run_id),
            None,
        );

        let state = AppState::new(session);
        let rows = derive_conversation_rows(&state);

        assert!(matches!(
            rows.last(),
            Some(ConversationRow::RunMarker(marker))
                if marker.kind == RunMarkerKind::Failed && marker.label == "failed"
        ));
    }

    #[test]
    fn derive_conversation_rows_does_not_emit_markers_for_historical_inferred_states() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("no inferred marker");
        let mut turn = make_turn(run_id, Role::Assistant, "pending tool snapshot");
        turn.sequence_number = 1;
        session.turns = vec![turn.clone()];
        let mut invocation = make_tool_invocation(
            run_id,
            Some(turn.id),
            "read",
            json!({"path": "src/main.rs"}),
            Utc::now(),
        );
        invocation.sequence_number = 2;
        session.tool_invocations = vec![invocation];

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
        let mut turn = make_turn(parent_run_id, Role::Assistant, "delegating now");
        turn.sequence_number = 1;
        let mut invocation = make_tool_invocation(
            parent_run_id,
            Some(turn.id),
            "task",
            json!({"agent": "explore", "prompt": "Inspect session persistence state"}),
            Utc::now(),
        );
        invocation.sequence_number = 2;
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
        let mut turn = make_turn(parent_run_id, Role::Assistant, "delegating now");
        turn.sequence_number = 1;
        let mut invocation = make_tool_invocation(
            parent_run_id,
            Some(turn.id),
            "task",
            json!({"agent": "explore", "prompt": "Inspect child flow"}),
            Utc::now(),
        );
        invocation.sequence_number = 2;
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
            sequence_number: 1,
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
            sequence_number: 1,
            requested_at,
            approved_at: None,
            completed_at: None,
        }
    }
}
