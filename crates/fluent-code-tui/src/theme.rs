use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub panel_border: Style,
    pub panel_border_active: Style,
    pub title: Style,
    pub text: Style,
    pub text_muted: Style,
    pub transcript_divider: Style,
    pub card_prefix: Style,
    pub user_accent: Style,
    pub assistant_accent: Style,
    pub system_accent: Style,
    pub tool_accent: Style,
    pub label: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub info: Style,
}

pub const TUI_THEME: Theme = Theme {
    panel_border: Style::new().fg(Color::DarkGray),
    panel_border_active: Style::new().fg(Color::Blue),
    title: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    text: Style::new().fg(Color::White),
    text_muted: Style::new().fg(Color::DarkGray),
    transcript_divider: Style::new().fg(Color::DarkGray),
    card_prefix: Style::new().fg(Color::DarkGray),
    user_accent: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    assistant_accent: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
    system_accent: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    tool_accent: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    label: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
    success: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
    warning: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    info: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
};
