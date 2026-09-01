//! Composer：输入行 + 附件标记 + 流式相位。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::ui::theme;

pub fn height() -> u16 {
    3 // 上下边框 + 1 行输入
}

pub fn draw_slash_menu(f: &mut Frame, app: &App, composer_area: Rect) {
    if app.overlays.len() > 0 {
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
    let block = Block::new().borders(Borders::ALL).border_style(theme::accent()).title(" / 命令 · Tab 补全 · ↑↓ 选择 · Enter 执行 · Esc 关闭 ");
    let inner = Rect { x: menu_area.x+1, y: menu_area.y+1, width: menu_area.width.saturating_sub(2), height: menu_area.height.saturating_sub(2) };
    f.render_widget(block, menu_area);
    let mut lines: Vec<Line> = Vec::new();
    for (idx, def) in visible.iter().enumerate() {
        let is_sel = idx == selected;
        let marker = if is_sel { "▸" } else { " " };
        let style = if is_sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), if is_sel { theme::accent() } else { theme::dim() }),
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
        let names: Vec<String> =
            sess.composer.attachments.iter().map(|a| a.path.clone()).collect();
        title.push_str(&format!("✎ [{}] ", names.join(",")));
    }
    // slash 时给出更精确的标题提示：按回退链预告最终 cwd
    let val = sess.composer.value();
    if val.trim_start().starts_with('/') {
        title.push_str(" / 命令（Tab 补全 · ↑↓ 选择 · Enter 执行）· ");
        if val.trim() == "/new" || val.trim() == "/n" {
            let hint = app.effective_cwd(None).map(|c| format!("[{}] ", truncate_cwd(&c))).unwrap_or_default();
            title.push_str(&hint);
        }
    }
    title.push_str("Enter 发送 · Ctrl+P 模式 · Ctrl+A 附件 · Ctrl+Y 撤销 · Ctrl+E 压缩 · F1 帮助 ");

    let streaming = sess.streaming.is_some();
    let border_style = if streaming { theme::warn() } else { theme::dim() };
    let block = Block::new().borders(Borders::ALL).border_style(border_style).title(title);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let prompt = "❯ ";
    let prompt_w = prompt.width();

    let cursor_char = sess.composer.cursor.min(sess.composer.input.len());
    let before: String = sess.composer.input[..cursor_char].iter().collect();
    let at: String = sess.composer.input.get(cursor_char).map(|c| c.to_string()).unwrap_or_default();
    let after: String = sess.composer.input[(cursor_char + if at.is_empty() { 0 } else { 1 }).min(sess.composer.input.len())..]
        .iter()
        .collect();

    let mut spans = vec![
        Span::styled(prompt, theme::accent()),
        Span::raw(before.clone()),
    ];
    if at.is_empty() {
        spans.push(Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)));
    } else {
        spans.push(Span::styled(at.clone(), Style::new().add_modifier(Modifier::REVERSED)));
        spans.push(Span::raw(after.clone()));
    }

    let used = prompt_w + before.width() + at.width() + after.width();
    if let Some(s) = &sess.streaming {
        let phase = match &s.tool_name {
            Some(t) => format!("{}({t})", s.phase.label()),
            None => s.phase.label().to_string(),
        };
        let label = format!(" Esc 中止 · {phase} ");
        let label_w = label.width();
        if used + label_w <= inner.width as usize {
            let pad = inner.width as usize - used - label_w;
            spans.push(Span::styled(" ".repeat(pad), Style::new()));
            spans.push(Span::styled(label, theme::warn()));
        }
    }

    f.render_widget(Line::from(spans), inner);

    // 终端光标定位（IME/复制友好）。
    let cursor_x = inner.x + (prompt_w + before.width()) as u16;
    if cursor_x < inner.x + inner.width {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

fn truncate_cwd(cwd: &str) -> String {
    let s = cwd.trim();
    if s.chars().count() <= 36 { return s.to_string(); }
    // 保留尾段
    let tail: String = s.chars().rev().take(33).collect::<String>().chars().rev().collect();
    format!("…{tail}")
}
