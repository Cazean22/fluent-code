#[allow(dead_code)]
#[path = "../src/acp.rs"]
mod acp_projection_regression;
#[allow(dead_code)]
#[path = "../src/conversation.rs"]
mod conversation;
#[allow(dead_code)]
#[path = "../src/markdown_render.rs"]
mod markdown_render;
#[allow(dead_code)]
#[path = "../src/terminal.rs"]
mod terminal;
#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;
#[allow(dead_code)]
#[path = "../src/ui_state.rs"]
mod ui_state;
#[allow(dead_code)]
#[path = "../src/view.rs"]
mod view;

#[test]
fn acp_default_render_shows_full_transcript_with_active_cell() {
    acp_projection_regression::assert_acp_default_render_shows_full_transcript_with_active_cell();
}

#[test]
fn acp_default_render_preserves_markdown_scroll_and_fidelity_state() {
    acp_projection_regression::assert_acp_default_render_preserves_markdown_scroll_and_fidelity_state();
}
