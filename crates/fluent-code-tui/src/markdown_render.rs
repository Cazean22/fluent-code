use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::theme::TUI_THEME;

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

pub(crate) fn render_markdown_lines(content: &str, base_style: Style) -> Vec<Line<'static>> {
    let markdown = if content.trim().is_empty() {
        "(empty)"
    } else {
        content
    };
    let parser = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH);
    let mut renderer = MarkdownRenderer::new(base_style);
    renderer.run(parser);
    renderer.finish(true).lines
}

pub(crate) fn render_streaming_markdown_lines(
    content: &str,
    base_style: Style,
) -> Vec<Line<'static>> {
    let markdown = if content.trim().is_empty() {
        "(empty)"
    } else {
        content
    };

    let plain_fenced_code_block_index = infer_unclosed_code_block(markdown)
        .then(|| count_fenced_code_block_markers(markdown).div_ceil(2));
    let parser = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH);
    let mut renderer = MarkdownRenderer::new(base_style)
        .with_plain_fenced_code_block_index(plain_fenced_code_block_index);
    renderer.run(parser);
    let lines = renderer.finish(false).lines;

    if lines.is_empty() {
        vec![Line::from(vec![Span::styled(
            "(empty)".to_string(),
            base_style,
        )])]
    } else {
        lines
    }
}

struct RenderResult {
    lines: Vec<Line<'static>>,
}

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    base_style: Style,
    plain_fenced_code_block_index: Option<usize>,
    fenced_code_blocks_seen: usize,
    current_code_block_plain: bool,
    inline_styles: Vec<Style>,
    link_destinations: Vec<String>,
    list_stack: Vec<ListContext>,
    blockquote_depth: usize,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
    suppress_next_softbreak: bool,
    previous_block_was_paragraph: bool,
}

#[derive(Clone, Copy)]
struct ListContext {
    ordered: bool,
    next_index: usize,
}

impl MarkdownRenderer {
    fn new(base_style: Style) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            base_style,
            plain_fenced_code_block_index: None,
            fenced_code_blocks_seen: 0,
            current_code_block_plain: false,
            inline_styles: Vec::new(),
            link_destinations: Vec::new(),
            list_stack: Vec::new(),
            blockquote_depth: 0,
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
            suppress_next_softbreak: false,
            previous_block_was_paragraph: false,
        }
    }

    fn with_plain_fenced_code_block_index(
        mut self,
        plain_fenced_code_block_index: Option<usize>,
    ) -> Self {
        self.plain_fenced_code_block_index = plain_fenced_code_block_index;
        self
    }

    fn run<'a>(&mut self, parser: Parser<'a>) {
        for event in parser {
            self.handle_event(event);
        }
    }

    fn finish(mut self, emit_empty_placeholder: bool) -> RenderResult {
        self.flush_line();
        if emit_empty_placeholder && self.lines.is_empty() {
            self.lines.push(Line::from(vec![Span::styled(
                "(empty)".to_string(),
                self.base_style,
            )]));
        }
        RenderResult { lines: self.lines }
    }

    fn handle_event<'a>(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => self.push_span(code.into_string(), TUI_THEME.markdown_code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(vec![Span::styled(
                    "———".to_string(),
                    TUI_THEME.transcript_divider,
                )]));
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.push_text(&math),
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.push_span(marker.to_string(), self.current_style());
            }
            Event::FootnoteReference(reference) => {
                self.push_text(&reference);
            }
        }
    }

    fn start_tag<'a>(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => {
                if self.previous_block_was_paragraph && !self.lines.is_empty() {
                    self.lines.push(Line::default());
                }
                self.previous_block_was_paragraph = false;
            }
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.push_span(self.heading_prefix(level), self.heading_style(level));
                self.inline_styles.push(self.heading_style(level));
                self.previous_block_was_paragraph = false;
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
                self.previous_block_was_paragraph = false;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.current_code_block_plain = false;
                self.code_block_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        self.fenced_code_blocks_seen += 1;
                        self.current_code_block_plain = self.plain_fenced_code_block_index
                            == Some(self.fenced_code_blocks_seen);
                        Some(lang.into_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code_block_buffer.clear();
                if self.code_block_lang.is_some() {
                    self.suppress_next_softbreak = true;
                }
                self.previous_block_was_paragraph = false;
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(ListContext {
                    ordered: start.is_some(),
                    next_index: start.unwrap_or(1) as usize,
                });
                self.previous_block_was_paragraph = false;
            }
            Tag::Item => {
                self.flush_line();
                let prefix = self.list_item_prefix();
                self.push_span(prefix, TUI_THEME.markdown_list_marker);
                self.previous_block_was_paragraph = false;
            }
            Tag::Emphasis => self.inline_styles.push(TUI_THEME.markdown_emphasis),
            Tag::Strong => self.inline_styles.push(TUI_THEME.markdown_strong),
            Tag::Strikethrough => self.inline_styles.push(TUI_THEME.markdown_strike),
            Tag::Link { dest_url, .. } => {
                self.inline_styles.push(TUI_THEME.markdown_link);
                self.link_destinations.push(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.push_span(format!("image ({dest_url})"), TUI_THEME.markdown_link);
                self.previous_block_was_paragraph = false;
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.previous_block_was_paragraph = true;
            }
            TagEnd::Heading(_) => {
                self.inline_styles.pop();
                self.flush_line();
                self.previous_block_was_paragraph = false;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.previous_block_was_paragraph = false;
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.flush_line();
                self.in_code_block = false;
                self.current_code_block_plain = false;
                self.code_block_lang = None;
                self.previous_block_was_paragraph = false;
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.previous_block_was_paragraph = false;
            }
            TagEnd::Item => {
                self.flush_line();
                self.previous_block_was_paragraph = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.inline_styles.pop();
            }
            TagEnd::Link => {
                self.inline_styles.pop();
                if let Some(destination) = self.link_destinations.pop()
                    && !destination.trim().is_empty()
                {
                    self.push_span(format!(" ({destination})"), TUI_THEME.markdown_link);
                }
            }
            TagEnd::Image
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_block_buffer.push_str(text);
            return;
        }

        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            if !line.is_empty() {
                self.push_span(line.to_string(), self.current_style());
            }
        }
    }

    fn soft_break(&mut self) {
        if self.in_code_block {
            self.flush_line();
            return;
        }
        if self.suppress_next_softbreak {
            self.suppress_next_softbreak = false;
            return;
        }
        self.push_span(" ".to_string(), self.current_style());
    }

    fn push_span(&mut self, text: String, style: Style) {
        self.push_text_prefix_if_needed();
        self.current_spans.push(Span::styled(text, style));
    }

    fn push_text_prefix_if_needed(&mut self) {
        if !self.current_spans.is_empty() {
            return;
        }

        if self.blockquote_depth > 0 {
            self.current_spans.push(Span::styled(
                "  ".repeat(self.blockquote_depth.saturating_sub(1)),
                TUI_THEME.markdown_quote,
            ));
            self.current_spans
                .push(Span::styled("› ".to_string(), TUI_THEME.markdown_quote));
        }
    }

    fn flush_line(&mut self) {
        if self.current_spans.is_empty() {
            self.lines.push(Line::default());
            return;
        }

        let spans = std::mem::take(&mut self.current_spans);
        self.lines.push(Line::from(spans));
    }

    fn flush_code_block(&mut self) {
        if self.code_block_buffer.is_empty() {
            return;
        }

        let language = self
            .code_block_lang
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        let rendered_lines = if self.current_code_block_plain {
            plain_code_lines(&self.code_block_buffer)
        } else {
            highlight_code_to_lines(&self.code_block_buffer, &language)
        };

        for line in rendered_lines {
            self.lines.push(line);
        }
        self.code_block_buffer.clear();
    }

    fn current_style(&self) -> Style {
        self.inline_styles
            .last()
            .copied()
            .unwrap_or(self.base_style)
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        match level {
            HeadingLevel::H1 => TUI_THEME.markdown_heading_1,
            HeadingLevel::H2 => TUI_THEME.markdown_heading_2,
            HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
                TUI_THEME.markdown_heading_3
            }
        }
    }

    fn heading_prefix(&self, level: HeadingLevel) -> String {
        match level {
            HeadingLevel::H1 => "# ".to_string(),
            HeadingLevel::H2 => "## ".to_string(),
            _ => "### ".to_string(),
        }
    }

    fn list_item_prefix(&mut self) -> String {
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        if let Some(context) = self.list_stack.last_mut() {
            if context.ordered {
                let index = context.next_index;
                context.next_index += 1;
                format!("{indent}{index}. ")
            } else {
                format!("{indent}- ")
            }
        } else {
            "- ".to_string()
        }
    }
}

fn highlight_code_to_lines(code: &str, language: &str) -> Vec<Line<'static>> {
    if code.is_empty() {
        return vec![Line::from(String::new())];
    }

    if code.len() > MAX_HIGHLIGHT_BYTES || code.lines().count() > MAX_HIGHLIGHT_LINES {
        return plain_code_lines(code);
    }

    let Some(syntax) = find_syntax(language) else {
        return plain_code_lines(code);
    };

    let syntax_set = syntax_set();
    let theme_set = ThemeSet::load_defaults();
    let theme = theme_set
        .themes
        .get("base16-ocean.dark")
        .or_else(|| theme_set.themes.values().next());
    let Some(theme) = theme else {
        return plain_code_lines(code);
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for line in LinesWithEndings::from(code) {
        let Ok(ranges) = highlighter.highlight_line(line, syntax_set) else {
            return plain_code_lines(code);
        };

        let mut spans = vec![Span::styled(
            "    ".to_string(),
            TUI_THEME.markdown_code_block,
        )];
        for (style, text) in ranges {
            let text = text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), convert_style(style)));
        }

        if spans.len() == 1 {
            spans.push(Span::styled(String::new(), TUI_THEME.markdown_code_block));
        }
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        vec![Line::from(vec![Span::styled(
            "    ".to_string(),
            TUI_THEME.markdown_code_block,
        )])]
    } else {
        lines
    }
}

fn plain_code_lines(code: &str) -> Vec<Line<'static>> {
    let mut result = code
        .lines()
        .map(|line| {
            Line::from(vec![Span::styled(
                format!("    {line}"),
                TUI_THEME.markdown_code_block,
            )])
        })
        .collect::<Vec<_>>();

    if result.is_empty() {
        result.push(Line::from(vec![Span::styled(
            "    ".to_string(),
            TUI_THEME.markdown_code_block,
        )]));
    }
    result
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn find_syntax(language: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    if language.is_empty() {
        return None;
    }

    let syntax_set = syntax_set();
    let patched = match language {
        "csharp" | "c-sharp" => "c#",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        other => other,
    };

    syntax_set
        .find_syntax_by_token(patched)
        .or_else(|| syntax_set.find_syntax_by_name(patched))
        .or_else(|| syntax_set.find_syntax_by_extension(patched))
}

fn convert_style(syntect_style: SyntectStyle) -> Style {
    let mut style = Style::default();
    style = style.fg(ratatui::style::Color::Rgb(
        syntect_style.foreground.r,
        syntect_style.foreground.g,
        syntect_style.foreground.b,
    ));

    if syntect_style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        style = style.add_modifier(ratatui::style::Modifier::BOLD);
    }

    style
}

fn infer_unclosed_code_block(content: &str) -> bool {
    count_fenced_code_block_markers(content) % 2 == 1
}

fn count_fenced_code_block_markers(content: &str) -> usize {
    content
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count()
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::{render_markdown_lines, render_streaming_markdown_lines};
    use crate::theme::TUI_THEME;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn renders_headings_quotes_links_and_inline_styles() {
        let lines = render_markdown_lines(
            "# Heading\n> quoted\nUse **bold** and `code` with [docs](https://example.com).",
            TUI_THEME.text,
        );

        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("# Heading"));
        assert!(text.contains("› quoted"));
        assert!(text.contains("Use bold and code with docs (https://example.com)."));
    }

    #[test]
    fn renders_lists_and_code_blocks() {
        let lines = render_markdown_lines(
            "1. first\n2. second\n```rust\nfn main() {}\n```",
            TUI_THEME.text,
        );

        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("1. first"));
        assert!(text.contains("2. second"));
        assert!(text.contains("    fn main() {}"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn highlights_supported_fenced_code_into_multiple_spans() {
        let lines = render_markdown_lines("```rust\nfn main() {}\n```", TUI_THEME.text);

        let code_line = lines
            .iter()
            .find(|line| line_text(line).contains("fn main() {}"))
            .expect("code line to exist");
        assert!(code_line.spans.len() > 2);
    }

    #[test]
    fn unknown_fenced_language_falls_back_to_plain_indented_code() {
        let lines = render_markdown_lines("```unknownlang\nhello\n```", TUI_THEME.text);

        let code_line = lines
            .iter()
            .find(|line| line_text(line).contains("hello"))
            .expect("code line to exist");
        assert_eq!(line_text(code_line), "    hello");
    }

    #[test]
    fn streaming_renderer_normalizes_incomplete_paragraph_tail_without_exposing_chunk_lines() {
        let streaming_lines = render_streaming_markdown_lines(
            "Committed line\nUse [docs](https://exam",
            TUI_THEME.text,
        );
        let committed_lines =
            render_markdown_lines("Committed line\nUse [docs](https://exam", TUI_THEME.text);

        let streaming_text = streaming_lines.iter().map(line_text).collect::<Vec<_>>();
        let committed_text = committed_lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(streaming_text, committed_text);
        assert_eq!(
            streaming_text.first().map(String::as_str),
            Some("Committed line Use [docs](https://exam")
        );
        assert!(
            !streaming_text
                .iter()
                .any(|line| line == "Use [docs](https://exam")
        );
    }

    #[test]
    fn streaming_renderer_preserves_incomplete_code_block_tail_as_code() {
        let lines = render_streaming_markdown_lines("```rust\nfn main", TUI_THEME.text);

        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("```rust"));
        assert!(text.contains("    fn main"));
    }

    #[test]
    fn completed_fenced_code_transitions_from_plain_streaming_tail_to_highlighted_output() {
        let streaming_lines = render_streaming_markdown_lines("```rust\nfn main", TUI_THEME.text);
        let streaming_code_line = streaming_lines
            .iter()
            .find(|line| line_text(line).contains("fn main"))
            .expect("streaming code line to exist");
        assert_eq!(streaming_code_line.spans.len(), 1);
        assert_eq!(line_text(streaming_code_line), "    fn main");

        let committed_lines = render_markdown_lines("```rust\nfn main\n```", TUI_THEME.text);
        let committed_code_line = committed_lines
            .iter()
            .find(|line| line_text(line).contains("fn main"))
            .expect("committed code line to exist");
        assert!(committed_code_line.spans.len() > 1);
        assert_eq!(line_text(committed_code_line), "    fn main");
    }

    #[test]
    fn streaming_renderer_keeps_completed_fenced_blocks_highlighted_before_plain_unfinished_tail() {
        let lines = render_streaming_markdown_lines(
            "```rust\nfn main() {}\n```\n\n```rust\nlet answer = 4",
            TUI_THEME.text,
        );

        let completed_code_line = lines
            .iter()
            .find(|line| line_text(line).contains("fn main() {}"))
            .expect("completed code line to exist");
        let unfinished_code_line = lines
            .iter()
            .find(|line| line_text(line).contains("let answer = 4"))
            .expect("unfinished code line to exist");

        assert!(completed_code_line.spans.len() > 2);
        assert_eq!(line_text(completed_code_line), "    fn main() {}");
        assert_eq!(unfinished_code_line.spans.len(), 1);
        assert_eq!(line_text(unfinished_code_line), "    let answer = 4");
    }
}
