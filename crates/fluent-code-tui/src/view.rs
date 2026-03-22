use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{Role, RunStatus, ToolApprovalState, ToolExecutionState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::theme::TUI_THEME;
use crate::ui_state::UiState;

const SUMMARY_LIMIT: usize = 72;

pub fn render(frame: &mut Frame, state: &AppState, ui_state: &UiState) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(34)])
        .split(shell[1]);

    render_header(frame, shell[0], state);
    render_transcript(frame, body[0], state);
    render_sidebar(frame, body[1], state, ui_state);
    render_input(frame, shell[2], state);
    render_footer(frame, shell[3], state, ui_state);

    if ui_state.show_help_overlay {
        render_help_overlay(frame, frame.area());
    }

    if matches!(state.status, AppStatus::Idle | AppStatus::Error(_)) {
        let cursor_x = shell[2]
            .x
            .saturating_add(state.draft_input.len() as u16 + 1);
        let cursor_y = shell[2].y.saturating_add(1);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(18)])
        .split(area);

    let run_badge = match state.session.latest_run_status() {
        Some(RunStatus::InProgress) => "in progress",
        Some(RunStatus::Completed) => "completed",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Cancelled) => "cancelled",
        None => "none",
    };

    let title = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("session", TUI_THEME.label),
            Span::raw(" "),
            Span::styled(state.session.title.as_str(), TUI_THEME.title),
        ]),
        Line::from(vec![
            Span::styled("turns ", TUI_THEME.text_muted),
            Span::styled(state.session.turns.len().to_string(), TUI_THEME.text),
            Span::styled("  tools ", TUI_THEME.text_muted),
            Span::styled(
                state.session.tool_invocations.len().to_string(),
                TUI_THEME.text,
            ),
        ]),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(TUI_THEME.panel_border)
            .title(Span::styled(" fluent-code ", TUI_THEME.title)),
    );

    let run = Paragraph::new(Text::from(vec![
        Line::from(Span::styled("run", TUI_THEME.label)),
        Line::from(Span::styled(run_badge, status_style(&state.status))),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(TUI_THEME.panel_border)
            .title(Span::styled(" status ", TUI_THEME.title)),
    );

    frame.render_widget(title, header[0]);
    frame.render_widget(run, header[1]);
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState) {
    let lines = transcript_lines(state);
    let transcript_scroll = transcript_scroll_offset(&lines, area.width, area.height);

    let transcript = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border_active)
                .title(Span::styled(" conversation ", TUI_THEME.title)),
        )
        .scroll((transcript_scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(transcript, area);
}

fn render_sidebar(frame: &mut Frame, area: Rect, state: &AppState, ui_state: &UiState) {
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(area);

    let summary = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("status ", TUI_THEME.label),
            Span::styled(status_label(&state.status), status_style(&state.status)),
        ]),
        Line::from(vec![
            Span::styled("last run ", TUI_THEME.label),
            Span::styled(run_status_label(state), TUI_THEME.text),
        ]),
        Line::from(vec![
            Span::styled("active ", TUI_THEME.label),
            Span::styled(
                state
                    .active_run_id
                    .map(|id| summarize_text(&id.to_string()))
                    .unwrap_or_else(|| "none".to_string()),
                TUI_THEME.text_muted,
            ),
        ]),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(TUI_THEME.panel_border)
            .title(Span::styled(" overview ", TUI_THEME.title)),
    )
    .wrap(Wrap { trim: false });

    let tool_lines = sidebar_tool_lines(state, ui_state.show_tool_details);
    let tools = Paragraph::new(Text::from(tool_lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border)
                .title(Span::styled(
                    if ui_state.show_tool_details {
                        " tool activity · expanded "
                    } else {
                        " tool activity · compact "
                    },
                    TUI_THEME.title,
                )),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(summary, sidebar[0]);
    frame.render_widget(tools, sidebar[1]);
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let input = Paragraph::new(state.draft_input.as_str())
        .style(TUI_THEME.text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(TUI_THEME.panel_border_active)
                .title(Span::styled(" input ", TUI_THEME.title)),
        );

    frame.render_widget(input, area);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState, ui_state: &UiState) {
    let footer = Paragraph::new(footer_text(state, ui_state)).style(TUI_THEME.text_muted);
    frame.render_widget(footer, area);
}

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let overlay = centered_rect(70, 45, area);
    let help = Paragraph::new(Text::from(vec![
        Line::from(vec![Span::styled("Help", TUI_THEME.title)]),
        Line::default(),
        Line::from("F1  toggle help"),
        Line::from("F2  toggle tool detail density"),
        Line::from("Enter  send prompt / approve tools"),
        Line::from("Y  approve pending tool batch"),
        Line::from("N  deny one pending tool"),
        Line::from("Ctrl-N  new session"),
        Line::from("Esc / Ctrl-C  cancel run or quit when idle"),
    ]))
    .style(TUI_THEME.text)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(TUI_THEME.panel_border_active)
            .title(Span::styled(" keyboard shortcuts ", TUI_THEME.title)),
    )
    .wrap(Wrap { trim: false });

    frame.render_widget(help, overlay);
}

fn transcript_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.session.turns.is_empty() {
        return vec![Line::styled(
            "No messages yet. Type and press Enter to chat.",
            TUI_THEME.text_muted,
        )];
    }

    state.session.turns.iter().flat_map(render_turn).collect()
}

fn sidebar_tool_lines(state: &AppState, show_tool_details: bool) -> Vec<Line<'static>> {
    if state.session.tool_invocations.is_empty() {
        return vec![
            Line::styled("No tool activity yet.", TUI_THEME.text_muted),
            Line::default(),
            Line::styled(
                "When the assistant calls tools, they will appear here instead of breaking the main transcript flow.",
                TUI_THEME.text_muted,
            ),
        ];
    }

    state
        .session
        .tool_invocations
        .iter()
        .rev()
        .flat_map(|invocation| render_tool_summary(invocation, show_tool_details))
        .collect()
}

fn render_turn(turn: &fluent_code_app::session::model::Turn) -> Vec<Line<'static>> {
    let (label, accent_style, content_style, meta_text) = match turn.role {
        Role::User => ("YOU", TUI_THEME.user_accent, TUI_THEME.text, "request"),
        Role::Assistant => (
            "ASSISTANT",
            TUI_THEME.assistant_accent,
            TUI_THEME.text,
            "response",
        ),
        Role::System => (
            "SYSTEM",
            TUI_THEME.system_accent,
            TUI_THEME.text_muted,
            "system note",
        ),
        Role::Tool => (
            "TOOL",
            TUI_THEME.tool_accent,
            TUI_THEME.text_muted,
            "tool turn",
        ),
    };

    let content = if turn.content.trim().is_empty() {
        "(empty)".to_string()
    } else {
        turn.content.clone()
    };

    let content_lines = content
        .lines()
        .map(|line| {
            Line::from(vec![
                Span::styled("│ ", TUI_THEME.card_prefix),
                Span::styled(line.to_string(), content_style),
            ])
        })
        .collect::<Vec<_>>();

    let divider = match turn.role {
        Role::User => "└─ user",
        Role::Assistant => "└─ assistant",
        Role::System => "└─ system",
        Role::Tool => "└─ tool",
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("╭─ ", TUI_THEME.card_prefix),
            Span::styled(label, accent_style),
            Span::raw(" "),
            Span::styled(meta_text, TUI_THEME.text_muted),
        ]),
        Line::default(),
    ];

    lines.extend(content_lines);
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::styled(
        divider,
        TUI_THEME.transcript_divider,
    )]));
    lines.push(Line::default());

    lines
}

fn render_tool_summary(
    invocation: &fluent_code_app::session::model::ToolInvocationRecord,
    show_tool_details: bool,
) -> Vec<Line<'static>> {
    let (approval_label, approval_style) = approval_badge(invocation.approval_state);
    let (execution_label, execution_style) = execution_badge(invocation.execution_state);

    let mut lines = vec![Line::from(vec![
        Span::styled("● ", TUI_THEME.tool_accent),
        Span::styled(invocation.tool_name.clone(), TUI_THEME.tool_accent),
        Span::raw(" "),
        Span::styled(
            format!("· {} / {}", approval_label, execution_label),
            TUI_THEME.text_muted,
        ),
    ])];

    if show_tool_details {
        lines.push(Line::from(vec![
            Span::styled("  approval ", TUI_THEME.label),
            Span::styled(approval_label.to_string(), approval_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  execution ", TUI_THEME.label),
            Span::styled(execution_label.to_string(), execution_style),
        ]));
    }

    if show_tool_details {
        lines.push(Line::from(vec![
            Span::styled("  args ", TUI_THEME.label),
            Span::styled(summarize_json(&invocation.arguments), TUI_THEME.text_muted),
        ]));
    }

    if let Some(result) = &invocation.result {
        lines.push(Line::from(vec![
            Span::styled("  result ", TUI_THEME.label),
            Span::styled(summarize_text(result), TUI_THEME.success),
        ]));
    }

    if let Some(error) = &invocation.error {
        lines.push(Line::from(vec![
            Span::styled("  note ", TUI_THEME.label),
            Span::styled(summarize_text(error), TUI_THEME.error),
        ]));
    }

    if invocation.approval_state == ToolApprovalState::Pending {
        lines.push(Line::from(vec![
            Span::styled("  action ", TUI_THEME.label),
            Span::styled("Enter/Y approve all • N deny one", TUI_THEME.warning),
        ]));
    }

    lines.push(Line::default());
    lines
}

fn approval_badge(state: ToolApprovalState) -> (&'static str, ratatui::style::Style) {
    match state {
        ToolApprovalState::Pending => ("pending", TUI_THEME.warning),
        ToolApprovalState::Approved => ("approved", TUI_THEME.success),
        ToolApprovalState::Denied => ("denied", TUI_THEME.error),
    }
}

fn execution_badge(state: ToolExecutionState) -> (&'static str, ratatui::style::Style) {
    match state {
        ToolExecutionState::NotStarted => ("queued", TUI_THEME.text_muted),
        ToolExecutionState::Running => ("running", TUI_THEME.info),
        ToolExecutionState::Completed => ("completed", TUI_THEME.success),
        ToolExecutionState::Failed => ("failed", TUI_THEME.error),
        ToolExecutionState::Skipped => ("skipped", TUI_THEME.text_muted),
    }
}

fn status_label(status: &AppStatus) -> &'static str {
    match status {
        AppStatus::Idle => "idle",
        AppStatus::Generating => "generating",
        AppStatus::AwaitingToolApproval => "awaiting approval",
        AppStatus::RunningTool => "running tool",
        AppStatus::Error(_) => "error",
    }
}

fn status_style(status: &AppStatus) -> ratatui::style::Style {
    match status {
        AppStatus::Idle => TUI_THEME.text,
        AppStatus::Generating => TUI_THEME.info,
        AppStatus::AwaitingToolApproval => TUI_THEME.warning,
        AppStatus::RunningTool => TUI_THEME.info,
        AppStatus::Error(_) => TUI_THEME.error,
    }
}

fn run_status_label(state: &AppState) -> &'static str {
    match state.session.latest_run_status() {
        Some(RunStatus::InProgress) => "in progress",
        Some(RunStatus::Completed) => "completed",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Cancelled) => "cancelled",
        None => "none",
    }
}

fn footer_text_with_ui(state: &AppState, show_tool_details: bool) -> String {
    match &state.status {
        AppStatus::Idle => format!(
            "Enter send • F1 help • F2 details {} • Ctrl-N new • Esc/Ctrl-C quit",
            if show_tool_details {
                "expanded"
            } else {
                "compact"
            }
        ),
        AppStatus::Generating => {
            "Assistant responding • F1 help • F2 details • Esc/Ctrl-C cancel".to_string()
        }
        AppStatus::AwaitingToolApproval => {
            if let Some(invocation) = state.session.pending_tool_invocation() {
                let pending_count = state
                    .session
                    .tool_invocations
                    .iter()
                    .filter(|pending| {
                        pending.approval_state == ToolApprovalState::Pending
                            && pending.run_id == invocation.run_id
                            && pending.preceding_turn_id == invocation.preceding_turn_id
                    })
                    .count();
                format!(
                    "{pending_count} tool call(s) waiting • Enter/Y approve all • N deny one • F1 help • F2 details • Esc/Ctrl-C cancel"
                )
            } else {
                "Awaiting tool approval".to_string()
            }
        }
        AppStatus::RunningTool => {
            "Executing approved tools • F1 help • F2 details • Esc/Ctrl-C cancel run".to_string()
        }
        AppStatus::Error(error) => format!(
            "Error: {} • F1 help • F2 details • Ctrl-N new session • Esc/Ctrl-C quit",
            summarize_text(error)
        ),
    }
}

fn footer_text(state: &AppState, ui_state: &UiState) -> String {
    footer_text_with_ui(state, ui_state.show_tool_details)
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

fn transcript_scroll_offset(lines: &[Line<'_>], area_width: u16, area_height: u16) -> u16 {
    let inner_width = area_width.saturating_sub(2).max(1) as usize;
    let visible_height = area_height.saturating_sub(2) as usize;

    if visible_height == 0 {
        return 0;
    }

    let wrapped_line_count = lines
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let char_count = text.chars().count().max(1);
            char_count.div_ceil(inner_width)
        })
        .sum::<usize>();

    wrapped_line_count.saturating_sub(visible_height) as u16
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::{footer_text_with_ui, transcript_scroll_offset};
    use fluent_code_app::app::AppState;
    use fluent_code_app::session::model::Session;

    #[test]
    fn transcript_scroll_offset_stays_at_top_when_content_fits() {
        let lines = vec![Line::from("short"), Line::from("content")];

        assert_eq!(transcript_scroll_offset(&lines, 20, 8), 0);
    }

    #[test]
    fn transcript_scroll_offset_tracks_bottom_for_growing_content() {
        let lines = vec![
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
        ];

        assert_eq!(transcript_scroll_offset(&lines, 10, 5), 1);
    }

    #[test]
    fn footer_text_reports_tool_detail_mode() {
        let state = AppState::new(Session::new("ui state test"));

        assert!(footer_text_with_ui(&state, false).contains("compact"));
        assert!(footer_text_with_ui(&state, true).contains("expanded"));
    }
}
