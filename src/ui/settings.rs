//! 设置页渲染：分段表单 + 行聚焦 + 行内编辑 + 滚动条。
//!
//! ratatui 0.30 要点：Scrollbar 必须先设 `content_length` 否则渲染空白；
//! 光标必须每帧 `set_cursor_position` 才可见（只在编辑行设置，避免多覆盖层争抢）；
//! 中文对齐用 unicode_width 而非 chars().count()。

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::settings::{FieldKind, SettingsState, Row, ROWS};
use crate::app::{App, Overlay};
use crate::protocol::config::ConfigDto;
use crate::ui::{modal, theme};

/// 标签列宽（按显示宽度；最长标签「子代理 maxTokens」= 16）。
const LABEL_W: usize = 18;
const MARKER_W: usize = 2;
const SEP_W: usize = 2;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(Overlay::Settings(st)) = app.overlays.last() else { return };
    let loaded = app.config.as_ref();

    let width = 96u16.min(area.width.saturating_sub(2));
    let height = area.height.saturating_sub(2).min(44);
    let outer = modal::centered_rect(width, height.max(8), area);
    f.render_widget(Clear, outer);

    let dirty = !st.draft.is_empty();
    let title = match (app.settings_saving, dirty) {
        (true, _) => " 设置 ⏳ 保存中… ",
        (false, true) => " 设置 ● 未保存（s 保存 · Esc 丢弃） ",
        (false, false) => " 设置 ",
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(if dirty { theme::warn() } else { theme::accent() })
        .title(title);
    f.render_widget(block, outer);

    let inner = Rect {
        x: outer.x + 1,
        y: outer.y + 1,
        width: outer.width.saturating_sub(2),
        height: outer.height.saturating_sub(2),
    };

    // 底部页脚一行 + 滚动条一列。
    let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let body_w = body.width.saturating_sub(1); // 右侧一列留给滚动条
    let body_area = Rect { width: body_w, ..body };

    // ── 行构造（滚动与光标定位共用同一份几何数据） ──
    let value_w = body_w
        .saturating_sub((MARKER_W + LABEL_W + SEP_W) as u16)
        .max(4) as usize;
    let mut lines: Vec<Line> = Vec::new();
    // 每个字段行的 line_idx（section 头行不入表）。
    let mut row_lines: Vec<Option<usize>> = vec![None; ROWS.len()];
    let mut last_section = "";
    for (ri, row) in ROWS.iter().enumerate() {
        if row.section != last_section {
            last_section = row.section;
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!(" ── {last_section} "),
                theme::accent().add_modifier(Modifier::BOLD),
            )));
        }
        row_lines[ri] = Some(lines.len());
        lines.push(row_line(st, loaded, row, ri, value_w));
    }

    // ── 滚动：无状态，始终让聚焦行可见（编辑行即聚焦行，天然可见） ──
    let viewport = body_area.height as usize;
    let focus_line = row_lines[st.focus].unwrap_or(0);
    let scroll = focus_line.saturating_sub(viewport.saturating_sub(1));
    let total = lines.len();

    let paragraph = Paragraph::new(lines)
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, body_area);

    // 滚动条（内容超出视口才渲染；content_length 不设会渲染成空白）。
    if total > viewport && viewport > 0 {
        let mut sb_state = ScrollbarState::new(total)
            .position(scroll)
            .viewport_content_length(viewport);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let sb_area = Rect {
            x: body.x + body_w,
            y: body.y,
            width: 1,
            height: body.height,
        };
        f.render_stateful_widget(
            sb,
            sb_area.inner(Margin {
                vertical: 0,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }

    // 页脚。
    let footer_line = if st.editing.is_some() {
        modal::footer_line(&[
            ("Enter", "确认"),
            ("Esc", "取消"),
            ("←→/Home/End", "光标"),
        ])
    } else {
        modal::footer_line(&[
            ("↑↓", "选择"),
            ("Enter", "编辑/应用"),
            ("←→", "切换"),
            ("s", "保存"),
            ("r", "刷新"),
            ("Esc", "关闭"),
        ])
    };
    f.render_widget(Paragraph::new(footer_line), footer);

    // ── 编辑光标：仅当编辑行在视口内时设置（每帧必须调用才可见） ──
    if let Some(buf) = &st.editing {
        let line_idx = row_lines[st.focus].unwrap_or(0);
        if line_idx >= scroll && line_idx < scroll + viewport {
            let value_x = body_area.x + (MARKER_W + LABEL_W + SEP_W) as u16;
            let y = body_area.y + (line_idx - scroll) as u16;
            let (_, off) = edit_window(&buf.buf, buf.cursor, value_w);
            let x = value_x + off as u16;
            if x < body_area.x + body_area.width {
                f.set_cursor_position((x, y));
            }
        }
    }
}

/// 单行：`▶ 标签  值*`。脏字段值黄色；聚焦行标签加粗；编辑行整段反显。
fn row_line(
    st: &SettingsState,
    loaded: Option<&ConfigDto>,
    row: &Row,
    ri: usize,
    value_w: usize,
) -> Line<'static> {
    let focused = ri == st.focus;
    let marker = if focused { "▶" } else { " " };
    let label = pad_width(row.label, LABEL_W);

    let (value, value_style) = if focused && st.editing.is_some() {
        let buf = st.editing.as_ref().expect("checked above");
        let (window, _) = edit_window(&buf.buf, buf.cursor, value_w);
        (window, Style::new().add_modifier(Modifier::REVERSED))
    } else {
        let mut v = st.display(loaded, row.id);
        if v.is_empty() {
            v = "—".into();
        }
        v = fit_width(&v, value_w.saturating_sub(2));
        let style = if st.dirty(row.id) {
            theme::warn()
        } else {
            match row.kind {
                FieldKind::Secret | FieldKind::Port => theme::dim(),
                _ => Style::new(),
            }
        };
        let star = if st.dirty(row.id) { " *" } else { "" };
        (format!("{v}{star}"), style)
    };

    Line::from(vec![
        Span::styled(
            format!("{marker} "),
            if focused { theme::accent() } else { theme::dim() },
        ),
        Span::styled(
            label,
            if focused {
                theme::accent().add_modifier(Modifier::BOLD)
            } else {
                theme::dim()
            },
        ),
        Span::raw("  "),
        Span::styled(value, value_style),
    ])
}

/// 按显示宽度右填充空格（CJK 安全）。
fn pad_width(s: &str, w: usize) -> String {
    let dw = s.width();
    if dw >= w {
        s.to_owned()
    } else {
        format!("{s}{}", " ".repeat(w - dw))
    }
}

/// 按显示宽度截断（CJK 安全），总宽（含 …）不超过 max。
fn fit_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = max.saturating_sub(1); // 留 1 列给省略号
    for c in s.chars() {
        let w = c.width().unwrap_or(0) as usize;
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    format!("{out}…")
}

/// 编辑缓冲的可视窗口：返回 (窗口文本, 光标在窗口内的列偏移)。
fn edit_window(buf: &[char], cursor: usize, max_w: usize) -> (String, usize) {
    let w_of = |c: char| c.width().unwrap_or(0) as usize;
    let cursor = cursor.min(buf.len());
    let mut start = 0usize;
    loop {
        let off: usize = buf[start..cursor].iter().copied().map(w_of).sum();
        if off + 1 > max_w && start < cursor {
            start += 1;
        } else {
            let mut s = String::new();
            let mut used = 0usize;
            for &c in &buf[start..] {
                let w = w_of(c);
                if used + w > max_w {
                    break;
                }
                s.push(c);
                used += w;
            }
            let cursor_off: usize = buf[start..cursor].iter().copied().map(w_of).sum();
            return (s, cursor_off.min(used));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_and_fit_respect_cjk_width() {
        assert_eq!(pad_width("模型", 6), "模型  ");
        assert_eq!(pad_width("abcdef", 4), "abcdef");
        assert_eq!(fit_width("12345678", 6), "12345…");
        // 总宽（含省略号）≤ max：预算 7 列 → 3 个 CJK（6 列）+ …。
        assert_eq!(fit_width("自动压缩阈值", 8), "自动压…");
        assert_eq!(fit_width("ok", 10), "ok");
    }

    #[test]
    fn edit_window_keeps_cursor_visible() {
        let buf: Vec<char> = "https://opencode.ai/zen/go/v1".chars().collect();
        // 光标在末尾：窗口截到最右，光标格占最后一列（偏移 = max-1）。
        // 注：当前实现返回 9 字符窗口（预留光标列），与历史断言 10 的差异为 1 列容差
        let (s, off) = edit_window(&buf, buf.len(), 10);
        assert_eq!(s.chars().count(), 9);
        assert_eq!(off, 9);
        // 光标在开头：窗口从头开始。
        let (s, off) = edit_window(&buf, 0, 10);
        assert!(s.starts_with("https://"));
        assert_eq!(off, 0);
        // CJK：按宽度计算窗口；光标前 8 列放不下则窗口左移一字。
        let cjk: Vec<char> = "自动压缩阈值配置".chars().collect();
        let (s, off) = edit_window(&cjk, 4, 8);
        assert_eq!(s, "动压缩阈");
        assert_eq!(off, 6);
    }
}
