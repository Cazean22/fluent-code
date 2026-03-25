#[derive(Debug, Clone, Default)]
pub struct UiState {
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
