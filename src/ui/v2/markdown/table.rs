//! 表格渲染：列宽分配、单元格折行、边框。
//!
//! 列宽分配学自 grok `xai-grok-markdown` 的 `format_table`——每列先取「最长不可断
//! 单元」作 word floor、「最宽单个字符」作 hard floor，再把剩余预算按各列未满足的
//! 余量（`natural - base`）**比例**分配，而不是把可用宽度平分给每一列。具体见
//! [`column_widths`]。
//!
//! 单元格是**折行**而非截断：断点优先落在空白，其次是 `/ - , 、` 这类标点，中文按
//! 单字可断，只有单个超长 token（URL、哈希）才会降到字符级硬切。行高取本行各格
//! 折行数的最大值。

use pulldown_cmark::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::fg;
use super::text::{display_width, longest_word_width, widest_char_width, wrap_plain};
use crate::theme::Theme;

/// 单表最多渲染的行数，多出的行折叠成一行提示。
pub(super) const MAX_TABLE_ROWS: usize = 32;
/// 列宽绝对下限。
const MIN_TABLE_COL_WIDTH: usize = 3;

pub(super) struct TableState {
    pub(super) alignments: Vec<Alignment>,
    pub(super) headers: Vec<String>,
    pub(super) rows: Vec<Vec<String>>,
    pub(super) row: Vec<String>,
    pub(super) cell: String,
    pub(super) in_head: bool,
}

impl TableState {
    pub(super) fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            headers: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
            cell: String::new(),
            in_head: false,
        }
    }

    pub(super) fn flush_cell(&mut self) {
        self.row.push(std::mem::take(&mut self.cell));
    }

    pub(super) fn flush_row(&mut self) {
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

pub(super) fn render_table(table: TableState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
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
    let avail = width
        .saturating_sub(border_w)
        .max(cols * MIN_TABLE_COL_WIDTH);
    let col_widths = column_widths(&table, cols, avail);

    let top = table_border(&col_widths, '┌', '┬', '┐');
    let middle = table_border(&col_widths, '├', '┼', '┤');
    let bottom = table_border(&col_widths, '└', '┴', '┘');
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(top, fg(theme.markdown.rule))));

    if !table.headers.is_empty() {
        out.extend(table_row(
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
        out.extend(table_row(
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
    // 列数受 `max_cols` 限制，超出的列会整列丢弃——这不是静默操作。
    if requested_cols > cols {
        out.push(Line::from(Span::styled(
            format!("  （表格省略 {} 列）", requested_cols - cols),
            fg(theme.text.dim),
        )));
    }
    out
}

/// 某列的全部单元格（表头 + 所有行）。
fn column_cells(table: &TableState, idx: usize) -> impl Iterator<Item = &str> {
    table
        .headers
        .get(idx)
        .map(String::as_str)
        .into_iter()
        .chain(
            table
                .rows
                .iter()
                .filter_map(move |row| row.get(idx))
                .map(String::as_str),
        )
}

/// 按内容分配列宽。
///
/// 三层宽度：
/// - `natural`：该列所有单元格（含表头）的最大显示宽度；
/// - `word_floor`：该列最长「不可断单元」的宽度——列宽低于它，长 token 只能硬切；
/// - `hard_floor`：该列最宽单个字符的宽度——低于它连一个字都放不下。
///
/// 预算 `avail` 够用就直接用 `natural`（表格只占内容那么宽，不撑满窗格）；不够则
/// 退到 `word_floor`，再不够退到 `hard_floor`，都放不下才按 `hard_floor` 压平。
/// 选定地板后，把剩余预算按各列「未满足的余量」比例分下去：宽列先长，窄列也不会
/// 被拉长到超过自身内容。
///
/// 前置条件：`avail >= cols * MIN_TABLE_COL_WIDTH`（调用方已保证）。
fn column_widths(table: &TableState, cols: usize, avail: usize) -> Vec<usize> {
    let mut natural = vec![MIN_TABLE_COL_WIDTH; cols];
    let mut word_floor = vec![MIN_TABLE_COL_WIDTH; cols];
    let mut hard_floor = vec![MIN_TABLE_COL_WIDTH; cols];

    for idx in 0..cols {
        let mut widest = 0usize;
        let mut word = 0usize;
        let mut hard = 0usize;
        for cell in column_cells(table, idx) {
            widest = widest.max(display_width(cell));
            word = word.max(longest_word_width(cell));
            hard = hard.max(widest_char_width(cell));
        }
        natural[idx] = widest.max(MIN_TABLE_COL_WIDTH);
        word_floor[idx] = word.clamp(MIN_TABLE_COL_WIDTH, natural[idx]);
        hard_floor[idx] = hard.clamp(MIN_TABLE_COL_WIDTH, word_floor[idx]);
    }

    let total = |widths: &[usize]| widths.iter().sum::<usize>();
    if total(&natural) <= avail {
        return natural;
    }
    if total(&word_floor) <= avail {
        return grow(word_floor, &natural, avail);
    }
    if total(&hard_floor) <= avail {
        return grow(hard_floor, &word_floor, avail);
    }
    shrink(&hard_floor, avail)
}

/// 把 `avail - base` 的余量按各列未满足的余量比例分下去，余数给「最欠」的列。
fn grow(base: Vec<usize>, target: &[usize], avail: usize) -> Vec<usize> {
    let mut widths = base;
    let base_total: usize = widths.iter().sum();
    let extra = avail.saturating_sub(base_total);
    let want: Vec<usize> = widths
        .iter()
        .zip(target)
        .map(|(width, target)| target.saturating_sub(*width))
        .collect();
    let total_want: usize = want.iter().sum();
    if extra == 0 || total_want == 0 {
        return widths;
    }

    let mut used = 0usize;
    for (width, want) in widths.iter_mut().zip(&want) {
        let share = want * extra / total_want;
        *width += share;
        used += share;
    }

    // 整除丢掉的零头一次一格地补给最「欠」的列（即 `target - width` 最大的）。
    let mut remaining = extra.saturating_sub(used);
    let mut order: Vec<usize> = (0..widths.len()).collect();
    order.sort_by_key(|&idx| std::cmp::Reverse(target[idx].saturating_sub(widths[idx])));
    while remaining > 0 {
        let mut progressed = false;
        for &idx in &order {
            if remaining == 0 {
                break;
            }
            if widths[idx] < target[idx] {
                widths[idx] += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    widths
}

/// 连字素地板都放不下时的兜底：从当前最宽的列往下压，不低于下限。
fn shrink(floors: &[usize], avail: usize) -> Vec<usize> {
    let mut widths = floors.to_vec();
    let mut total: usize = widths.iter().sum();
    while total > avail {
        let widest = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > MIN_TABLE_COL_WIDTH)
            .max_by_key(|(_, width)| **width)
            .map(|(idx, _)| idx);
        match widest {
            Some(idx) => {
                widths[idx] -= 1;
                total -= 1;
            }
            None => break,
        }
    }
    widths
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

/// 渲染一行单元格；某格内容需要多行时，本行整体加高到最高的那格。
fn table_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    cell_style: Style,
    border_style: Style,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(idx, width)| {
            let raw = cells.get(idx).map(String::as_str).unwrap_or("");
            wrap_cell(raw, *width)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

    let mut out = Vec::with_capacity(height);
    for row_line in 0..height {
        let mut spans = Vec::with_capacity(widths.len() * 2 + 1);
        spans.push(Span::styled("│".to_string(), border_style));
        for (idx, width) in widths.iter().enumerate() {
            let raw = wrapped
                .get(idx)
                .and_then(|lines| lines.get(row_line))
                .map(String::as_str)
                .unwrap_or("");
            spans.push(Span::styled(
                format_cell(raw, *width, alignments.get(idx).copied()),
                cell_style,
            ));
            spans.push(Span::styled("│".to_string(), border_style));
        }
        out.push(Line::from(spans));
    }
    out
}

/// 单元格内容 → 若干行。`\n` 是硬换行（`<br>` 会写进单元格），其余按列宽折行。
fn wrap_cell(raw: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for segment in raw.split('\n') {
        out.extend(wrap_plain(segment, width));
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 单行单元格内容补齐到列宽；只在真的溢出时截断（`==` 时整格正好填满）。
fn format_cell(raw: &str, width: usize, align: Option<Alignment>) -> String {
    let raw = raw.replace('\n', " ");
    let display_width = raw.width();
    if display_width > width {
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
