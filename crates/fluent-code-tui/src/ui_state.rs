#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutMode {
    #[default]
    SideBySide,
    Stacked,
}

impl LayoutMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::SideBySide => Self::Stacked,
            Self::Stacked => Self::SideBySide,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SideBySide => "side-by-side",
            Self::Stacked => "stacked",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub layout_mode: LayoutMode,
    pub show_tool_details: bool,
    pub show_help_overlay: bool,
    pub transcript_scroll_top: u16,
    pub transcript_follow_tail: bool,
}

impl UiState {
    pub fn reset_transcript_navigation(&mut self) {
        self.transcript_scroll_top = 0;
        self.transcript_follow_tail = true;
    }
}
