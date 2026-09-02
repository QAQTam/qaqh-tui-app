//! Composer：输入行 + 附件标记 + 流式相位。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use unicode_width::UnicodeWidthChar;

use crate::app::App;
use crate::app::render_line::edit_window;
use crate::ui::theme;

/// composer 显示行数上限（超出后以尾部窗口展示，光标行恒可见）。
const MAX_ROWS: usize = 6;

/// 自适应高度：上下边框 + min(输入行数, MAX_ROWS)。
pub fn height(app: &App) -> u16 {
    let rows = app.active_session().map(|s| s.composer.rows()).unwrap_or(1);
    (rows.clamp(1, MAX_ROWS) as u16).saturating_add(2)
}

pub fn draw_slash_menu(f: &mut Frame, app: &App, composer_area: Rect) {
    if !app.overlays.is_empty() {
        return;
    }
    let candidates = app.slash_candidates();
    if candidates.is_empty() {
        return;
    }
    let selected = app.slash_selected.min(candidates.len().saturating_sub(1));
    // 在 composer 上方弹出，最多 5 行
    let visible = candidates.iter().take(6).collect::<Vec<_>>();
    let h = (visible.len() as u16).min(6) + 2; // border
    let w = 58u16.min(composer_area.width.saturating_sub(2));
    let menu_area = Rect {
        x: composer_area.x + 2,
        y: composer_area.y.saturating_sub(h),
        width: w,
        height: h,
    };
    if menu_area.width < 20 || menu_area.height < 3 {
        return;
    }
    f.render_widget(Clear, menu_area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(" / 命令 · Tab 补全 · ↑↓ 选择 · Enter 执行 · Esc 关闭 ");
    let inner = Rect {
        x: menu_area.x + 1,
        y: menu_area.y + 1,
        width: menu_area.width.saturating_sub(2),
        height: menu_area.height.saturating_sub(2),
    };
    f.render_widget(block, menu_area);
    let mut lines: Vec<Line> = Vec::new();
    for (idx, def) in visible.iter().enumerate() {
        let is_sel = idx == selected;
        let marker = if is_sel { "▸" } else { " " };
        let style = if is_sel {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if is_sel {
                    theme::accent()
                } else {
                    theme::dim()
                },
            ),
            Span::styled(format!("/{:<10}", def.name), style),
            Span::styled(def.desc.to_string(), theme::dim()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
    // 选中项的 hint 画在最后一行下方（若空间允许，已在标题中展示 Tab 提示）
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(sess) = app.active_session() else {
        f.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme::dim())
                .title(" 无活动会话 — Ctrl+T 新建 / Ctrl+L 列表 "),
            area,
        );
        return;
    };

    let mut title = String::new();
    if !sess.composer.attachments.is_empty() {
        let names: Vec<String> = sess
            .composer
            .attachments
            .iter()
            .map(|a| a.path.clone())
            .collect();
        title.push_str(&format!("✎ [{}] ", names.join(",")));
    }
    // slash 时给出更精确的标题提示：按回退链预告最终 cwd
    let val = sess.composer.value();
    if val.trim_start().starts_with('/') {
        title.push_str(" / 命令（Tab 补全 · ↑↓ 选择 · Enter 执行）· ");
        if val.trim() == "/new" || val.trim() == "/n" {
            let hint = app
                .effective_cwd(None)
                .map(|c| format!("[{}] ", truncate_cwd(&c)))
                .unwrap_or_default();
            title.push_str(&hint);
        }
    }
    title.push_str("Enter 发送 · Ctrl+P 模式 · Ctrl+A 附件 · Ctrl+Y 撤销 · Ctrl+E 压缩 · F1 帮助 ");

    let streaming = sess.streaming.is_some();
    // 边框呼吸：流式中 warn ↔ warn+DIM 交替（200ms 帧），静默时恒 dim。
    let frame = crate::app::anim::frame_now();
    let border_style = if streaming {
        if frame.is_multiple_of(2) {
            theme::warn()
        } else {
            theme::warn().add_modifier(Modifier::DIM)
        }
    } else {
        theme::dim()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    // 尾部窗口：光标行恒可见（输入超过 MAX_ROWS 时只显示最后 shown 行）。
    let shown = inner.height as usize;
    let (cursor_line, cursor_col) = sess.composer.line_col();
    let first = (cursor_line + 1).saturating_sub(shown);

    let prompt = "❯ ";
    let prompt_w = prompt.width();
    let avail_w = (inner.width as usize).saturating_sub(prompt_w);

    let mut rows: Vec<Line> = Vec::with_capacity(shown);
    let mut cursor_pos: Option<(u16, u16)> = None;
    for i in 0..shown {
        let line_no = first + i;
        let (ls, le) = sess.composer.line_bounds(line_no);
        let line_slice = &sess.composer.input[ls..le];
        let is_cursor_line = line_no == cursor_line;
        let col = if is_cursor_line {
            cursor_col.min(line_slice.len())
        } else {
            line_slice.len()
        };
        let (window, cursor_off) = edit_window(line_slice, col, avail_w.max(1));

        let wchars: Vec<char> = window.chars().collect();
        let before: String = wchars[..cursor_off.min(wchars.len())].iter().collect();
        let at: Option<char> = wchars.get(cursor_off).copied();
        let after: String = wchars[(cursor_off + usize::from(at.is_some())).min(wchars.len())..]
            .iter()
            .collect();

        let row_prompt = if line_no == 0 { prompt } else { "  " };
        let row_prompt_w = if line_no == 0 { prompt_w } else { 2 };
        let before_w = before.width();
        let at_w = at.map(|c| c.width().unwrap_or(0)).unwrap_or(1);
        let after_w = after.width();
        let mut spans = vec![Span::styled(row_prompt, theme::accent()), Span::raw(before)];
        spans.push(Span::styled(
            at.map(String::from).unwrap_or_else(|| " ".to_string()),
            Style::new().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::raw(after));

        // 流式状态标签：仅画在光标行的行尾。
        let mut used = row_prompt_w + before_w + at_w + after_w;
        if is_cursor_line && let Some(st) = &sess.streaming {
            let phase = match &st.tool_name {
                Some(t) => format!("{}({t})", st.phase.label()),
                None => st.phase.label().to_string(),
            };
            let label = format!("工作中 · {phase} · Esc 中止");
            // 空间充足时跑马灯滚动；局促时退化为静态截断标签。
            let animate = inner.width as usize > used + 16;
            let show = if animate {
                crate::app::anim::marquee(&label, (inner.width as usize - used - 1).min(40), frame)
            } else {
                label
            };
            let show_w = show.width();
            if used + show_w <= inner.width as usize {
                let pad = inner.width as usize - used - show_w;
                spans.push(Span::styled(" ".repeat(pad), Style::new()));
                spans.push(Span::styled(show, theme::warn()));
            }
            used = inner.width as usize;
        }

        if is_cursor_line {
            // edit_window 保证 off + 1 <= avail_w → 终端光标恒在界内。
            cursor_pos = Some((
                inner.x + row_prompt_w as u16 + cursor_off as u16,
                inner.y + i as u16,
            ));
        }
        let _ = used;
        rows.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(rows), inner);

    // 终端光标定位（IME/复制友好）。
    if let Some((x, y)) = cursor_pos
        && x < inner.x + inner.width
    {
        f.set_cursor_position((x, y));
    }
}

fn truncate_cwd(cwd: &str) -> String {
    let s = cwd.trim();
    if s.chars().count() <= 36 {
        return s.to_string();
    }
    // 保留尾段
    let tail: String = s
        .chars()
        .rev()
        .take(33)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}
