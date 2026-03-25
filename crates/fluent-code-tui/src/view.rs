use fluent_code_app::app::{AppState, AppStatus};
use fluent_code_app::session::model::{Role, RunStatus, ToolApprovalState, ToolExecutionState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::conversation::{
    ConversationRow, DelegatedTaskRow, RunMarkerKind, RunMarkerRow, ToolGroupKind, ToolGroupRow,
    ToolRow, TurnRow, derive_conversation_rows,
};
use crate::markdown_render::{render_markdown_lines, render_streaming_markdown_lines};
use crate::theme::TUI_THEME;
use crate::ui_state::{LayoutMode, UiState};

const SUMMARY_LIMIT: usize = 72;
const TOOL_PREVIEW_LINE_LIMIT: usize = 3;
const INLINE_TOOL_PREFIX: &str = "  ├─ tool ";
const INLINE_TOOL_DETAIL_PREFIX: &str = "  │   ";
const GROUP_PREFIX: &str = "  ├─ ";
const GROUP_ITEM_PREFIX: &str = "  │   • ";
const GROUP_DETAIL_PREFIX: &str = "  │     ";
const RUN_MARKER_PREFIX: &str = "  · run ";
const SIDEBAR_HEIGHT: u16 = 18;
const SIDEBAR_WIDTH: u16 = 36;

pub fn render(frame: &mut Frame, state: &AppState, ui_state: &UiState) {
    let (header_area, body_area, input_area, footer_area) = shell_areas(frame.area());
    let (transcript_area, sidebar_area) = body_areas(body_area, ui_state.layout_mode);

    render_header(frame, header_area, state);
    render_transcript(frame, transcript_area, state, ui_state);
    render_sidebar(frame, sidebar_area, state, ui_state);
    render_input(frame, input_area, state);
    render_footer(frame, footer_area, state, ui_state);

    if ui_state.show_help_overlay {
        render_help_overlay(frame, frame.area());
    }

    if matches!(state.status, AppStatus::Idle | AppStatus::Error(_)) {
        let cursor_x = input_area
            .x
            .saturating_add(state.draft_input.len() as u16 + 1);
        let cursor_y = input_area.y.saturating_add(1);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

pub(crate) fn transcript_area(area: Rect, layout_mode: LayoutMode) -> Rect {
    let (_, body_area, _, _) = shell_areas(area);
    body_areas(body_area, layout_mode).0
}

fn shell_areas(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    (shell[0], shell[1], shell[2], shell[3])
}

fn body_areas(area: Rect, layout_mode: LayoutMode) -> (Rect, Rect) {
    let body = Layout::default()
        .direction(match layout_mode {
            LayoutMode::SideBySide => Direction::Horizontal,
            LayoutMode::Stacked => Direction::Vertical,
        })
        .constraints(match layout_mode {
            LayoutMode::SideBySide => [Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)],
            LayoutMode::Stacked => [Constraint::Min(1), Constraint::Length(SIDEBAR_HEIGHT)],
        })
        .split(area);

    (body[0], body[1])
}

fn header_areas(area: Rect) -> (Rect, Rect) {
    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(18)])
        .split(area);

    (header[0], header[1])
}

fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let (title_area, run_area) = header_areas(area);

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

    frame.render_widget(title, title_area);
    frame.render_widget(run, run_area);
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState, ui_state: &UiState) {
    let lines = conversation_lines(state, ui_state.show_tool_details);
    let transcript_scroll = resolve_transcript_scroll(
        &lines,
        area.width,
        area.height,
        ui_state.transcript_follow_tail,
        ui_state.transcript_scroll_top,
    );

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
        .constraints([Constraint::Length(9), Constraint::Min(1)])
        .split(area);
    let overview_context = active_run_context(state);
    let active_badge = overview_context
        .active_run_label
        .clone()
        .unwrap_or_else(|| "none".to_string());

    let summary = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("status ", TUI_THEME.label),
            Span::styled(status_label(&state.status), status_style(&state.status)),
        ]),
        Line::from(vec![
            Span::styled("focus ", TUI_THEME.label),
            Span::styled(overview_context.focus_label, overview_context.focus_style),
        ]),
        Line::from(vec![
            Span::styled("last run ", TUI_THEME.label),
            Span::styled(run_status_label(state), TUI_THEME.text),
        ]),
        Line::from(vec![
            Span::styled("active ", TUI_THEME.label),
            Span::styled(active_badge, overview_context.active_run_style),
        ]),
        if let Some(task_label) = overview_context.task_label {
            Line::from(vec![
                Span::styled("task ", TUI_THEME.label),
                Span::styled(task_label, TUI_THEME.operational_label),
            ])
        } else {
            Line::default()
        },
        Line::from(vec![
            Span::styled("plugins ", TUI_THEME.label),
            Span::styled(
                state.plugin_load_snapshot.plugin_count().to_string(),
                TUI_THEME.info,
            ),
            Span::styled("  warnings ", TUI_THEME.label),
            Span::styled(
                state.plugin_load_snapshot.warning_count().to_string(),
                if state.plugin_load_snapshot.warning_count() == 0 {
                    TUI_THEME.text
                } else {
                    TUI_THEME.warning
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("loaded ", TUI_THEME.label),
            Span::styled(
                summarize_plugin_names(&state.plugin_load_snapshot),
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
                        " operations · expanded "
                    } else {
                        " operations · compact "
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
        Line::from("F3  toggle transcript layout"),
        Line::from("↑/↓/PgUp/PgDn/Home/End  transcript navigation"),
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

pub(crate) fn conversation_lines(state: &AppState, show_tool_details: bool) -> Vec<Line<'static>> {
    let rows = derive_conversation_rows(state);

    if rows.is_empty() {
        return vec![Line::styled(
            "No messages yet. Type and press Enter to chat.",
            TUI_THEME.text_muted,
        )];
    }

    rows.iter()
        .flat_map(|row| render_row(row, show_tool_details))
        .collect()
}

fn render_row(row: &ConversationRow, show_tool_details: bool) -> Vec<Line<'static>> {
    match row {
        ConversationRow::Turn(turn) => render_turn_row(turn),
        ConversationRow::Tool(tool) => render_tool_row(tool, show_tool_details),
        ConversationRow::ToolGroup(group) => render_tool_group_row(group, show_tool_details),
        ConversationRow::RunMarker(marker) => render_run_marker_row(marker),
    }
}

fn sidebar_tool_lines(state: &AppState, show_tool_details: bool) -> Vec<Line<'static>> {
    let pending_count = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| invocation.approval_state == ToolApprovalState::Pending)
        .count();
    let running_count = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.approval_state == ToolApprovalState::Approved
                && invocation.execution_state == ToolExecutionState::Running
        })
        .count();
    let completed_count = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| invocation.execution_state == ToolExecutionState::Completed)
        .count();
    let failed_count = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.approval_state == ToolApprovalState::Denied
                || invocation.execution_state == ToolExecutionState::Failed
        })
        .count();
    let delegated_invocations = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| invocation.tool_name == "task" && invocation.child_run_id.is_some())
        .count();
    let delegated_running = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.tool_name == "task"
                && invocation.child_run_id.is_some()
                && invocation.execution_state == ToolExecutionState::Running
        })
        .count();
    let delegated_terminal = state
        .session
        .tool_invocations
        .iter()
        .filter(|invocation| {
            invocation.tool_name == "task"
                && invocation.child_run_id.is_some()
                && matches!(
                    invocation.execution_state,
                    ToolExecutionState::Completed
                        | ToolExecutionState::Failed
                        | ToolExecutionState::Skipped
                )
        })
        .count();

    let mut lines = sidebar_plugin_lines(state, show_tool_details);

    if !lines.is_empty() {
        lines.push(Line::default());
    }

    lines.extend(vec![
        Line::from(vec![
            Span::styled("pending ", TUI_THEME.label),
            Span::styled(pending_count.to_string(), TUI_THEME.warning),
        ]),
        Line::from(vec![
            Span::styled("running ", TUI_THEME.label),
            Span::styled(running_count.to_string(), TUI_THEME.info),
        ]),
        Line::from(vec![
            Span::styled("completed ", TUI_THEME.label),
            Span::styled(completed_count.to_string(), TUI_THEME.success),
        ]),
        Line::from(vec![
            Span::styled("attention ", TUI_THEME.label),
            Span::styled(failed_count.to_string(), TUI_THEME.error),
        ]),
        Line::from(vec![
            Span::styled("delegated ", TUI_THEME.label),
            Span::styled(
                delegated_invocations.to_string(),
                TUI_THEME.operational_label,
            ),
            Span::styled("  running ", TUI_THEME.label),
            Span::styled(delegated_running.to_string(), TUI_THEME.info),
            Span::styled("  done ", TUI_THEME.label),
            Span::styled(delegated_terminal.to_string(), TUI_THEME.success),
        ]),
        Line::default(),
    ]);

    if state.session.tool_invocations.is_empty() {
        lines.push(Line::styled("No tool activity yet.", TUI_THEME.text_muted));
        lines.push(Line::default());
        lines.push(Line::styled(
            "Inline tool activity now appears in the main transcript. This panel stays as an operations overview.",
            TUI_THEME.text_muted,
        ));
        return lines;
    }

    if let Some(latest) = state
        .session
        .tool_invocations
        .iter()
        .max_by_key(|invocation| {
            invocation
                .completed_at
                .or(invocation.approved_at)
                .unwrap_or(invocation.requested_at)
        })
    {
        let (approval_label, _) = approval_badge(latest.approval_state);
        let (execution_label, _) = execution_badge(latest.execution_state);

        lines.push(Line::from(vec![
            Span::styled("latest ", TUI_THEME.label),
            Span::styled(tool_summary_label(state, latest), TUI_THEME.tool_accent),
        ]));
        lines.push(Line::from(vec![
            Span::styled("status ", TUI_THEME.label),
            Span::styled(
                format!("{} / {}", approval_label, execution_label),
                TUI_THEME.text_muted,
            ),
        ]));

        if let Some(delegated_task) = delegated_task_from_invocation(state, latest) {
            if let Some(child_status) = delegated_task.child_run_status {
                lines.push(Line::from(vec![
                    Span::styled("child ", TUI_THEME.label),
                    Span::styled(
                        run_status_text(child_status),
                        run_status_style(child_status),
                    ),
                ]));
            }

            if show_tool_details {
                if let Some(prompt_preview) = delegated_task.prompt_preview {
                    lines.push(Line::from(vec![
                        Span::styled("prompt ", TUI_THEME.label),
                        Span::styled(prompt_preview, TUI_THEME.text_muted),
                    ]));
                }

                if let Some(child_run_id) = delegated_task.child_run_id {
                    lines.push(Line::from(vec![
                        Span::styled("child id ", TUI_THEME.label),
                        Span::styled(
                            summarize_text(&child_run_id.to_string()),
                            TUI_THEME.text_muted,
                        ),
                    ]));
                }
            }
        }

        if show_tool_details {
            lines.push(Line::from(vec![
                Span::styled("args ", TUI_THEME.label),
                Span::styled(summarize_json(&latest.arguments), TUI_THEME.text_muted),
            ]));

            if let Some(result) = &latest.result {
                lines.push(Line::from(vec![
                    Span::styled("result ", TUI_THEME.label),
                    Span::styled(summarize_text(result), TUI_THEME.success),
                ]));
            }

            if let Some(error) = &latest.error {
                lines.push(Line::from(vec![
                    Span::styled("note ", TUI_THEME.label),
                    Span::styled(summarize_text(error), TUI_THEME.error),
                ]));
            }
        }
    }

    lines
}

fn sidebar_plugin_lines(state: &AppState, show_tool_details: bool) -> Vec<Line<'static>> {
    let snapshot = &state.plugin_load_snapshot;
    let mut lines = vec![Line::from(vec![
        Span::styled("plugins ", TUI_THEME.label),
        Span::styled(snapshot.plugin_count().to_string(), TUI_THEME.info),
        Span::styled("  warnings ", TUI_THEME.label),
        Span::styled(
            snapshot.warning_count().to_string(),
            if snapshot.warning_count() == 0 {
                TUI_THEME.text
            } else {
                TUI_THEME.warning
            },
        ),
    ])];

    if snapshot.accepted_plugins.is_empty() {
        lines.push(Line::styled("No plugins loaded.", TUI_THEME.text_muted));
    } else if show_tool_details {
        for plugin in &snapshot.accepted_plugins {
            lines.push(Line::from(vec![
                Span::styled("plugin ", TUI_THEME.label),
                Span::styled(plugin.name.clone(), TUI_THEME.operational_label),
                Span::raw(" "),
                Span::styled(format!("v{}", plugin.version), TUI_THEME.operational_text),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  scope ", TUI_THEME.text_muted),
                Span::styled(format_discovery_scope(plugin.scope), TUI_THEME.text),
                Span::styled("  id ", TUI_THEME.text_muted),
                Span::styled(plugin.id.clone(), TUI_THEME.text_muted),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  tools ", TUI_THEME.text_muted),
                Span::styled(
                    format!("{} ({})", plugin.tool_names.join(", "), plugin.tool_count()),
                    TUI_THEME.operational_text,
                ),
            ]));

            if let Some(description) = &plugin.description {
                lines.extend(format_preview_lines(
                    "  · ",
                    Some("about "),
                    description,
                    TUI_THEME.text_muted,
                    TUI_THEME.text_muted,
                    2,
                ));
            }
        }
    } else {
        for plugin in &snapshot.accepted_plugins {
            lines.push(Line::from(vec![
                Span::styled("• ", TUI_THEME.operational_prefix),
                Span::styled(plugin.name.clone(), TUI_THEME.operational_label),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} tool{}",
                        plugin.tool_count(),
                        if plugin.tool_count() == 1 { "" } else { "s" }
                    ),
                    TUI_THEME.text_muted,
                ),
            ]));
        }
    }

    if snapshot.warnings.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("warnings ", TUI_THEME.label),
            Span::styled("none", TUI_THEME.text_muted),
        ]));
    } else if show_tool_details {
        lines.push(Line::from(vec![
            Span::styled("warnings ", TUI_THEME.label),
            Span::styled(snapshot.warning_count().to_string(), TUI_THEME.warning),
        ]));
        for warning in &snapshot.warnings {
            lines.extend(format_preview_lines(
                "! ",
                None,
                warning,
                TUI_THEME.warning,
                TUI_THEME.warning,
                2,
            ));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("warning ", TUI_THEME.label),
            Span::styled(summarize_text(&snapshot.warnings[0]), TUI_THEME.warning),
        ]));
    }

    lines
}

fn summarize_plugin_names(snapshot: &fluent_code_app::plugin::PluginLoadSnapshot) -> String {
    if snapshot.accepted_plugins.is_empty() {
        return "none".to_string();
    }

    summarize_text(
        &snapshot
            .accepted_plugins
            .iter()
            .map(|plugin| plugin.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn format_discovery_scope(scope: fluent_code_app::plugin::DiscoveryScope) -> &'static str {
    match scope {
        fluent_code_app::plugin::DiscoveryScope::Global => "global",
        fluent_code_app::plugin::DiscoveryScope::Project => "project",
    }
}

fn render_turn_row(turn: &TurnRow) -> Vec<Line<'static>> {
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

    let content_lines = if turn.is_streaming {
        format_streaming_turn_content_lines(&content, content_style)
    } else {
        format_turn_content_lines(&content, content_style)
    };

    let divider = match turn.role {
        Role::User => "╰─ user",
        Role::Assistant => "╰─ assistant",
        Role::System => "╰─ system",
        Role::Tool => "╰─ tool",
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

fn render_tool_row(tool: &ToolRow, show_tool_details: bool) -> Vec<Line<'static>> {
    let (approval_label, approval_style) = approval_badge(tool.approval_state);
    let (execution_label, execution_style) = execution_badge(tool.execution_state);
    let provenance = if show_tool_details {
        tool.provenance_expanded.as_ref()
    } else {
        tool.provenance_compact.as_ref()
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(INLINE_TOOL_PREFIX, TUI_THEME.operational_prefix),
        Span::styled(tool.display_name.clone(), TUI_THEME.operational_label),
        if let Some(provenance) = provenance {
            Span::styled(format!(" · {provenance}"), TUI_THEME.tool_accent)
        } else {
            Span::raw(String::new())
        },
        Span::raw(" "),
        Span::styled("· ", TUI_THEME.operational_prefix),
        Span::styled(approval_label.to_string(), approval_style),
        Span::styled(" / ", TUI_THEME.text_muted),
        Span::styled(execution_label.to_string(), execution_style),
    ])];

    lines.extend(format_preview_lines(
        INLINE_TOOL_DETAIL_PREFIX,
        None,
        &tool.summary,
        TUI_THEME.operational_text,
        TUI_THEME.text_muted,
        1,
    ));

    if let Some(delegated_task) = &tool.delegated_task
        && show_tool_details
    {
        if let Some(child_status) = delegated_task.child_run_status {
            lines.push(Line::from(vec![
                Span::styled("  │   child ", TUI_THEME.text_muted),
                Span::styled(
                    run_status_text(child_status),
                    run_status_style(child_status),
                ),
            ]));
        }

        if let Some(child_run_id) = delegated_task.child_run_id {
            lines.push(Line::from(vec![
                Span::styled("  │   child id ", TUI_THEME.text_muted),
                Span::styled(
                    summarize_text(&child_run_id.to_string()),
                    TUI_THEME.operational_text,
                ),
            ]));
        }

        if let Some(prompt_preview) = delegated_task.prompt_preview.as_deref() {
            lines.extend(format_preview_lines(
                INLINE_TOOL_DETAIL_PREFIX,
                Some("prompt "),
                prompt_preview,
                TUI_THEME.operational_text,
                TUI_THEME.text_muted,
                2,
            ));
        }
    }

    if show_tool_details {
        lines.push(Line::from(vec![
            Span::styled("  │   approval ", TUI_THEME.text_muted),
            Span::styled(approval_label.to_string(), approval_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  │   execution ", TUI_THEME.text_muted),
            Span::styled(execution_label.to_string(), execution_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  │   args ", TUI_THEME.text_muted),
            Span::styled(tool.arguments_preview.clone(), TUI_THEME.operational_text),
        ]));
    }

    if let Some(result) = &tool.result_preview
        && show_tool_details
    {
        lines.extend(format_preview_lines(
            INLINE_TOOL_DETAIL_PREFIX,
            Some("result "),
            result,
            TUI_THEME.success,
            TUI_THEME.text_muted,
            TOOL_PREVIEW_LINE_LIMIT,
        ));
    }

    if let Some(error) = &tool.error_preview {
        lines.extend(format_preview_lines(
            INLINE_TOOL_DETAIL_PREFIX,
            Some("note "),
            error,
            TUI_THEME.error,
            TUI_THEME.text_muted,
            if show_tool_details {
                TOOL_PREVIEW_LINE_LIMIT
            } else {
                1
            },
        ));
    }

    if tool.approval_state == ToolApprovalState::Pending {
        lines.push(Line::from(vec![
            Span::styled("  │   action ", TUI_THEME.text_muted),
            Span::styled("Enter/Y approve all • N deny one", TUI_THEME.warning),
        ]));
    }

    lines.push(Line::default());
    lines
}

fn render_tool_group_row(group: &ToolGroupRow, show_tool_details: bool) -> Vec<Line<'static>> {
    let label = match group.kind {
        ToolGroupKind::ReadLike => "read batch",
        ToolGroupKind::SearchLike => "search batch",
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(GROUP_PREFIX, TUI_THEME.operational_prefix),
        Span::styled(label, TUI_THEME.operational_label),
        Span::raw(" "),
        Span::styled(format!("({})", group.items.len()), TUI_THEME.text_muted),
    ])];

    for item in &group.items {
        let (approval_label, _) = approval_badge(item.approval_state);
        let (execution_label, _) = execution_badge(item.execution_state);
        let provenance = if show_tool_details {
            item.provenance_expanded.as_ref()
        } else {
            item.provenance_compact.as_ref()
        };

        lines.push(Line::from(vec![
            Span::styled(GROUP_ITEM_PREFIX, TUI_THEME.operational_prefix),
            Span::styled(item.summary.clone(), TUI_THEME.operational_text),
            if let Some(provenance) = provenance {
                Span::styled(format!(" · {provenance}"), TUI_THEME.tool_accent)
            } else {
                Span::raw(String::new())
            },
            Span::raw(" "),
            Span::styled("· ", TUI_THEME.operational_prefix),
            Span::styled(approval_label.to_string(), TUI_THEME.text_muted),
            Span::styled(" / ", TUI_THEME.text_muted),
            Span::styled(execution_label.to_string(), TUI_THEME.text_muted),
        ]));

        if show_tool_details {
            if let Some(result) = &item.result_preview {
                lines.extend(format_preview_lines(
                    GROUP_DETAIL_PREFIX,
                    Some("result "),
                    result,
                    TUI_THEME.success,
                    TUI_THEME.text_muted,
                    TOOL_PREVIEW_LINE_LIMIT,
                ));
            }

            if let Some(error) = &item.error_preview {
                lines.extend(format_preview_lines(
                    GROUP_DETAIL_PREFIX,
                    Some("note "),
                    error,
                    TUI_THEME.error,
                    TUI_THEME.text_muted,
                    TOOL_PREVIEW_LINE_LIMIT,
                ));
            }
        }
    }

    lines.push(Line::default());
    lines
}

fn render_run_marker_row(marker: &RunMarkerRow) -> Vec<Line<'static>> {
    let style = match marker.kind {
        RunMarkerKind::AwaitingApproval => TUI_THEME.warning,
        RunMarkerKind::Running => TUI_THEME.info,
        RunMarkerKind::Completed => TUI_THEME.success,
        RunMarkerKind::Failed | RunMarkerKind::Cancelled => TUI_THEME.error,
    };

    vec![
        Line::from(vec![
            Span::styled(RUN_MARKER_PREFIX, TUI_THEME.operational_prefix),
            Span::styled(marker.label.clone(), style),
        ]),
        Line::default(),
    ]
}

#[derive(Debug)]
struct ActiveRunContext {
    focus_label: String,
    focus_style: ratatui::style::Style,
    active_run_label: Option<String>,
    active_run_style: ratatui::style::Style,
    task_label: Option<String>,
}

fn active_run_context(state: &AppState) -> ActiveRunContext {
    let Some(active_run_id) = state.active_run_id else {
        return ActiveRunContext {
            focus_label: "main session".to_string(),
            focus_style: TUI_THEME.text,
            active_run_label: None,
            active_run_style: TUI_THEME.text_muted,
            task_label: None,
        };
    };

    let delegated_task = state
        .session
        .tool_invocations
        .iter()
        .find(|invocation| {
            invocation.tool_name == "task" && invocation.child_run_id == Some(active_run_id)
        })
        .and_then(|invocation| delegated_task_from_invocation(state, invocation));

    if let Some(delegated_task) = delegated_task {
        let agent_name = delegated_task
            .agent_name
            .unwrap_or_else(|| "subagent".to_string());
        return ActiveRunContext {
            focus_label: format!("child subagent · {agent_name}"),
            focus_style: TUI_THEME.info,
            active_run_label: Some(format!(
                "child {}",
                summarize_text(&active_run_id.to_string())
            )),
            active_run_style: TUI_THEME.info,
            task_label: Some(format!("task {agent_name}")),
        };
    }

    ActiveRunContext {
        focus_label: "main session".to_string(),
        focus_style: TUI_THEME.text,
        active_run_label: Some(summarize_text(&active_run_id.to_string())),
        active_run_style: TUI_THEME.text_muted,
        task_label: None,
    }
}

fn tool_summary_label(
    state: &AppState,
    invocation: &fluent_code_app::session::model::ToolInvocationRecord,
) -> String {
    crate::conversation::derive_tool_row(&state.session, invocation).display_name
}

fn delegated_task_from_invocation(
    state: &AppState,
    invocation: &fluent_code_app::session::model::ToolInvocationRecord,
) -> Option<DelegatedTaskRow> {
    crate::conversation::derive_tool_row(&state.session, invocation).delegated_task
}

fn run_status_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::InProgress => "running",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn run_status_style(status: RunStatus) -> ratatui::style::Style {
    match status {
        RunStatus::InProgress => TUI_THEME.info,
        RunStatus::Completed => TUI_THEME.success,
        RunStatus::Failed | RunStatus::Cancelled => TUI_THEME.error,
    }
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

fn footer_text_with_ui(
    state: &AppState,
    show_tool_details: bool,
    layout_mode: LayoutMode,
) -> String {
    let active_context = active_run_context(state);
    let run_hint = if active_context.focus_label == "main session" {
        None
    } else {
        Some(active_context.focus_label.as_str())
    };
    let layout_label = layout_mode.label();

    match &state.status {
        AppStatus::Idle => format!(
            "Enter send • F1 help • F2 details {} • F3 layout {} • ↑↓/Pg keys scroll • End latest • Ctrl-N new • Esc/Ctrl-C quit",
            if show_tool_details {
                "expanded"
            } else {
                "compact"
            },
            layout_label,
        ),
        AppStatus::Generating => match run_hint {
            Some(run_hint) => {
                format!(
                    "Assistant responding · {run_hint} • F1 help • F2 details • F3 layout {layout_label} • Esc/Ctrl-C cancel"
                )
            }
            None => format!(
                "Assistant responding • F1 help • F2 details • F3 layout {layout_label} • Esc/Ctrl-C cancel"
            ),
        },
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
                    "{pending_count} tool call(s) waiting{} • Enter/Y approve all • N deny one • F1 help • F2 details • F3 layout {layout_label} • Esc/Ctrl-C cancel",
                    run_hint
                        .map(|run_hint| format!(" · {run_hint}"))
                        .unwrap_or_default()
                )
            } else {
                "Awaiting tool approval".to_string()
            }
        }
        AppStatus::RunningTool => match run_hint {
            Some(run_hint) => format!(
                "Executing approved tools · {run_hint} • F1 help • F2 details • F3 layout {layout_label} • Esc/Ctrl-C cancel run"
            ),
            None => format!(
                "Executing approved tools • F1 help • F2 details • F3 layout {layout_label} • Esc/Ctrl-C cancel run"
            ),
        },
        AppStatus::Error(error) => format!(
            "Error: {} • F1 help • F2 details • F3 layout {} • Ctrl-N new session • Esc/Ctrl-C quit",
            summarize_text(error),
            layout_label,
        ),
    }
}

fn footer_text(state: &AppState, ui_state: &UiState) -> String {
    footer_text_with_ui(state, ui_state.show_tool_details, ui_state.layout_mode)
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

fn format_turn_content_lines(
    content: &str,
    content_style: ratatui::style::Style,
) -> Vec<Line<'static>> {
    render_markdown_lines(content, content_style)
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("│ ", TUI_THEME.card_prefix)];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn format_streaming_turn_content_lines(
    content: &str,
    content_style: ratatui::style::Style,
) -> Vec<Line<'static>> {
    render_streaming_markdown_lines(content, content_style)
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("│ ", TUI_THEME.card_prefix)];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn format_text_blocks(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut paragraph = String::new();
    let mut in_code_block = false;

    for raw_line in content.lines() {
        let trimmed = raw_line.trim();

        if trimmed.starts_with("```") {
            if !paragraph.is_empty() {
                lines.push(std::mem::take(&mut paragraph));
            }
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            lines.push(format!("    {raw_line}"));
            continue;
        }

        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                lines.push(std::mem::take(&mut paragraph));
            }
            if lines.last().is_none_or(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }

        if is_list_line(trimmed)
            || trimmed.starts_with("> ")
            || strip_heading_prefix(trimmed).is_some()
        {
            if !paragraph.is_empty() {
                lines.push(std::mem::take(&mut paragraph));
            }
            lines.push(render_markdown_line(trimmed));
            continue;
        }

        let rendered = render_markdown_line(trimmed);

        if paragraph.is_empty() {
            paragraph.push_str(&rendered);
        } else {
            paragraph.push(' ');
            paragraph.push_str(&rendered);
        }
    }

    if !paragraph.is_empty() {
        lines.push(paragraph);
    }

    if lines.is_empty() {
        vec!["(empty)".to_string()]
    } else {
        lines
    }
}

fn is_list_line(line: &str) -> bool {
    line.starts_with("- ")
        || line.starts_with("* ")
        || line.split_once(". ").is_some_and(|(prefix, _)| {
            !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit())
        })
}

fn render_markdown_line(line: &str) -> String {
    let trimmed = line.trim();

    if let Some(heading) = strip_heading_prefix(trimmed) {
        return strip_inline_markdown(heading);
    }

    if let Some(quote) = trimmed.strip_prefix("> ") {
        return format!("› {}", strip_inline_markdown(quote));
    }

    if let Some((prefix, body)) = split_list_prefix(trimmed) {
        return format!("{prefix}{}", strip_inline_markdown(body));
    }

    strip_inline_markdown(trimmed)
}

fn strip_heading_prefix(line: &str) -> Option<&str> {
    let hash_count = line.chars().take_while(|ch| *ch == '#').count();
    if hash_count == 0 || hash_count > 6 {
        return None;
    }

    let rest = &line[hash_count..];
    rest.strip_prefix(' ').map(str::trim)
}

fn split_list_prefix(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix("- ") {
        return Some(("- ", rest));
    }

    if let Some(rest) = line.strip_prefix("* ") {
        return Some(("* ", rest));
    }

    line.split_once(". ").and_then(|(prefix, rest)| {
        (!prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit()))
            .then_some((line.split_at(prefix.len() + 2).0, rest))
    })
}

fn strip_inline_markdown(line: &str) -> String {
    let chars = line.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '['
            && let Some(close_label) = chars[i + 1..].iter().position(|ch| *ch == ']')
        {
            let label_end = i + 1 + close_label;
            if chars.get(label_end + 1) == Some(&'(')
                && let Some(close_url) = chars[label_end + 2..].iter().position(|ch| *ch == ')')
            {
                let label = chars[i + 1..label_end].iter().collect::<String>();
                let url_end = label_end + 2 + close_url;
                let url = chars[label_end + 2..url_end].iter().collect::<String>();
                out.push_str(&label);
                if !url.trim().is_empty() {
                    out.push_str(" (");
                    out.push_str(&url);
                    out.push(')');
                }
                i = url_end + 1;
                continue;
            }
        }

        if i + 1 < chars.len()
            && ((chars[i] == '*' && chars[i + 1] == '*')
                || (chars[i] == '_' && chars[i + 1] == '_')
                || (chars[i] == '~' && chars[i + 1] == '~'))
        {
            i += 2;
            continue;
        }

        if chars[i] == '`' || chars[i] == '*' {
            i += 1;
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

fn format_preview_lines(
    prefix: &'static str,
    label: Option<&'static str>,
    content: &str,
    content_style: ratatui::style::Style,
    label_style: ratatui::style::Style,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let mut shaped_lines = format_text_blocks(content)
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if shaped_lines.is_empty() {
        shaped_lines.push("(empty)".to_string());
    }

    let truncated = shaped_lines.len() > max_lines;
    shaped_lines.truncate(max_lines);

    if truncated && let Some(last) = shaped_lines.last_mut() {
        last.push_str(" ...");
    }

    shaped_lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = vec![Span::styled(prefix, TUI_THEME.operational_prefix)];
            let label_padding = label.map(|label| " ".repeat(label.chars().count()));

            if index == 0 {
                if let Some(label) = label {
                    spans.push(Span::styled(label, label_style));
                }
            } else if let Some(padding) = &label_padding {
                spans.push(Span::raw(padding.clone()));
            }

            spans.push(Span::styled(line, content_style));
            Line::from(spans)
        })
        .collect()
}

pub(crate) fn transcript_max_scroll(lines: &[Line<'_>], area_width: u16, area_height: u16) -> u16 {
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

fn resolve_transcript_scroll(
    lines: &[Line<'_>],
    area_width: u16,
    area_height: u16,
    follow_tail: bool,
    manual_scroll_top: u16,
) -> u16 {
    let max_scroll = transcript_max_scroll(lines, area_width, area_height);

    if follow_tail {
        max_scroll
    } else {
        manual_scroll_top.min(max_scroll)
    }
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
    use chrono::Utc;
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        active_run_context, body_areas, conversation_lines, footer_text_with_ui,
        resolve_transcript_scroll, shell_areas, sidebar_tool_lines, summarize_text,
        transcript_area, transcript_max_scroll,
    };
    use crate::ui_state::LayoutMode;
    use fluent_code_app::app::AppState;
    use fluent_code_app::plugin::{DiscoveryScope, LoadedPluginMetadata, PluginLoadSnapshot};
    use fluent_code_app::session::model::{
        Role, RunStatus, Session, ToolApprovalState, ToolExecutionState, ToolInvocationRecord,
        ToolSource, Turn,
    };

    #[test]
    fn transcript_max_scroll_stays_at_top_when_content_fits() {
        let lines = vec![Line::from("short"), Line::from("content")];

        assert_eq!(transcript_max_scroll(&lines, 20, 8), 0);
    }

    #[test]
    fn transcript_max_scroll_tracks_bottom_for_growing_content() {
        let lines = vec![
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
        ];

        assert_eq!(transcript_max_scroll(&lines, 10, 5), 1);
    }

    #[test]
    fn resolve_transcript_scroll_follows_tail_when_enabled() {
        let lines = vec![
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
        ];

        assert_eq!(resolve_transcript_scroll(&lines, 10, 5, true, 0), 1);
    }

    #[test]
    fn resolve_transcript_scroll_preserves_manual_position() {
        let lines = vec![
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
            Line::from("12345678"),
        ];

        assert_eq!(resolve_transcript_scroll(&lines, 10, 5, false, 0), 0);
    }

    #[test]
    fn resolve_transcript_scroll_clamps_manual_position_to_max() {
        let lines = vec![Line::from("12345678"), Line::from("12345678")];

        assert_eq!(resolve_transcript_scroll(&lines, 10, 5, false, 9), 0);
    }

    #[test]
    fn transcript_area_matches_side_by_side_layout_body_geometry() {
        let area = Rect::new(2, 4, 120, 40);
        let (_, body_area, _, _) = shell_areas(area);
        let (conversation_area, sidebar_area) = body_areas(body_area, LayoutMode::SideBySide);

        assert_eq!(
            transcript_area(area, LayoutMode::SideBySide),
            conversation_area
        );
        assert_eq!(conversation_area.x, body_area.x);
        assert_eq!(conversation_area.y, body_area.y);
        assert_eq!(conversation_area.height, body_area.height);
        assert_eq!(sidebar_area.y, body_area.y);
        assert_eq!(sidebar_area.height, body_area.height);
        assert_eq!(
            conversation_area.x + conversation_area.width,
            sidebar_area.x
        );
        assert_eq!(
            sidebar_area.x + sidebar_area.width,
            body_area.x + body_area.width
        );
    }

    #[test]
    fn transcript_area_matches_stacked_layout_body_geometry() {
        let area = Rect::new(2, 4, 120, 40);
        let (_, body_area, _, _) = shell_areas(area);
        let (conversation_area, sidebar_area) = body_areas(body_area, LayoutMode::Stacked);

        assert_eq!(
            transcript_area(area, LayoutMode::Stacked),
            conversation_area
        );
        assert_eq!(conversation_area.x, area.x);
        assert_eq!(conversation_area.width, area.width);
        assert_eq!(sidebar_area.x, area.x);
        assert_eq!(sidebar_area.width, area.width);
        assert_eq!(conversation_area.y, body_area.y);
        assert_eq!(
            conversation_area.y + conversation_area.height,
            sidebar_area.y
        );
        assert_eq!(
            sidebar_area.y + sidebar_area.height,
            body_area.y + body_area.height
        );
    }

    #[test]
    fn footer_text_reports_tool_detail_and_layout_modes() {
        let state = AppState::new(Session::new("ui state test"));

        assert!(footer_text_with_ui(&state, false, LayoutMode::SideBySide).contains("compact"));
        assert!(footer_text_with_ui(&state, true, LayoutMode::SideBySide).contains("expanded"));
        assert!(
            footer_text_with_ui(&state, false, LayoutMode::SideBySide)
                .contains("layout side-by-side")
        );
        assert!(footer_text_with_ui(&state, false, LayoutMode::Stacked).contains("layout stacked"));
    }

    #[test]
    fn footer_text_mentions_child_subagent_when_foreground_run_is_delegated() {
        let (mut state, _parent_run_id, child_run_id) = delegated_child_state();
        state.active_run_id = Some(child_run_id);
        state.status = fluent_code_app::app::AppStatus::Generating;

        let text = footer_text_with_ui(&state, false, LayoutMode::SideBySide);

        assert!(text.contains("Assistant responding"));
        assert!(text.contains("child subagent · explore"));
    }

    #[test]
    fn conversation_lines_shows_empty_state_when_no_rows_exist() {
        let state = AppState::new(Session::new("empty"));

        let lines = conversation_lines(&state, false);

        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("No messages yet"));
    }

    #[test]
    fn conversation_lines_renders_tool_row_inline_after_turn() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("inline tools");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Investigating session storage.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "crates/fluent-code-app/src/session/store.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });

        let state = AppState::new(session);
        let lines = conversation_lines(&state, true);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("ASSISTANT"));
        assert!(text.contains("├─ tool read"));
        assert!(text.contains("crates/fluent-code-app/src/session/store.rs"));
    }

    #[test]
    fn sidebar_tool_lines_shows_overview_counts() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("tool overview");

        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-pending".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "README.md"}),
            preceding_turn_id: None,
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-running".to_string(),
            tool_name: "search".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"query": "PersistSession"}),
            preceding_turn_id: None,
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });

        let state = AppState::new(session);
        let text = sidebar_tool_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("pending 1"));
        assert!(text.contains("running 1"));
        assert!(text.contains("completed 0"));
        assert!(text.contains("attention 0"));
        assert!(text.contains("delegated 0  running 0  done 0"));
        assert!(text.contains("latest search"));
    }

    #[test]
    fn sidebar_tool_lines_surface_delegated_task_status_and_prompt() {
        let (state, _parent_run_id, child_run_id) = delegated_child_state();
        let text = sidebar_tool_lines(&state, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("delegated 1  running 1  done 0"));
        assert!(text.contains("latest task explore"));
        assert!(text.contains("status approved / running"));
        assert!(text.contains("child running"));
        assert!(text.contains("prompt Inspect session persistence state"));
        assert!(text.contains(&summarize_text(&child_run_id.to_string())));
    }

    #[test]
    fn sidebar_tool_lines_empty_state_mentions_transcript_overview() {
        let state = AppState::new(Session::new("empty tools"));
        let text = sidebar_tool_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("No tool activity yet."));
        assert!(text.contains("main transcript"));
        assert!(text.contains("operations overview"));
    }

    #[test]
    fn sidebar_tool_lines_compact_mode_shows_plugin_and_warning_summary() {
        let mut state = AppState::new(Session::new("plugin sidebar compact"));
        state.plugin_load_snapshot = sample_plugin_load_snapshot();

        let text = sidebar_tool_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("plugins 2  warnings 2"));
        assert!(text.contains("Docs Plugin 2 tools"));
        assert!(text.contains("Formatter 1 tool"));
        assert!(text.contains("warning failed to parse plugin manifest"));
    }

    #[test]
    fn sidebar_tool_lines_expanded_mode_shows_plugin_metadata_and_warning_lines() {
        let mut state = AppState::new(Session::new("plugin sidebar expanded"));
        state.plugin_load_snapshot = sample_plugin_load_snapshot();

        let text = sidebar_tool_lines(&state, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("plugin Docs Plugin v0.2.0"));
        assert!(text.contains("scope global  id global.docs"));
        assert!(text.contains("tools docs_search, docs_read (2)"));
        assert!(text.contains("about Indexes docs for the workspace."));
        assert!(text.contains("plugin Formatter v1.4.1"));
        assert!(text.contains("scope project  id project.fmt"));
        assert!(text.contains("warnings 2"));
        assert!(text.contains("failed to parse plugin manifest"));
        assert!(text.contains("disabled during registry validation"));
    }

    #[test]
    fn conversation_lines_renders_grouped_tool_batch() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("grouped transcript");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Reading project files.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("ok".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-2".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/lib.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("ok".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let state = AppState::new(session);
        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("read batch (2)"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("src/lib.rs"));
    }

    #[test]
    fn conversation_lines_renders_approval_marker_inline() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("approval marker");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Waiting on tools.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-approval".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });

        let mut state = AppState::new(session);
        state.active_run_id = Some(run_id);
        state.status = fluent_code_app::app::AppStatus::AwaitingToolApproval;

        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("awaiting approval"));
    }

    #[test]
    fn conversation_lines_renders_running_marker_inline() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("running marker");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Still working.".to_string(),
            timestamp: Utc::now(),
        });

        let mut state = AppState::new(session);
        state.active_run_id = Some(run_id);
        state.status = fluent_code_app::app::AppStatus::RunningTool;

        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("running"));
    }

    #[test]
    fn conversation_lines_renders_completed_marker_inline() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("completed marker");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Done.".to_string(),
            timestamp: Utc::now(),
        });
        session.upsert_run(run_id, RunStatus::Completed);

        let state = AppState::new(session);
        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("completed"));
    }

    #[test]
    fn conversation_lines_renders_failed_marker_inline() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("failed marker");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Oops.".to_string(),
            timestamp: Utc::now(),
        });
        session.upsert_run(run_id, RunStatus::Failed);

        let state = AppState::new(session);
        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("failed"));
    }

    #[test]
    fn conversation_lines_collapses_paragraph_lines_but_preserves_blank_breaks() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("paragraph formatting");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "first line\ncontinues here\n\nnext paragraph".to_string(),
            timestamp: Utc::now(),
        });

        let text = conversation_lines(&AppState::new(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("first line continues here"));
        assert!(text.contains("\n│ \n│ next paragraph"));
    }

    #[test]
    fn conversation_lines_preserves_lists_and_code_fences() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("structured formatting");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "- first item\n2. second item\n```rust\nfn main() {}\n```".to_string(),
            timestamp: Utc::now(),
        });

        let text = conversation_lines(&AppState::new(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("│ - first item"));
        assert!(text.contains("│ 2. second item"));
        assert!(!text.contains("```rust"));
        assert!(text.contains("│     fn main() {}"));
    }

    #[test]
    fn conversation_lines_render_common_markdown_as_chat_text() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("markdown chat rendering");
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "# Heading\n> quoted text\nUse **bold** and `inline_code` plus [docs](https://example.com)."
                .to_string(),
            timestamp: Utc::now(),
        });

        let text = conversation_lines(&AppState::new(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("│ # Heading"));
        assert!(text.contains("│ › quoted text"));
        assert!(text.contains("Use "));
        assert!(text.contains("bold"));
        assert!(text.contains("inline_code"));
        assert!(text.contains("docs"));
        assert!(text.contains("https://example.com"));
        assert!(!text.contains("**bold**"));
        assert!(!text.contains("`inline_code`"));
        assert!(!text.contains("[docs](https://example.com)"));
    }

    #[test]
    fn conversation_lines_bounds_multiline_tool_previews_in_expanded_mode() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("bounded previews");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Inspecting results.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-preview".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("line one\nline two\nline three\nline four".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&AppState::new(session), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("result line one line two line three line four"));
        assert!(!text.contains("line four ..."));
    }

    #[test]
    fn conversation_lines_hides_success_detail_in_compact_mode() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("compact success");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Inspecting results.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-success".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("useful success payload".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&AppState::new(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("read src/main.rs"));
        assert!(!text.contains("result useful success payload"));
        assert!(!text.contains("approval approved"));
        assert!(!text.contains("execution completed"));
        assert!(!text.contains("args {"));
    }

    #[test]
    fn conversation_lines_shows_success_detail_in_expanded_mode() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("expanded success");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Inspecting results.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-success-expanded".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("useful success payload".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&AppState::new(session), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("approval approved"));
        assert!(text.contains("execution completed"));
        assert!(text.contains("args {\"path\":\"src/main.rs\"}"));
        assert!(text.contains("result useful success payload"));
    }

    #[test]
    fn conversation_lines_hides_grouped_success_previews_in_compact_mode() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("compact grouped success");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Reading project files.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        for (call_id, path, result) in [
            ("call-1", "src/main.rs", "alpha output"),
            ("call-2", "src/lib.rs", "beta output"),
        ] {
            session.tool_invocations.push(ToolInvocationRecord {
                id: Uuid::new_v4(),
                run_id,
                tool_call_id: call_id.to_string(),
                tool_name: "read".to_string(),
                tool_source: ToolSource::BuiltIn,
                arguments: json!({"path": path}),
                preceding_turn_id: Some(turn.id),
                approval_state: ToolApprovalState::Approved,
                execution_state: ToolExecutionState::Completed,
                result: Some(result.to_string()),
                error: None,
                child_run_id: None,
                delegation_agent_name: None,
                delegation_prompt: None,
                requested_at: Utc::now(),
                approved_at: Some(Utc::now()),
                completed_at: Some(Utc::now()),
            });
        }

        let text = conversation_lines(&AppState::new(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("read batch (2)"));
        assert!(text.contains("read src/main.rs"));
        assert!(text.contains("read src/lib.rs"));
        assert!(!text.contains("result alpha output"));
        assert!(!text.contains("result beta output"));
    }

    #[test]
    fn conversation_lines_bounds_grouped_tool_preview_lines() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("grouped bounded previews");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Reading project files.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("alpha\nbeta\ngamma\ndelta".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-2".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/lib.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("one\ntwo\nthree\nfour".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&AppState::new(session), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("read batch (2)"));
        assert!(text.contains("result alpha beta gamma delta"));
        assert!(text.contains("result one two three four"));
    }

    #[test]
    fn conversation_lines_maintains_primary_turns_and_subordinate_operational_rail() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("hierarchy polish");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "I will inspect the files first.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Pending,
            execution_state: ToolExecutionState::NotStarted,
            result: None,
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: None,
            completed_at: None,
        });

        let mut state = AppState::new(session);
        state.active_run_id = Some(run_id);
        state.status = fluent_code_app::app::AppStatus::AwaitingToolApproval;

        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("╭─ ASSISTANT response"));
        assert!(text.contains("╰─ assistant"));
        assert!(text.contains("  ├─ tool read"));
        assert!(text.contains("  · run awaiting approval"));
    }

    #[test]
    fn conversation_lines_aligns_wrapped_grouped_operational_details_under_labels() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("wrapped grouped details");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Inspecting grouped tool output.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("first line\nsecond line\nthird line\nfourth line".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-2".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/lib.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("alpha\nbeta\ngamma\ndelta".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&AppState::new(session), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("  ├─ read batch (2)"));
        assert!(text.contains("  │   • read src/main.rs"));
        assert!(text.contains("  │     result first line second line third line fourth line"));
        assert!(text.contains("  │   • read src/lib.rs"));
        assert!(text.contains("  │     result alpha beta gamma delta"));
    }

    #[test]
    fn conversation_lines_compact_mode_shows_plugin_tool_provenance_briefly() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("plugin provenance compact");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Using project plugin tools.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-plugin-compact".to_string(),
            tool_name: "docs_search".to_string(),
            tool_source: ToolSource::Plugin {
                plugin_id: "global.docs".to_string(),
                plugin_name: "Docs Plugin".to_string(),
                plugin_version: "0.2.0".to_string(),
                scope: DiscoveryScope::Global,
            },
            arguments: json!({"query": "AppState"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("found matches".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&state_with_snapshot(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("docs_search · plugin Docs Plugin · approved / completed"));
        assert!(!text.contains("global.docs"));
        assert!(!text.contains("v0.2.0"));
    }

    #[test]
    fn conversation_lines_expanded_mode_shows_plugin_tool_provenance_details() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("plugin provenance expanded");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Using project plugin tools.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-plugin-expanded".to_string(),
            tool_name: "docs_search".to_string(),
            tool_source: ToolSource::Plugin {
                plugin_id: "project.docs".to_string(),
                plugin_name: "Docs Plugin".to_string(),
                plugin_version: "1.1.0".to_string(),
                scope: DiscoveryScope::Project,
            },
            arguments: json!({"query": "AppState"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("found matches".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&state_with_snapshot(session), true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(
            "docs_search · plugin Docs Plugin v1.1.0 · project · project.docs · approved / completed"
        ));
    }

    #[test]
    fn conversation_lines_compact_mode_keeps_built_in_tools_quiet() {
        let run_id = Uuid::new_v4();
        let mut session = Session::new("built in quiet");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "Using built in tools.".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-built-in".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({"path": "src/main.rs"}),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("ok".to_string()),
            error: None,
            child_run_id: None,
            delegation_agent_name: None,
            delegation_prompt: None,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        let text = conversation_lines(&state_with_snapshot(session), false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!text.contains("via plugin"));
    }

    #[test]
    fn conversation_lines_render_delegated_task_compact_label_instead_of_json() {
        let (state, _parent_run_id, _child_run_id) = delegated_child_state();
        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("├─ tool task explore · approved / running"));
        assert!(text.contains("task explore · Inspect session persistence state"));
        assert!(!text.contains("{\"agent\":\"explore\""));
    }

    #[test]
    fn conversation_lines_render_delegated_task_expanded_child_details() {
        let (state, _parent_run_id, child_run_id) = delegated_child_state();
        let text = conversation_lines(&state, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("child running"));
        assert!(text.contains("prompt Inspect session persistence state"));
        assert!(text.contains(&format!(
            "child id {}",
            summarize_text(&child_run_id.to_string())
        )));
        assert!(text.contains(
            "args {\"agent\":\"explore\",\"prompt\":\"Inspect session persistence state\"}"
        ));
    }

    #[test]
    fn conversation_lines_show_child_foreground_run_marker() {
        let (mut state, _parent_run_id, child_run_id) = delegated_child_state();
        state.active_run_id = Some(child_run_id);
        state.status = fluent_code_app::app::AppStatus::Generating;

        let text = conversation_lines(&state, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Inspect child flow"));
        assert!(text.contains("running · subagent explore"));
        assert!(text.contains("task explore"));
    }

    #[test]
    fn conversation_lines_child_foreground_state_can_power_overview_focus_labels() {
        let (mut state, _parent_run_id, child_run_id) = delegated_child_state();
        state.active_run_id = Some(child_run_id);
        state.status = fluent_code_app::app::AppStatus::Generating;

        let context = active_run_context(&state);
        let expected_active_label = format!("child {}", summarize_text(&child_run_id.to_string()));

        assert_eq!(context.focus_label, "child subagent · explore");
        assert_eq!(context.task_label.as_deref(), Some("task explore"));
        assert_eq!(
            context.active_run_label.as_deref(),
            Some(expected_active_label.as_str())
        );
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn sample_plugin_load_snapshot() -> PluginLoadSnapshot {
        PluginLoadSnapshot {
            accepted_plugins: vec![
                LoadedPluginMetadata {
                    name: "Docs Plugin".to_string(),
                    id: "global.docs".to_string(),
                    version: "0.2.0".to_string(),
                    scope: DiscoveryScope::Global,
                    description: Some("Indexes docs for the workspace.".to_string()),
                    tool_names: vec!["docs_search".to_string(), "docs_read".to_string()],
                    tool_count: 2,
                },
                LoadedPluginMetadata {
                    name: "Formatter".to_string(),
                    id: "project.fmt".to_string(),
                    version: "1.4.1".to_string(),
                    scope: DiscoveryScope::Project,
                    description: None,
                    tool_names: vec!["format_diff".to_string()],
                    tool_count: 1,
                },
            ],
            warnings: vec![
                "failed to parse plugin manifest '/tmp/broken/plugin.toml': invalid type".to_string(),
                "plugin 'broken.docs' disabled during registry validation: reserved built-in tool name 'read'".to_string(),
            ],
        }
    }

    fn state_with_snapshot(session: Session) -> AppState {
        let mut state = AppState::new(session);
        state.plugin_load_snapshot = sample_plugin_load_snapshot();
        state
    }

    fn delegated_child_state() -> (AppState, Uuid, Uuid) {
        let parent_run_id = Uuid::new_v4();
        let child_run_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let mut session = Session::new("delegated child state");
        let turn = Turn {
            id: Uuid::new_v4(),
            run_id: parent_run_id,
            role: Role::Assistant,
            content: "Delegate this task".to_string(),
            timestamp: Utc::now(),
        };

        session.turns.push(turn.clone());
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: child_run_id,
            role: Role::User,
            content: "Inspect child flow".to_string(),
            timestamp: Utc::now(),
        });
        session.tool_invocations.push(ToolInvocationRecord {
            id: invocation_id,
            run_id: parent_run_id,
            tool_call_id: "task-call-1".to_string(),
            tool_name: "task".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: json!({
                "agent": "explore",
                "prompt": "Inspect session persistence state"
            }),
            preceding_turn_id: Some(turn.id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            child_run_id: Some(child_run_id),
            delegation_agent_name: Some("explore".to_string()),
            delegation_prompt: Some("Inspect session persistence state".to_string()),
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });
        session.upsert_run(parent_run_id, RunStatus::InProgress);
        session.upsert_run_with_parent(
            child_run_id,
            RunStatus::InProgress,
            Some(parent_run_id),
            Some(invocation_id),
        );

        (AppState::new(session), parent_run_id, child_run_id)
    }
}
