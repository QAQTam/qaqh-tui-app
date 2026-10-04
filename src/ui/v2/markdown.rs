//! Markdown 渲染。
//!
//! 模块划分对齐 codex `tui/src/markdown_render.rs` + `markdown_render/`：事件状态机
//! 留在本文件，代码高亮（`code`）、表格（`table`）、行内折行（`text`）、列表（`list`）
//! 各自独立成子模块。表格列宽与单元格折行的算法取自 grok `xai-grok-markdown`。

mod code;
mod list;
mod table;
#[cfg(test)]
mod tests;
mod text;

use pulldown_cmark::{BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

use code::{CodeState, render_code_block};
use list::{ItemState, ListState};
use table::{TableState, render_table};
use text::{StyledSpan, wrap_styled};

const CODE_INDENT: &str = "  ";
const QUOTE_PREFIX: &str = "▎ ";

struct RenderState<'a> {
    theme: &'a Theme,
    width: usize,
    out: Vec<Line<'static>>,
    paragraph: Vec<StyledSpan>,
    bold: bool,
    italic: bool,
    strike: bool,
    link: bool,
    heading: Option<u8>,
    quote_depth: usize,
    /// GFM alert 类型（`> [!NOTE]` 等）；无标签引用块为 `None`。
    quote_kind: Option<BlockQuoteKind>,
    lists: Vec<ListState>,
    item: Option<ItemState>,
    code: Option<CodeState>,
    table: Option<TableState>,
}

impl<'a> RenderState<'a> {
    fn new(theme: &'a Theme, width: usize) -> Self {
        Self {
            theme,
            width: width.max(1),
            out: Vec::new(),
            paragraph: Vec::new(),
            bold: false,
            italic: false,
            strike: false,
            link: false,
            heading: None,
            quote_depth: 0,
            quote_kind: None,
            lists: Vec::new(),
            item: None,
            code: None,
            table: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_paragraph();
        self.flush_item();
        if let Some(code) = self.code.take() {
            self.flush_code(code);
        }
        if let Some(table) = self.table.take() {
            self.flush_table(table);
        }
        while self.out.last().is_some_and(|line| line.spans.is_empty()) {
            self.out.pop();
        }
        if self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.out
    }

    fn handle(&mut self, event: Event<'_>) {
        if self.handle_table_event(&event) {
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(text) => {
                let style = fg(self.theme.markdown.code);
                self.push_styled(text.to_string(), style);
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_text("\n"),
            Event::Rule => {
                self.flush_paragraph();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width.min(40)),
                    fg(self.theme.markdown.rule),
                )));
                self.push_blank();
            }
            Event::TaskListMarker(checked) => self.task_marker(checked),
            Event::FootnoteReference(name) => {
                self.push_styled(format!("[^{name}]"), fg(self.theme.markdown.link))
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_styled(text.to_string(), fg(self.theme.markdown.code));
            }
            Event::Html(_) | Event::InlineHtml(_) => {}
        }
    }

    fn handle_table_event(&mut self, event: &Event<'_>) -> bool {
        if self.table.is_none() {
            return false;
        }

        match event {
            Event::Start(Tag::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.flush_row();
                    table.in_head = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.flush_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.flush_cell();
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.push_str(text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.push(' ');
                }
            }
            // `<br>` 是单元格内的换行。其余内联 HTML 直接丢弃：留下原文会在表格里
            // 漏出标签（codex `test_table_inline_html_no_raw_text_leak`）。
            Event::Html(html) | Event::InlineHtml(html) => {
                if is_line_break_tag(html)
                    && let Some(table) = self.table.as_mut()
                {
                    table.cell.push('\n');
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = self.table.take() {
                    self.flush_table(table);
                }
            }
            _ => {}
        }
        true
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_paragraph();
                self.heading = Some(heading_level(level));
            }
            Tag::Strong => self.bold = true,
            Tag::Emphasis => self.italic = true,
            Tag::Strikethrough => self.strike = true,
            Tag::Link { .. } => self.link = true,
            Tag::BlockQuote(kind) => {
                self.flush_paragraph();
                self.quote_depth += 1;
                self.quote_kind = kind;
                // GFM alert（`> [!NOTE]`）：在第一行给一个带标签的抬头；正文沿用
                // 引用块配色，只有标签用各类型自己的颜色。
                if let Some(kind) = kind {
                    let prefix = self.quote_prefix();
                    self.out.push(Line::from(vec![
                        Span::styled(prefix, fg(self.theme.markdown.quote)),
                        Span::styled(
                            alert_label(kind).to_string(),
                            fg(alert_color(kind, self.theme)).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
            }
            Tag::List(start) => {
                self.flush_paragraph();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => self.start_item(),
            Tag::CodeBlock(kind) => {
                self.flush_paragraph();
                self.code = Some(CodeState {
                    lang: match kind {
                        CodeBlockKind::Fenced(lang) => Some(lang.to_string()),
                        CodeBlockKind::Indented => None,
                    },
                    text: String::new(),
                });
            }
            Tag::Table(alignments) => {
                self.flush_paragraph();
                self.table = Some(TableState::new(alignments));
            }
            Tag::Paragraph => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush_paragraph();
                self.heading = None;
                self.push_blank();
            }
            TagEnd::Strong => self.bold = false,
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strikethrough => self.strike = false,
            TagEnd::Link => self.link = false,
            TagEnd::Paragraph => {
                if self.item.is_none() && self.code.is_none() {
                    self.flush_paragraph();
                    self.push_blank();
                }
            }
            TagEnd::Item => self.flush_item(),
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.push_blank();
                }
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.flush_code(code);
                }
                self.push_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.flush_item();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    self.quote_kind = None;
                }
                self.push_blank();
            }
            _ => {}
        }
    }

    fn start_item(&mut self) {
        self.flush_item();
        let depth = self.lists.len().saturating_sub(1);
        let (marker, ordered) = self
            .lists
            .last_mut()
            .map(|list| {
                if list.ordered {
                    let marker = format!("{}. ", list.next);
                    list.next += 1;
                    (marker, true)
                } else {
                    ("• ".to_string(), false)
                }
            })
            .unwrap_or_else(|| ("• ".to_string(), false));
        let indent = "  ".repeat(depth);
        let prefix = format!("{indent}{marker}");
        let prefix_style = if ordered {
            fg(self.theme.markdown.text)
        } else {
            fg(self.theme.text.dim)
        };
        self.item = Some(ItemState {
            continuation: " ".repeat(prefix.width()),
            prefix,
            prefix_style,
            spans: Vec::new(),
        });
    }

    fn task_marker(&mut self, checked: bool) {
        let Some(item) = self.item.as_mut() else {
            return;
        };
        if checked {
            item.prefix.push_str("✓ ");
            item.prefix_style = fg(self.theme.markdown.task_done);
        } else {
            item.prefix.push_str("○ ");
            item.prefix_style = fg(self.theme.markdown.task_todo);
        }
        item.continuation = " ".repeat(item.prefix.width());
    }

    fn push_text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            code.text.push_str(text);
            return;
        }
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        self.push_styled(text.to_string(), style);
    }

    fn push_styled(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        let span = StyledSpan::new(text, style);
        if let Some(item) = self.item.as_mut() {
            item.spans.push(span);
        } else {
            self.paragraph.push(span);
        }
    }

    fn current_style(&self) -> Style {
        let mut style = if let Some(level) = self.heading {
            heading_style(level, self.theme)
        } else if self.quote_depth > 0 {
            fg(self.theme.markdown.quote)
        } else {
            fg(self.theme.markdown.text)
        };
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link {
            style = style
                .fg(self.theme.markdown.link)
                .add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    fn flush_paragraph(&mut self) {
        let spans = std::mem::take(&mut self.paragraph);
        if spans.is_empty() {
            return;
        }
        self.flush_body(spans, None);
    }

    fn flush_item(&mut self) {
        let Some(item) = self.item.take() else {
            return;
        };
        if item.spans.is_empty() {
            return;
        }
        self.flush_body(
            item.spans,
            Some((item.prefix, item.continuation, item.prefix_style)),
        );
    }

    fn flush_body(&mut self, spans: Vec<StyledSpan>, item: Option<(String, String, Style)>) {
        let quote = self.quote_prefix();
        let item_width = item.as_ref().map_or(0, |(prefix, _, _)| prefix.width());
        let body_width = self
            .width
            .saturating_sub(quote.width())
            .saturating_sub(item_width)
            .max(1);
        let rows = wrap_styled(&spans, body_width);
        for (idx, row) in rows.into_iter().enumerate() {
            let mut line = Vec::with_capacity(row.len() + 2);
            if !quote.is_empty() {
                line.push(Span::styled(quote.clone(), fg(self.theme.markdown.quote)));
            }
            if let Some((prefix, continuation, prefix_style)) = &item {
                let shown = if idx == 0 { prefix } else { continuation };
                line.push(Span::styled(shown.clone(), *prefix_style));
            }
            line.extend(row.into_iter().map(StyledSpan::into_span));
            self.out.push(Line::from(line));
        }
    }

    fn flush_code(&mut self, code: CodeState) {
        let quote = self.quote_prefix();
        let width = self.width.saturating_sub(quote.width()).max(1);
        let lines = render_code_block(&code.text, code.lang.as_deref(), width, self.theme);
        for line in lines {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            if !quote.is_empty() {
                spans.push(Span::styled(quote.clone(), fg(self.theme.markdown.quote)));
            }
            spans.extend(line.spans);
            self.out.push(Line::from(spans));
        }
    }

    fn flush_table(&mut self, table: TableState) {
        let quote = self.quote_prefix();
        let width = self.width.saturating_sub(quote.width()).max(1);
        let lines = render_table(table, width, self.theme);
        for line in lines {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            if !quote.is_empty() {
                spans.push(Span::styled(quote.clone(), fg(self.theme.markdown.quote)));
            }
            spans.extend(line.spans);
            self.out.push(Line::from(spans));
        }
        self.push_blank();
    }

    fn quote_prefix(&self) -> String {
        QUOTE_PREFIX.repeat(self.quote_depth)
    }

    fn push_blank(&mut self) {
        if self.out.last().is_some_and(|line| !line.spans.is_empty()) {
            self.out.push(Line::default());
        }
    }
}

pub fn render(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::default()];
    }
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    // GFM 的 blockquote tags（`> [!NOTE]` / `[!TIP]` / `[!IMPORTANT]` /
    // `[!WARNING]` / `[!CAUTION]`）。
    options.insert(Options::ENABLE_GFM);

    let mut state = RenderState::new(theme, width);
    for event in Parser::new_ext(text, options) {
        state.handle(event);
    }
    state.finish()
}

fn heading_level(level: pulldown_cmark::HeadingLevel) -> u8 {
    match level {
        pulldown_cmark::HeadingLevel::H1 => 1,
        pulldown_cmark::HeadingLevel::H2 => 2,
        pulldown_cmark::HeadingLevel::H3 => 3,
        pulldown_cmark::HeadingLevel::H4 => 4,
        pulldown_cmark::HeadingLevel::H5 => 5,
        pulldown_cmark::HeadingLevel::H6 => 6,
    }
}

fn heading_style(level: u8, theme: &Theme) -> Style {
    let color = match level {
        1 => theme.markdown.h1,
        2 => theme.markdown.h2,
        3 => theme.markdown.h3,
        4 => theme.markdown.h4,
        5 => theme.markdown.h5,
        _ => theme.markdown.h6,
    };
    fg(color).add_modifier(Modifier::BOLD)
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// GFM alert（`> [!NOTE]` 等）的抬头文字。
fn alert_label(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "NOTE",
        BlockQuoteKind::Tip => "TIP",
        BlockQuoteKind::Important => "IMPORTANT",
        BlockQuoteKind::Warning => "WARNING",
        BlockQuoteKind::Caution => "CAUTION",
    }
}

/// GFM alert 的抬头配色。
fn alert_color(kind: BlockQuoteKind, theme: &Theme) -> Color {
    match kind {
        BlockQuoteKind::Note => theme.accent.system,
        BlockQuoteKind::Tip => theme.accent.success,
        BlockQuoteKind::Important => theme.semantic.plan,
        BlockQuoteKind::Warning => theme.semantic.warning,
        BlockQuoteKind::Caution => theme.accent.error,
    }
}

/// `<br>` / `<br/>` / `<br />` / `<BR>` 都算换行标签。
fn is_line_break_tag(html: &str) -> bool {
    let mut tag = html.trim().to_ascii_lowercase();
    tag.retain(|ch| !ch.is_whitespace());
    tag.trim_end_matches('/') == "<br>"
}
