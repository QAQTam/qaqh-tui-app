//! Markdown → RenderLine 渲染（首版：标题/粗斜/行内码/围栏代码/链接/引用/分割线/列表/表格）。
//!
//! 批处理纪律：流式 `is_streaming=true` 时调用方应跳过本模块（纯文本+▌），
//! 仅在 `BlockCheckpoint/TurnCompleted` 后对单块落盘文本调用，避免半截 markdown 高亮抖动。
//! 表格采用栅格化 RenderLine，不依赖 ratatui Table widget，保持单 Paragraph 滚动链路。

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color as RatColor, Modifier, Style as RatStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::render_line::{wrap_text, RenderLine, SpanStyle};
use unicode_width::UnicodeWidthChar;

const CODE_INDENT: &str = "  ";
const QUOTE_PREFIX: &str = "▎ ";

static PS: OnceLock<SyntaxSet> = OnceLock::new();
static TS: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    PS.get_or_init(SyntaxSet::load_defaults_newlines)
}
fn theme_set() -> &'static ThemeSet {
    TS.get_or_init(ThemeSet::load_defaults)
}

fn syntect_to_ratatui(s: syntect::highlighting::Style) -> RatStyle {
    let fg = RatColor::Rgb(s.foreground.r, s.foreground.g, s.foreground.b);
    let bg = RatColor::Rgb(s.background.r, s.background.g, s.background.b);
    let mut style = RatStyle::new().fg(fg).bg(bg);
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn render_highlighted_code_block(text: &str, lang: Option<&str>, width: usize) -> Vec<RenderLine> {
    let ps = syntax_set();
    let ts = theme_set();
    // 主题选用深色：base16-ocean.dark 为 syntect 默认深色，与 MdCodeBlock 背景 Indexed(236) 接近
    let theme = &ts.themes["base16-ocean.dark"];
    let syntax = lang
        .and_then(|l| ps.find_syntax_by_token(l))
        .or_else(|| lang.and_then(|l| ps.find_syntax_by_extension(l)))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    let max_w = width.saturating_sub(CODE_INDENT.len()).max(10);
    for line in LinesWithEndings::from(text) {
        // 去掉 LinesWithEndings 自带的换行符，保留空行
        let line = line.trim_end_matches("\r\n").trim_end_matches('\n');
        if line.is_empty() {
            out.push(RenderLine::new().span_direct(CODE_INDENT, RatStyle::new().bg(RatColor::Indexed(236))));
            continue;
        }
        let ranges = match h.highlight_line(line, ps) {
            Ok(r) => r,
            Err(_) => {
                // 降级：纯色块
                for seg in wrap_text(line, max_w) {
                    out.push(RenderLine::new().span_direct(format!("{CODE_INDENT}{seg}"), RatStyle::new().fg(RatColor::White).bg(RatColor::Indexed(236))));
                }
                continue;
            }
        };
        // 将高亮 ranges 转为单个 RenderLine（可能需硬截）
        let mut cur_line = RenderLine::new();
        cur_line.spans.push(crate::app::render_line::RenderSpan::with_style(CODE_INDENT, RatStyle::new().bg(RatColor::Indexed(236))));
        let mut line_w: usize = 0;
        for (style, txt) in ranges {
            let rat_style = syntect_to_ratatui(style);
            // 按 max_w 硬截：若超宽则拆行
            let mut remaining = txt;
            while !remaining.is_empty() {
                let avail = max_w.saturating_sub(line_w);
                if avail == 0 {
                    out.push(std::mem::take(&mut cur_line));
                    cur_line.spans.push(crate::app::render_line::RenderSpan::with_style(CODE_INDENT, RatStyle::new().bg(RatColor::Indexed(236))));
                    line_w = 0;
                    continue;
                }
                // 按显示宽度截取
                let take = take_width(remaining, avail);
                let (head, tail) = remaining.split_at(take);
                cur_line.spans.push(crate::app::render_line::RenderSpan::with_style(head, rat_style));
                line_w += head.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as usize).sum::<usize>();
                remaining = tail;
                if !remaining.is_empty() {
                    out.push(std::mem::take(&mut cur_line));
                    cur_line.spans.push(crate::app::render_line::RenderSpan::with_style(CODE_INDENT, RatStyle::new().bg(RatColor::Indexed(236))));
                    line_w = 0;
                }
            }
        }
        out.push(cur_line);
        // 流式保护：单块超 500 行截断由调用方处理，此处不再截
    }
    if out.is_empty() {
        out.push(RenderLine::new().span_direct(CODE_INDENT, RatStyle::new().bg(RatColor::Indexed(236))));
    }
    out
}

fn take_width(s: &str, max_w: usize) -> usize {
    let mut used = 0usize;
    let mut idx = 0usize;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as usize;
        if used + w > max_w {
            break;
        }
        used += w;
        idx = i + c.len_utf8();
    }
    if idx == 0 && !s.is_empty() {
        // 至少取一个字符
        s.chars().next().map(|c| c.len_utf8()).unwrap_or(0)
    } else {
        idx
    }
}

#[allow(dead_code)]
fn wrap_code_block(text: &str, width: usize) -> Vec<String> {
    let w = width.saturating_sub(CODE_INDENT.len()).max(10);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() { out.push(String::new()); continue; }
        for seg in wrap_text(line, w) { out.push(seg); }
    }
    if out.is_empty() { out.push(String::new()); }
    out
}

#[allow(dead_code)]
fn is_markdown_char(_c: char) -> bool {
    // 保留作扩展点，未直接使用（Parser 为准），避免 dead_code 告警由 allow 覆盖
    true
}

/// 检测单字符强调 `_..._` / `*...*`（非 `**`/`__`），避免 is_markdown 漏判导致符号残留。
/// 规则：成对出现、中间非空非纯空白、开分隔后/闭分隔前不为空白；`_` 需非词内（`foo_bar` 不是强调）。
fn has_single_emphasis(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let is_word_char = |ch: char| ch.is_alphanumeric() || ch == '_';
    for i in 0..n {
        let c = chars[i];
        if c != '*' && c != '_' {
            continue;
        }
        // 跳过 ** / __ 已由外层 contains 覆盖；此处只关心单分隔
        if i + 1 < n && chars[i + 1] == c {
            continue;
        }
        if i > 0 && chars[i - 1] == c {
            continue;
        }
        // 向后寻找同类单闭分隔
        for j in i + 1..n {
            if chars[j] != c {
                continue;
            }
            if j + 1 < n && chars[j + 1] == c {
                continue;
            }
            if j > 0 && chars[j - 1] == c {
                continue;
            }
            let between_len = j - i - 1;
            if between_len == 0 {
                break;
            }
            let between: String = chars[i + 1..j].iter().collect();
            if between.trim().is_empty() {
                break;
            }
            let after_open = chars[i + 1];
            let before_close = chars[j - 1];
            if after_open == ' ' || after_open == '\n' || after_open == '\r' || after_open == '\t' {
                break;
            }
            if before_close == ' ' || before_close == '\n' || before_close == '\r' || before_close == '\t' {
                break;
            }
            if c == '_' {
                let before_open = if i > 0 { Some(chars[i - 1]) } else { None };
                let after_close = if j + 1 < n { Some(chars[j + 1]) } else { None };
                if let (Some(bo), Some(ac)) = (before_open, after_close) {
                    if is_word_char(bo) && is_word_char(ac) {
                        // 词内下划线如 foo_bar_baz，按 CommonMark 不视为强调，继续找下一闭合
                        continue;
                    }
                }
            }
            return true;
        }
    }
    false
}

/// 轻量探测：含典型 md 标记才走 Parser，否则回退 wrap_text（省 CPU）。
/// 已放宽至单 `*`/`_` 强调与 `~~` 删除线，解决符号残留与 konsole SGR3 不可见问题。
pub fn is_markdown(text: &str) -> bool {
    // 快速路径：围栏/行内码/粗斜双字符/删除线
    if text.contains("```") || text.contains("``") || text.contains("**") || text.contains("__") || text.contains('`') || text.contains("~~") {
        return true;
    }
    // 表格：含 | 且含分隔线，或 | >=3
    let pipes = text.chars().filter(|&c| c == '|').count();
    if (text.contains('|') && text.contains("---")) || pipes >= 3 {
        return true;
    }
    // 块级标记：标题/列表/引用/分割线
    if text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("# ") || t.starts_with("## ") || t.starts_with("### ")
            || t.starts_with("- ") || t.starts_with("* ") || t.starts_with("1. ")
            || t.starts_with("> ") || t.starts_with("---") || t.starts_with("***")
    }) {
        return true;
    }
    if text.contains('[') && text.contains("](") {
        return true;
    }
    let backticks = text.chars().filter(|&c| c == '`').count();
    if backticks >= 2 {
        return true;
    }
    if has_single_emphasis(text) {
        return true;
    }
    false
}

#[derive(Debug, Clone)]
struct TableState {
    alignments: Vec<Alignment>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    in_head: bool,
    cur_row: Vec<String>,
    cur_cell: String,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self { alignments, headers: Vec::new(), rows: Vec::new(), in_head: false, cur_row: Vec::new(), cur_cell: String::new() }
    }
    fn flush_cell(&mut self) {
        let cell = std::mem::take(&mut self.cur_cell).trim().to_string();
        self.cur_row.push(cell);
    }
    fn flush_row(&mut self) {
        if self.cur_row.is_empty() { return; }
        let row = std::mem::take(&mut self.cur_row);
        if self.in_head && self.headers.is_empty() {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }
}

/// 带样式的整段折行（跨 run 协同）。
///
/// 背景：旧实现按“同样式 run”分别调 `wrap_text`，每个 run 从行首重新折行，
/// 导致 `**bold** and *italic*` 这类混合段落被拆成一行一个碎片（异常换行）。
/// 本实现把段落展平为 (char, style) 流后统一贪心折行：优先在最近空格断行，
/// 无空格时硬断（CJK 宽度感知），行内保留样式。语义与 `wrap_text` 对齐。
fn wrap_spans(spans: &[(String, SpanStyle)], width: usize) -> Vec<Vec<(String, SpanStyle)>> {
    if width == 0 {
        return vec![spans.to_vec()];
    }
    // 展平为字符流（保留样式）
    let mut chars: Vec<(char, SpanStyle)> = Vec::new();
    for (text, style) in spans {
        chars.reserve(text.chars().count());
        for c in text.chars() {
            chars.push((c, *style));
        }
    }
    let mut out: Vec<Vec<(String, SpanStyle)>> = Vec::new();
    let mut start = 0usize; // 当前行在 chars 中的起点
    let mut line_w = 0usize; // 当前行显示宽度
    let mut last_ws: Option<usize> = None; // 最近空格下标
    let mut i = 0usize;
    while i < chars.len() {
        let (c, _st) = chars[i];
        if c == '\n' {
            // 硬换行（HardBreak）：立即落盘当前行，丢弃换行符
            if start < i {
                push_line(&mut out, &chars[start..i]);
            }
            start = i + 1;
            line_w = 0;
            last_ws = None;
            i += 1;
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if line_w + cw > width {
            if c == ' ' {
                // 行尾空格本身是断点：丢弃之
                if start < i {
                    push_line(&mut out, &chars[start..i]);
                }
                start = i + 1;
                line_w = 0;
                last_ws = None;
                i += 1;
                continue;
            }
            if let Some(bp) = last_ws {
                // 从最近空格处断行（空格丢弃）
                if start < bp {
                    push_line(&mut out, &chars[start..bp]);
                }
                start = bp + 1;
                line_w = chars[start..i]
                    .iter()
                    .map(|(ch, _)| ch.width().unwrap_or(0))
                    .sum();
                last_ws = None;
            } else {
                // 硬断：至少放一个字符（防 CJK 宽 2 > width 时死循环）
                let end = if start == i { i + 1 } else { i };
                push_line(&mut out, &chars[start..end]);
                start = end;
                line_w = 0;
                last_ws = None;
                i = end;
                continue;
            }
            // 断行后重新处理当前字符（i 不变）
            continue;
        }
        if c == ' ' {
            last_ws = Some(i);
        }
        line_w += cw;
        i += 1;
    }
    if start < chars.len() {
        push_line(&mut out, &chars[start..]);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

/// 把一段 (char, style) 合并为连续同样式 span 序列。
fn push_line(out: &mut Vec<Vec<(String, SpanStyle)>>, seg: &[(char, SpanStyle)]) {
    let mut cur: Vec<(String, SpanStyle)> = Vec::new();
    for (c, st) in seg {
        if let Some((last, last_st)) = cur.last_mut()
            && *last_st == *st
        {
            last.push(*c);
            continue;
        }
        cur.push((c.to_string(), *st));
    }
    out.push(cur);
}

pub fn render_markdown(text: &str, width: usize) -> Vec<RenderLine> {
    if width == 0 || text.is_empty() {
        return vec![RenderLine::plain("")];
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, opts);

    let mut out: Vec<RenderLine> = Vec::new();
    let mut style_stack: Vec<SpanStyle> = Vec::new();
    let mut cur_spans: Vec<(String, SpanStyle)> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut list_depth: usize = 0;
    let mut in_code_block = false;
    let mut code_block_lang: Option<String> = None;
    let mut table: Option<TableState> = None;
    let mut link_dest: Option<String> = None;

    let cur_style = |stack: &[SpanStyle]| stack.last().copied().unwrap_or(SpanStyle::Plain);

    // 段落整体落盘：跨 run 协同折行（修复样式 run 各自 wrap 导致的碎片行）。
    // quote=true 时每行加引用前缀，折行可用宽度相应扣除前缀。
    let flush_para = |spans: &mut Vec<(String, SpanStyle)>, out: &mut Vec<RenderLine>, width: usize, quote: bool| {
        if spans.is_empty() {
            return;
        }
        let avail = if quote { width.saturating_sub(QUOTE_PREFIX.len()) } else { width };
        let rows = wrap_spans(spans, avail);
        spans.clear();
        for row in rows {
            let mut line = RenderLine::new();
            if quote {
                line = line.span(QUOTE_PREFIX, SpanStyle::MdQuote);
            }
            for (t, s) in row {
                line = line.span(t, s);
            }
            out.push(line);
        }
    };

    let flush_table = |tbl: TableState, out: &mut Vec<RenderLine>, width: usize| {
        if tbl.headers.is_empty() && tbl.rows.is_empty() { return; }
        let cols = tbl.headers.len().max(tbl.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if cols == 0 { return; }
        // 列宽：等分，可用 width 减边框( cols+1 )
        let border_w = cols + 1;
        let avail = width.saturating_sub(border_w).max(cols * 3);
        let col_w = (avail / cols).max(3);
        let mut col_widths = vec![col_w; cols];
        // 若剩余空间，按最长单元格微调（限幅）
        let rem = avail.saturating_sub(col_w * cols);
        if rem > 0 {
            for i in 0..rem.min(cols) { col_widths[i] += 1; }
        }

        let hline = |widths: &[usize]| -> String {
            let mut s = String::from("┌");
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(*w));
                if i + 1 < widths.len() { s.push('┬'); } else { s.push('┐'); }
            }
            s
        };
        let mline = |widths: &[usize]| -> String {
            let mut s = String::from("├");
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(*w));
                if i + 1 < widths.len() { s.push('┼'); } else { s.push('┤'); }
            }
            s
        };
        let bline = |widths: &[usize]| -> String {
            let mut s = String::from("└");
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(*w));
                if i + 1 < widths.len() { s.push('┴'); } else { s.push('┘'); }
            }
            s
        };

        out.push(RenderLine::new().span(hline(&col_widths), SpanStyle::MdRuler));
        // 表头
        if !tbl.headers.is_empty() {
            let mut line = RenderLine::new().span("│", SpanStyle::MdRuler);
            for (i, h) in tbl.headers.iter().enumerate() {
                let w = col_widths[i];
                let cell = format_cell(h, w, tbl.alignments.get(i).copied());
                line = line.span(cell, SpanStyle::MdTableHead).span("│", SpanStyle::MdRuler);
            }
            out.push(line);
            out.push(RenderLine::new().span(mline(&col_widths), SpanStyle::MdRuler));
        }
        let max_rows = 32usize;
        let omitted = tbl.rows.len().saturating_sub(max_rows);
        for row in tbl.rows.iter().take(max_rows) {
            let mut line = RenderLine::new().span("│", SpanStyle::MdRuler);
            for i in 0..cols {
                let w = col_widths[i];
                let raw = row.get(i).map(|s| s.as_str()).unwrap_or("");
                let cell = format_cell(raw, w, tbl.alignments.get(i).copied());
                line = line.span(cell, SpanStyle::MdTableCell).span("│", SpanStyle::MdRuler);
            }
            out.push(line);
        }
        if omitted > 0 {
            out.push(RenderLine::new().span(format!("  （表格省略 {omitted} 行）"), SpanStyle::Dim));
        }
        out.push(RenderLine::new().span(bline(&col_widths), SpanStyle::MdRuler));
    };

    for ev in parser {
        // 表格内：只收集字符，忽略样式栈
        if let Some(tbl) = table.as_mut() {
            match ev {
                Event::Start(Tag::TableHead) => { tbl.in_head = true; },
                Event::End(TagEnd::TableHead) => { tbl.flush_row(); tbl.in_head = false; },
                Event::Start(Tag::TableRow) => tbl.cur_row.clear(),
                Event::End(TagEnd::TableRow) => tbl.flush_row(),
                Event::Start(Tag::TableCell) => tbl.cur_cell.clear(),
                Event::End(TagEnd::TableCell) => tbl.flush_cell(),
                Event::Text(t) | Event::Code(t) => tbl.cur_cell.push_str(&t),
                Event::SoftBreak | Event::HardBreak => tbl.cur_cell.push(' '),
                Event::End(TagEnd::Table) => {
                    let taken = table.take().unwrap();
                    flush_para(&mut cur_spans, &mut out, width, false);
                    flush_table(taken, &mut out, width);
                }
                _ => {}
            }
            continue;
        }

        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_para(&mut cur_spans, &mut out, width, quote_depth > 0);
                    let st = match level {
                        pulldown_cmark::HeadingLevel::H1 => SpanStyle::MdH1,
                        pulldown_cmark::HeadingLevel::H2 => SpanStyle::MdH2,
                        _ => SpanStyle::MdH3,
                    };
                    style_stack.push(st);
                }
                Tag::Strong | Tag::Emphasis => {
                    let st = if matches!(tag, Tag::Strong) { SpanStyle::Bold } else { SpanStyle::Italic };
                    style_stack.push(st);
                }
                Tag::Strikethrough => style_stack.push(SpanStyle::Dim),
                Tag::Link { dest_url, .. } => {
                    link_dest = Some(dest_url.to_string());
                    style_stack.push(SpanStyle::MdLink);
                }
                Tag::BlockQuote(_) => {
                    flush_para(&mut cur_spans, &mut out, width, false);
                    style_stack.push(SpanStyle::MdQuote);
                    quote_depth += 1;
                }
                Tag::List(_) => list_depth += 1,
                Tag::Item => {
                    flush_para(&mut cur_spans, &mut out, width, false);
                    let indent = "  ".repeat(list_depth.saturating_sub(1));
                    cur_spans.push((format!("{indent}• "), SpanStyle::Dim));
                }
                Tag::CodeBlock(kind) => {
                    flush_para(&mut cur_spans, &mut out, width, false);
                    in_code_block = true;
                    code_block_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(c) => Some(c.to_string()),
                        _ => None,
                    };
                    let _ = code_block_lang;
                }
                Tag::Table(alignment) => {
                    flush_para(&mut cur_spans, &mut out, width, false);
                    table = Some(TableState::new(alignment));
                }
                Tag::Paragraph => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) | TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => {
                    style_stack.pop();
                    if matches!(tag, TagEnd::Link) {
                        if let Some(dest) = link_dest.take() {
                            cur_spans.push((format!(" ({dest})"), SpanStyle::Dim));
                        }
                    }
                }
                TagEnd::Paragraph => {
                    // 段落结束：落盘并空行
                    if in_code_block {
                        // 代码块内段落不额外空行
                    } else if !cur_spans.is_empty() {
                        flush_para(&mut cur_spans, &mut out, width, quote_depth > 0);
                        out.push(RenderLine::new());
                    } else {
                        // 空段落：仍保留空行分隔
                        if out.last().is_some_and(|l| !l.spans.is_empty()) {
                            out.push(RenderLine::new());
                        }
                    }
                }
                TagEnd::Item => {
                    // 列表项前缀（• 或 [x]/[ ]）宽度从折行可用宽度中扣除，防超宽截断
                    let prefix_w = if cur_spans.iter().any(|(t, _)| t.starts_with("[ ] ") || t.starts_with("[x] ")) {
                        4
                    } else {
                        2
                    };
                    flush_para(&mut cur_spans, &mut out, width.saturating_sub(prefix_w), false);
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 { out.push(RenderLine::new()); }
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    code_block_lang = None;
                    out.push(RenderLine::new());
                }
                TagEnd::BlockQuote(_) => {
                    style_stack.pop();
                    quote_depth = quote_depth.saturating_sub(1);
                    flush_para(&mut cur_spans, &mut out, width, false);
                    out.push(RenderLine::new());
                }
                _ => {}
            },
            Event::Text(t) => {
                let st = cur_style(&style_stack);
                if in_code_block {
                    let hl = render_highlighted_code_block(&t, code_block_lang.as_deref(), width);
                    out.extend(hl);
                } else {
                    // 引用/普通段落统一收集：换行由 wrap_spans 跨 run 协同处理（见 flush_para）
                    cur_spans.push((t.to_string(), st));
                }
            }
            Event::Code(t) => {
                // 行内码
                cur_spans.push((t.to_string(), SpanStyle::MdInlineCode));
            }
            Event::SoftBreak => cur_spans.push((" ".into(), cur_style(&style_stack))),
            Event::HardBreak => {
                cur_spans.push(("\n".into(), cur_style(&style_stack)));
                flush_para(&mut cur_spans, &mut out, width, quote_depth > 0);
            }
            Event::Rule => {
                flush_para(&mut cur_spans, &mut out, width, false);
                out.push(RenderLine::new().span("─".repeat(width.min(40)), SpanStyle::MdRuler));
                out.push(RenderLine::new());
            }
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                cur_spans.push((mark.to_string(), SpanStyle::Dim));
            }
            Event::FootnoteReference(_) => {}
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                cur_spans.push((t.to_string(), SpanStyle::MdInlineCode));
            }
            Event::InlineHtml(_) | Event::Html(_) => {}
        }
    }
    flush_para(&mut cur_spans, &mut out, width, quote_depth > 0);
    if let Some(tbl) = table.take() { flush_table(tbl, &mut out, width); }
    // 去重尾部多空行
    while out.len() >= 2 && out[out.len()-1].spans.is_empty() && out[out.len()-2].spans.is_empty() {
        out.pop();
    }
    if out.is_empty() { out.push(RenderLine::plain("")); }
    out
}

fn format_cell(raw: &str, width: usize, align: Option<Alignment>) -> String {
    use unicode_width::UnicodeWidthStr;
    let raw = raw.replace('\n', " ");
    let dw = raw.width();
    if dw >= width {
        // 截断 + …
        let mut out = String::new();
        let mut used = 0usize;
        for c in raw.chars() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w + 1 > width { break; }
            out.push(c); used += w;
        }
        let pad = width.saturating_sub(out.width() + 1);
        return format!("{out}…{}", " ".repeat(pad));
    }
    let pad = width.saturating_sub(dw);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::render_line::RenderStyle;

    #[test]
    fn detects_markdown() {
        assert!(is_markdown("# hi"));
        assert!(is_markdown("**bold**"));
        assert!(is_markdown("| a | b |\n|---|---|"));
        assert!(!is_markdown("hello world"));
    }

    #[test]
    fn wrap_keeps_line_integrity() {
        // 回归：混合样式段落必须跨 run 协同折行，不得拆成碎片行
        // （root cause：旧 flush_para 对每个样式 run 独立 wrap_text）。
        let cases: &[(&str, usize, usize)] = &[
            ("**bold** and *italic* and `code` and plain text", 40, 1),
            ("[text](https://example.com) and more text after", 40, 2),
            ("> quote **bold** inside", 40, 1),
            ("这是**加粗**的一段中文文本内容测试换行", 20, 2),
            ("[a](u1) [b](u2) middle", 40, 1),
        ];
        let line_width = |l: &RenderLine| -> usize {
            l.spans.iter()
                .map(|s| s.text.chars().map(UnicodeWidthChar::width).map(|w| w.unwrap_or(0) as usize).sum::<usize>())
                .sum()
        };
        for (md, w, expect_lines) in cases {
            let lines = render_markdown(md, *w);
            let content: Vec<&RenderLine> = lines.iter().filter(|l| !l.spans.is_empty()).collect();
            assert_eq!(content.len(), *expect_lines, "md={md:?} 内容行数应为 {expect_lines}");
            for l in &content {
                let dw = line_width(l);
                assert!(dw <= *w, "md={md:?} 行宽 {dw} 超限 {w}");
            }
        }
        // 引用前缀必须完整保留
        let quote = render_markdown("> quote **bold** inside", 40);
        let text: String = quote.iter().flat_map(|l| l.spans.iter().map(|s| s.text.as_str())).collect();
        assert!(text.starts_with("▎ quote bold inside"), "引用行应保留 ▎ 前缀且不拆行，got: {text:?}");
    }

    #[test]
    fn wrap_handles_hardbreak_and_list() {
        // 行尾两个空格 + 换行 = HardBreak（CommonMark）
        let lines = render_markdown("line1  \nline2", 40);
        let content: Vec<&RenderLine> = lines.iter().filter(|l| !l.spans.is_empty()).collect();
        assert_eq!(content.len(), 2, "HardBreak 应产生 2 行");

        let list = render_markdown("- first **bold** item text\n- second item", 30);
        let content: Vec<&RenderLine> = list.iter().filter(|l| !l.spans.is_empty()).collect();
        assert_eq!(content.len(), 2, "两个列表项应各为 1 行");
        let merged: String = content.iter().flat_map(|l| l.spans.iter().map(|s| s.text.as_str())).collect();
        assert!(merged.starts_with("• first bold item text"), "列表项不得拆分，got: {merged:?}");
        let dw_map: Vec<usize> = content.iter().map(|l| {
            l.spans.iter()
                .map(|s| s.text.chars().map(UnicodeWidthChar::width).map(|w| w.unwrap_or(0) as usize).sum::<usize>())
                .sum()
        }).collect();
        assert!(dw_map.iter().all(|w| *w <= 30), "列表行宽不得超 30，got {dw_map:?}");
    }



    #[test]
    fn renders_heading_and_bold() {
        let lines = render_markdown("# Title\n\n**bold** and *italic* `code`", 40);
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.style == RenderStyle::Semantic(SpanStyle::MdH1))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.style == RenderStyle::Semantic(SpanStyle::Bold))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.style == RenderStyle::Semantic(SpanStyle::MdInlineCode))));
    }

    #[test]
    fn renders_table() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let lines = render_markdown(md, 20);
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains('┌'))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.style == RenderStyle::Semantic(SpanStyle::MdTableHead))));
    }

    #[test]
    fn renders_code_block() {
        let md = "```rs\nfn main() {}\n```";
        let lines = render_markdown(md, 30);
        // 高亮后为 Direct(Rgb) 风格，不再是 Semantic MdCodeBlock，检查 Direct 存在且含代码
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| matches!(s.style, RenderStyle::Direct(_)))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains("fn"))));
    }

    #[test]
    fn perf_baseline() {
        use std::time::Instant;
        let chunk = "# Title\n\n**bold** text with `inline` and [link](https://example.com)\n\n```rs\nfn main() { println!(\"hello\"); }\n```\n\n| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n";
        let md = chunk.repeat(20); // ~5k
        let start = Instant::now();
        for _ in 0..100 {
            let _ = render_markdown(&md, 80);
        }
        let elapsed = start.elapsed();
        eprintln!("perf markdown 5k x100: {:?} avg {:?}", elapsed, elapsed/100);
        assert!(elapsed.as_millis() < 2000, "markdown 5k x100 should be <2s, got {:?}", elapsed);

        // 大表格 50 行
        let big_table = (0..50).map(|i| format!("| {} | {} | {} |", i, i*2, i*3)).collect::<Vec<_>>().join("\n");
        let md_table = format!("| a | b | c |\n|---|---|---|\n{}", big_table);
        let start = Instant::now();
        let lines = render_markdown(&md_table, 80);
        eprintln!("big table 50 rows -> {} lines in {:?}", lines.len(), start.elapsed());
        assert!(lines.len() < 600);
        assert!(start.elapsed().as_millis() < 500);

        // 流式批处理：1000 次增量 wrap_text（模拟 streaming）
        let mut s = String::new();
        let start = Instant::now();
        for i in 0..1000 {
            s.push('a');
            let _ = crate::app::render_line::wrap_text(&format!("{}▌", s), 80);
            if i % 100 == 0 { let _ = render_markdown(&s, 80); }
        }
        eprintln!("streaming 1000 inc: {:?}", start.elapsed());
        assert!(start.elapsed().as_millis() < 1000);
    }

    #[test]
    fn perf_cjk_150tps() {
        use std::time::Instant;
        // 150 tokens/s ~ 225 CJK chars/s（1 token≈1.5 CJK），5s 模拟 750 tokens ≈1125 chars
        let cjk_tokens = ["自动", "压缩", "阈值", "配置", "子代理", "工具", "模型", "思考", "强度", "表格", "代码", "高亮", "流式", "批处理", "内存", "控制"];
        let mut s = String::new();
        let start = Instant::now();
        for i in 0..750 {
            s.push_str(cjk_tokens[i % cjk_tokens.len()]);
            // 每 token 触发一次 wrap_text（流式路径，禁 markdown）
            let _ = crate::app::render_line::wrap_text(&format!("{}▌", s), 80);
            // 每 150 tokens（1s）批一次 markdown 富化
            if i % 150 == 149 {
                let lines = render_markdown(&s, 80);
                // CJK 切片必须按 width 2 且不 panic，且行宽 ≤80
                for l in &lines {
                    let w: usize = l.spans.iter().map(|sp| sp.text.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as usize).sum::<usize>()).sum();
                    assert!(w <= 80, "CJK line width overflow: {} vs 80", w);
                }
                assert!(lines.len() < 500, "CJK 150tps batch should be <500 lines");
            }
            // 模拟 150 tokens/s 的字节切片安全：按 1 char 递增不 panic
            let _ = take_width(&s, 80);
        }
        let elapsed = start.elapsed();
        eprintln!("CJK 150tps 750 tokens (5s) in {:?} ({:.0} tokens/s)", elapsed, 750.0 / elapsed.as_secs_f64());
        assert!(elapsed.as_millis() < 1000, "CJK 150tps 5s should be <1s, got {:?}", elapsed);
        // 表格 CJK 混合
        let cjk_table = "| 模型 | 描述 |\n|---|---|\n| 自动压缩 | 阈值配置 |\n| 子代理工具 | 全部工具 |\n";
        let lines = render_markdown(cjk_table, 40);
        assert!(lines.iter().any(|l| l.spans.iter().any(|sp| sp.text.contains('┌'))));
        eprintln!("CJK table lines: {}", lines.len());
    }

    #[test]
    fn perf_transcript_400() {
        use crate::app::render_transcript::render_transcript;
        use crate::app::session::SessionState;
        use crate::app::timeline_model::{Block, Round, TimelineModel, Turn};
        use crate::protocol::timeline::{TimelineBlockKind, TimelineBlockState, TimelineTurnState};
        use std::time::Instant;
        let mut model = TimelineModel::default();
        let chunk = "# H1\n\nBold **text** `code`  ".repeat(10); // ~200 chars per turn
        for i in 0..400 {
            let text = format!("回合 {i} {}", chunk);
            let block = Block { block_id: format!("b{i}"), block_order: 0, kind: TimelineBlockKind::Text, state: TimelineBlockState::Sealed, text, tool: None, last_fragment: 0 };
            let round = Round { round_num: 0, sealed: true, is_final: true, blocks: vec![block] };
            let turn = Turn { turn_id: format!("t{i}"), user_text: format!("user {i}"), state: TimelineTurnState::Completed, failure: None, rounds: vec![round] };
            model.turns.push(turn);
        }
        let mut sess = SessionState::new("perf".into());
        sess.timeline = model;
        sess.timeline.version = 1;
        let start = Instant::now();
        let lines = render_transcript(&sess, 80);
        let elapsed = start.elapsed();
        eprintln!("transcript 400 turns -> {} lines in {:?}", lines.len(), elapsed);
        // 预估内存：每行平均 ~80 chars + 2 spans ~100B => 25600*100B ~2.5MB，远 <100MB
        assert!(lines.len() < 40000, "400 turns should be <40000 lines, got {}", lines.len());
        assert!(elapsed.as_millis() < 500, "400 turns render should be <500ms, got {:?}", elapsed);
        // 模拟流式增量：单回合追加 100 次，每次仅重算 active（缓存命中 width 相同则快）
        // 注：直接 render_transcript 无 App 缓存，每次 38ms，100x ~3.8s 仍 <5s；若走 ensure_render_caches 则 <100ms
        let start = Instant::now();
        for _ in 0..100 {
            let _ = render_transcript(&sess, 80);
        }
        eprintln!("cached re-render 100x: {:?}", start.elapsed());
        assert!(start.elapsed().as_millis() < 5000);
    }
}
