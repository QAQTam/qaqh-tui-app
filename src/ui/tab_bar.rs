//! 标签栏（多会话 tab 条）。

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{truncate_str, App};
use crate::ui::theme;

pub fn draw(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut spans: Vec<Span> = vec![Span::styled(" qaqh-tui ", theme::active_tab())];

    let mut used: usize = 9;
    let mut overflow = false;
    for (idx, seed) in app.tabs.iter().enumerate() {
        let Some(sess) = app.sessions.get(seed) else { continue };
        let is_active = idx == app.active;
        let title = truncate_str(&sess.title(), 18);
        let mut label = format!(" {} {} ", idx + 1, title);
        if !is_active {
            // 挂起交互徽标。
            if sess.pending_permissions.len() > 0 || sess.pending_ask.is_some() || sess.pending_plan.is_some() {
                label = format!(" {} {} !", idx + 1, title);
            } else if sess.streaming.is_some() {
                label = format!(" {} {} …", idx + 1, title);
            }
        }
        let w = label.chars().count() + 1;
        if used + w > area.width as usize {
            overflow = true;
            break;
        }
        let style: Style = if is_active {
            theme::active_tab()
        } else if sess.is_waiting_user() {
            theme::warn()
        } else if sess.streaming.is_some() {
            theme::dim()
        } else {
            Style::new()
        };
        spans.push(Span::styled(label, style));
        used += w;
    }
    if overflow {
        spans.push(Span::styled(" …", theme::dim()));
    }

    // 右侧提示。
    let hint = " Alt+1..9 切换 · Ctrl+T 新建 · Ctrl+L 列表 ";
    let hint_w = hint.chars().count();
    if area.width as usize > used + hint_w {
        let pad = area.width as usize - used - hint_w;
        spans.push(Span::styled(" ".repeat(pad), Style::new()));
        spans.push(Span::styled(hint, theme::dim()));
    }

    f.render_widget(Line::from(spans), area);
}
