use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{Role, RunStatus, ToolApprovalState, ToolExecutionState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

const SUMMARY_LIMIT: usize = 72;

pub fn render(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let mut lines = if state.session.turns.is_empty() {
        vec![Line::styled(
            "No messages yet. Type and press Enter to chat.",
            muted_style(),
        )]
    } else {
        state.session.turns.iter().flat_map(render_turn).collect()
    };

    for invocation in &state.session.tool_invocations {
        lines.extend(render_tool_invocation(invocation));
    }

    let transcript = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(state.session.title.as_str()),
        )
        .wrap(Wrap { trim: false });

    let input = Paragraph::new(state.draft_input.as_str())
        .block(Block::default().borders(Borders::ALL).title("Input"));

    let status_text = match &state.status {
        AppStatus::Idle => {
            "Enter: send prompt | Ctrl-N: new session | Esc/Ctrl-C: quit".to_string()
        }
        AppStatus::Generating => "Generating assistant response... Esc/Ctrl-C: cancel".to_string(),
        AppStatus::AwaitingToolApproval => {
            if let Some(invocation) = state.session.pending_tool_invocation() {
                format!(
                    "Tool approval: {} | args: {} | Enter/Y approve, N deny, Esc/Ctrl-C cancel",
                    invocation.tool_name,
                    summarize_json(&invocation.arguments)
                )
            } else {
                "Awaiting tool approval".to_string()
            }
        }
        AppStatus::RunningTool => "Running approved tool... Esc/Ctrl-C: cancel run".to_string(),
        AppStatus::Error(error) => {
            format!("Provider error: {error} | Ctrl-N: new session | Esc/Ctrl-C: quit")
        }
    };

    let run_status_text = match state.session.latest_run_status() {
        Some(RunStatus::InProgress) => "run: in progress",
        Some(RunStatus::Completed) => "run: completed",
        Some(RunStatus::Failed) => "run: failed",
        Some(RunStatus::Cancelled) => "run: cancelled",
        None => "run: none",
    };

    let status = Paragraph::new(format!("{status_text} | {run_status_text}"))
        .style(Style::default().fg(Color::DarkGray));

    frame.render_widget(transcript, chunks[0]);
    frame.render_widget(input, chunks[1]);
    frame.render_widget(status, chunks[2]);

    if matches!(state.status, AppStatus::Idle | AppStatus::Error(_)) {
        let cursor_x = chunks[1]
            .x
            .saturating_add(state.draft_input.len() as u16 + 1);
        let cursor_y = chunks[1].y.saturating_add(1);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_turn(turn: &fluent_code_app::session::model::Turn) -> Vec<Line<'static>> {
    let (prefix, prefix_style, content_style) = match turn.role {
        Role::User => ("[you]", Style::default().fg(Color::Cyan), Style::default()),
        Role::Assistant => (
            "[assistant]",
            Style::default().fg(Color::Green),
            Style::default(),
        ),
        Role::System => (
            "[system]",
            Style::default().fg(Color::Yellow),
            Style::default().fg(Color::Gray),
        ),
        Role::Tool => (
            "[tool-turn]",
            Style::default().fg(Color::Magenta),
            Style::default().fg(Color::Gray),
        ),
    };

    let content = if turn.content.trim().is_empty() {
        "(empty)".to_string()
    } else {
        turn.content.clone()
    };

    vec![
        Line::from(vec![
            Span::styled(
                prefix.to_string(),
                prefix_style.add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(content, content_style),
        ]),
        Line::default(),
    ]
}

fn render_tool_invocation(
    invocation: &fluent_code_app::session::model::ToolInvocationRecord,
) -> Vec<Line<'static>> {
    let (approval_label, approval_style) = approval_badge(invocation.approval_state);
    let (execution_label, execution_style) = execution_badge(invocation.execution_state);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "[tool] ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                invocation.tool_name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(approval_label.to_string(), approval_style),
            Span::raw("  "),
            Span::styled(execution_label.to_string(), execution_style),
        ]),
        Line::styled(format!("  call {}", invocation.tool_call_id), muted_style()),
        Line::from(vec![
            Span::styled("  args: ", label_style()),
            Span::styled(
                summarize_json(&invocation.arguments),
                Style::default().fg(Color::Gray),
            ),
        ]),
    ];

    if let Some(result) = &invocation.result {
        lines.push(Line::from(vec![
            Span::styled("  result: ", label_style()),
            Span::styled(summarize_text(result), Style::default().fg(Color::Green)),
        ]));
    }

    if let Some(error) = &invocation.error {
        lines.push(Line::from(vec![
            Span::styled("  note: ", label_style()),
            Span::styled(summarize_text(error), Style::default().fg(Color::Red)),
        ]));
    }

    if invocation.approval_state == ToolApprovalState::Pending {
        lines.push(Line::from(vec![
            Span::styled("  action: ", label_style()),
            Span::styled(
                "Enter/Y approve, N deny, Esc/Ctrl-C cancel run",
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }

    lines.push(Line::default());
    lines
}

fn approval_badge(state: ToolApprovalState) -> (&'static str, Style) {
    match state {
        ToolApprovalState::Pending => (
            "approval pending",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        ToolApprovalState::Approved => (
            "approved",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ToolApprovalState::Denied => (
            "denied",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

fn execution_badge(state: ToolExecutionState) -> (&'static str, Style) {
    match state {
        ToolExecutionState::NotStarted => ("queued", muted_style()),
        ToolExecutionState::Running => (
            "running",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        ToolExecutionState::Completed => (
            "completed",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ToolExecutionState::Failed => (
            "failed",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        ToolExecutionState::Skipped => (
            "skipped",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

fn label_style() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().fg(Color::DarkGray)
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
