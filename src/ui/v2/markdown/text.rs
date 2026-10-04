//! 行内样式片段、折行与显示宽度。
//!
//! 两个折行器各管一段：`wrap_styled` 服务于代码块（保留样式片段），
//! `wrap_plain` 服务于表格单元格（词边界优先、标点次之、字符级硬切兜底）。

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 一段带样式的文本，用于拼装 `Line`。
#[derive(Clone)]
pub(super) struct StyledSpan {
    pub(super) text: String,
    pub(super) style: Style,
}

impl StyledSpan {
    pub(super) fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub(super) fn into_span(self) -> Span<'static> {
        Span::styled(self.text, self.style)
    }
}

pub(super) fn wrap_styled(spans: &[StyledSpan], width: usize) -> Vec<Vec<StyledSpan>> {
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

/// 文本的最大显示宽度（多行取最大）。
pub(super) fn display_width(text: &str) -> usize {
    text.split('\n')
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// 最长「不可断单元」的显示宽度——列宽低于它，这个单元就必须被硬切。
///
/// 口径与 [`wrap_plain`] 的断点保持一致：中文按单字可断，ASCII 连续串
/// （`LongalphaToken`、`$145,000`、`ID-AA1001`）算一个不可断单元。
pub(super) fn longest_word_width(text: &str) -> usize {
    text.split('\n')
        .flat_map(break_units)
        .map(|unit| unit.trim().width())
        .max()
        .unwrap_or(0)
}

/// 最宽单个字符的显示宽度——低于它连一个字符都放不下。
///
/// 用 `char` 近似 grapheme cluster：不额外引入 `unicode-segmentation`，代价是
/// emoji ZWJ 序列按多个字符计宽。这个方向只会让地板更保守。
pub(super) fn widest_char_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .max()
        .unwrap_or(0)
}

/// 折行点优先级：空白 → 标点 → 字符级硬切。
const BREAK_AFTER: [char; 11] = [',', '，', '、', ';', '；', ':', '：', '/', '|', '-', '·'];

/// CJK / 假名 / 韩文等「逐字可断」的区段。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
    )
}

/// 把文本切成「不可断单元」：单元以空白、标点结尾，或本身就是单个 CJK 字符。
fn break_units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            current.push(' ');
            units.push(std::mem::take(&mut current));
        } else if BREAK_AFTER.contains(&ch) || is_cjk(ch) {
            current.push(ch);
            units.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

/// 按显示宽度折行（表格单元格用）。返回值至少有一行。
pub(super) fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;

    for unit in break_units(text) {
        if unit.trim().is_empty() {
            if line_w > 0 && line_w < width {
                line.push(' ');
                line_w += 1;
            }
            continue;
        }
        let unit_w = unit.width();
        if line_w > 0 && line_w + unit_w > width {
            push_line(&mut lines, &mut line, &mut line_w);
        }
        if unit_w > width {
            hard_wrap(&mut lines, &mut line, &mut line_w, &unit, width);
            continue;
        }
        line.push_str(&unit);
        line_w += unit_w;
    }

    push_line(&mut lines, &mut line, &mut line_w);
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn push_line(lines: &mut Vec<String>, line: &mut String, line_w: &mut usize) {
    lines.push(line.trim_end().to_string());
    line.clear();
    *line_w = 0;
}

/// 单个单元比整列还宽时的兜底：按字符填满一行再换行。
fn hard_wrap(
    lines: &mut Vec<String>,
    line: &mut String,
    line_w: &mut usize,
    unit: &str,
    width: usize,
) {
    for ch in unit.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *line_w > 0 && *line_w + ch_w > width {
            push_line(lines, line, line_w);
        }
        line.push(ch);
        *line_w += ch_w;
    }
}
