//! V2 Markdown → ratatui 渲染。
//!
//! 这一层只负责把 `pulldown-cmark` 事件投影成 V2 主题下的 [`Line`]：
//! - 颜色全部来自 [`Theme`]，不依赖 V1 的 `RenderLine/SpanStyle`；
//! - 代码高亮只取 syntect 前景与字形，不搬背景，保持终端底色；
//! - 输出仍然是可逐行提交的普通 `Line`，方便流式稳定边界接管。
//!
//! 完整块用于 sealed/history 渲染；流式路径只把已经稳定的源码行交给
//! [`render_code_line`] 或 [`render`] 单行渲染，未完成尾行留在 inline viewport。

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

const CODE_INDENT: &str = "  ";
const QUOTE_PREFIX: &str = "▎ ";
const MAX_TABLE_ROWS: usize = 32;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

#[derive(Clone)]
struct StyledSpan {
    text: String,
    style: Style,
}

impl StyledSpan {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn into_span(self) -> Span<'static> {
        Span::styled(self.text, self.style)
    }
}

struct ListState {
    ordered: bool,
    next: u64,
}

struct ItemState {
    prefix: String,
    continuation: String,
    prefix_style: Style,
    spans: Vec<StyledSpan>,
}

struct CodeState {
    lang: Option<String>,
    text: String,
}

struct TableState {
    alignments: Vec<Alignment>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    in_head: bool,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            headers: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
            cell: String::new(),
            in_head: false,
        }
    }

    fn flush_cell(&mut self) {
        self.row.push(std::mem::take(&mut self.cell));
    }

    fn flush_row(&mut self) {
        if self.row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.row);
        if self.in_head && self.headers.is_empty() {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }
}

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
            Tag::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth += 1;
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

pub(crate) fn render_code_line(
    text: &str,
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(CODE_INDENT.width()).max(1);
    let spans = if text.is_empty() {
        Vec::new()
    } else {
        highlighted_spans(text, lang, theme)
    };
    let rows = if spans.is_empty() {
        vec![Vec::new()]
    } else {
        wrap_styled(&spans, content_width)
    };
    rows.into_iter()
        .map(|row| {
            let mut line = Vec::with_capacity(row.len() + 1);
            line.push(Span::styled(CODE_INDENT.to_string(), Style::default()));
            line.extend(row.into_iter().map(StyledSpan::into_span));
            Line::from(line)
        })
        .collect()
}

fn render_code_block(
    text: &str,
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(CODE_INDENT.width()).max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.is_empty() {
            out.push(Line::from(Span::styled(
                CODE_INDENT.to_string(),
                Style::default(),
            )));
            continue;
        }
        let spans = highlighted_spans(raw, lang, theme);
        let rows = wrap_styled(&spans, content_width);
        for row in rows {
            let mut line = Vec::with_capacity(row.len() + 1);
            line.push(Span::styled(CODE_INDENT.to_string(), Style::default()));
            line.extend(row.into_iter().map(StyledSpan::into_span));
            out.push(Line::from(line));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            CODE_INDENT.to_string(),
            Style::default(),
        )));
    }
    out
}

fn highlighted_spans(line: &str, lang: Option<&str>, theme: &Theme) -> Vec<StyledSpan> {
    let syntax_set = syntax_set();
    let syntax = lang
        .and_then(|lang| syntax_set.find_syntax_by_token(lang))
        .or_else(|| lang.and_then(|lang| syntax_set.find_syntax_by_extension(lang)))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let syntax_theme = &theme_set().themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, syntax_theme);
    match highlighter.highlight_line(line, syntax_set) {
        Ok(ranges) => ranges
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(style, text)| StyledSpan::new(text.to_string(), syntect_style(style)))
            .collect(),
        Err(_) => vec![StyledSpan::new(line.to_string(), fg(theme.markdown.code))],
    }
}

fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let mut out = fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn render_table(table: TableState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if table.headers.is_empty() && table.rows.is_empty() {
        return Vec::new();
    }

    let requested_cols = table
        .headers
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let max_cols = width.saturating_sub(1).saturating_div(4).max(1);
    let cols = requested_cols.min(max_cols);
    if cols == 0 {
        return Vec::new();
    }

    let border_w = cols + 1;
    let avail = width.saturating_sub(border_w).max(cols * 3);
    let base = (avail / cols).max(3);
    let mut col_widths = vec![base; cols];
    let rem = avail.saturating_sub(base * cols);
    for width in col_widths.iter_mut().take(rem.min(cols)) {
        *width += 1;
    }

    let top = table_border(&col_widths, '┌', '┬', '┐');
    let middle = table_border(&col_widths, '├', '┼', '┤');
    let bottom = table_border(&col_widths, '└', '┴', '┘');
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(top, fg(theme.markdown.rule))));

    if !table.headers.is_empty() {
        out.push(table_row(
            &table.headers,
            &col_widths,
            &table.alignments,
            fg(theme.markdown.table_head).add_modifier(Modifier::BOLD),
            fg(theme.markdown.rule),
        ));
        out.push(Line::from(Span::styled(middle, fg(theme.markdown.rule))));
    }

    let omitted = table.rows.len().saturating_sub(MAX_TABLE_ROWS);
    for row in table.rows.iter().take(MAX_TABLE_ROWS) {
        out.push(table_row(
            row,
            &col_widths,
            &table.alignments,
            fg(theme.markdown.text),
            fg(theme.markdown.rule),
        ));
    }
    if omitted > 0 {
        out.push(Line::from(Span::styled(
            format!("  （表格省略 {omitted} 行）"),
            fg(theme.text.dim),
        )));
    }
    out.push(Line::from(Span::styled(bottom, fg(theme.markdown.rule))));
    out
}

fn table_border(widths: &[usize], left: char, middle: char, right: char) -> String {
    let mut out = String::from(left);
    for (idx, width) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(*width));
        if idx + 1 < widths.len() {
            out.push(middle);
        } else {
            out.push(right);
        }
    }
    out
}

fn table_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    cell_style: Style,
    border_style: Style,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(widths.len() * 2 + 1);
    spans.push(Span::styled("│".to_string(), border_style));
    for (idx, width) in widths.iter().enumerate() {
        let raw = cells.get(idx).map(String::as_str).unwrap_or("");
        spans.push(Span::styled(
            format_cell(raw, *width, alignments.get(idx).copied()),
            cell_style,
        ));
        spans.push(Span::styled("│".to_string(), border_style));
    }
    Line::from(spans)
}

fn format_cell(raw: &str, width: usize, align: Option<Alignment>) -> String {
    let raw = raw.replace('\n', " ");
    let display_width = raw.width();
    if display_width >= width {
        let mut out = String::new();
        let mut used = 0usize;
        for ch in raw.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if used + ch_width + 1 > width {
                break;
            }
            out.push(ch);
            used += ch_width;
        }
        let pad = width.saturating_sub(out.width() + 1);
        return format!("{out}…{}", " ".repeat(pad));
    }

    let pad = width.saturating_sub(display_width);
    match align {
        Some(Alignment::Right) => format!("{}{raw}", " ".repeat(pad)),
        Some(Alignment::Center) => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{raw}{}", " ".repeat(left), " ".repeat(right))
        }
        _ => format!("{raw}{}", " ".repeat(pad)),
    }
}

fn wrap_styled(spans: &[StyledSpan], width: usize) -> Vec<Vec<StyledSpan>> {
    if width == 0 {
        return vec![spans.to_vec()];
    }

    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        chars.reserve(span.text.chars().count());
        for ch in span.text.chars() {
            chars.push((ch, span.style));
        }
    }

    let mut out: Vec<Vec<StyledSpan>> = Vec::new();
    let mut start = 0usize;
    let mut line_width = 0usize;
    let mut last_space: Option<usize> = None;
    let mut idx = 0usize;
    while idx < chars.len() {
        let (ch, _) = chars[idx];
        if ch == '\n' {
            push_wrapped_line(&mut out, &chars[start..idx]);
            start = idx + 1;
            line_width = 0;
            last_space = None;
            idx += 1;
            continue;
        }

        let ch_width = ch.width().unwrap_or(0);
        if line_width + ch_width > width {
            if ch == ' ' {
                push_wrapped_line(&mut out, &chars[start..idx]);
                start = idx + 1;
                line_width = 0;
                last_space = None;
                idx += 1;
                continue;
            }
            if let Some(boundary) = last_space {
                if start < boundary {
                    push_wrapped_line(&mut out, &chars[start..boundary]);
                }
                start = boundary + 1;
                line_width = chars[start..idx]
                    .iter()
                    .map(|(ch, _)| ch.width().unwrap_or(0))
                    .sum();
                last_space = None;
            } else {
                let end = if start == idx { idx + 1 } else { idx };
                push_wrapped_line(&mut out, &chars[start..end]);
                start = end;
                line_width = 0;
                last_space = None;
                idx = end;
                continue;
            }
            continue;
        }

        if ch == ' ' {
            last_space = Some(idx);
        }
        line_width += ch_width;
        idx += 1;
    }

    if start < chars.len() {
        push_wrapped_line(&mut out, &chars[start..]);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

fn push_wrapped_line(out: &mut Vec<Vec<StyledSpan>>, chars: &[(char, Style)]) {
    let mut spans: Vec<StyledSpan> = Vec::new();
    for (ch, style) in chars {
        if let Some(last) = spans.last_mut()
            && last.style == *style
        {
            last.text.push(*ch);
        } else {
            spans.push(StyledSpan::new(ch.to_string(), *style));
        }
    }
    out.push(spans);
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_inline_emphasis_without_markers() {
        let lines = render("**bold** and *italic* and ~~gone~~", 60, &theme());
        let text = text_of(&lines);
        assert_eq!(text, "bold and italic and gone");
        let spans: Vec<&Span<'_>> = lines.iter().flat_map(|line| line.spans.iter()).collect();
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn renders_heading_levels_and_theme_colors() {
        let lines = render("# one\n\n#### four", 40, &theme());
        let heading_one = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "one")
            .expect("h1");
        let heading_four = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "four")
            .expect("h4");
        assert_eq!(heading_one.style.fg, Some(theme().markdown.h1));
        assert_eq!(heading_four.style.fg, Some(theme().markdown.h4));
    }

    #[test]
    fn renders_links_with_link_style() {
        let lines = render("[OpenAI](https://openai.com)", 60, &theme());
        let text = text_of(&lines);
        assert_eq!(text, "OpenAI");
        let span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "OpenAI")
            .expect("link text");
        assert_eq!(span.style.fg, Some(theme().markdown.link));
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn renders_tables_and_keeps_rows() {
        let lines = render(
            "| name | value |\n|---|---|\n| a | 1 |\n| b | 2 |",
            40,
            &theme(),
        );
        let text = text_of(&lines);
        assert!(text.contains("name"));
        assert!(text.contains("value"));
        assert!(text.contains("a"));
        assert!(text.contains("1"));
        assert!(text.contains("b"));
        assert!(text.contains("2"));
        assert!(lines.len() >= 6);
    }

    #[test]
    fn code_block_has_no_background_and_keeps_indent() {
        let lines = render("```rust\nfn main() {}\n```", 50, &theme());
        let text = text_of(&lines);
        assert!(text.contains("fn main() {}"));
        for line in &lines {
            for span in &line.spans {
                assert_eq!(span.style.bg, None, "code span must not paint background");
            }
        }
    }

    #[test]
    fn wraps_cjk_without_splitting_wide_chars() {
        let lines = render("这是一段需要折行的中文文本，用于验证宽度。", 14, &theme());
        assert!(lines.iter().all(|line| line.width() <= 14));
        assert!(text_of(&lines).contains("中文文本"));
    }

    #[test]
    fn empty_input_still_returns_one_line() {
        let lines = render("", 20, &theme());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.is_empty());
    }
}
