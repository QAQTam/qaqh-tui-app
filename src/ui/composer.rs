//! Composer：输入行 + 附件标记 + 流式相位。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::ui::theme;

pub fn height() -> u16 {
    3 // 上下边框 + 1 行输入
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
