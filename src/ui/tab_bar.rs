//! 标签栏（多会话 tab 条）。

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{App, truncate_str};
use crate::ui::theme;

pub fn draw(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut spans: Vec<Span> = vec![Span::styled(" qaqh-tui ", theme::active_tab())];

    let mut used: usize = 9;
    let mut overflow = false;
    for (idx, seed) in app.tabs.iter().enumerate() {
        let Some(sess) = app.sessions.get(seed) else {
            continue;
        };
        let is_active = idx == app.active;
        let title = truncate_str(&sess.title(), 18);
        // 子代理徽标：有拉起的子代理时追加 ↳N；任一运行中 → 高亮。
        let sub_count = sess.subagents.iter().filter(|e| e.seed.is_some()).count();
        let sub_running = sess
            .subagents
            .iter()
            .any(|e| e.state == crate::app::subagent::SubagentState::Running);
        let sub_badge = if sub_count > 0 {
            format!("↳{sub_count}")
        } else {
            String::new()
        };
        let mut label = format!(" {} {} ", idx + 1, title);
        if !is_active {
            // 挂起交互徽标。
            if !sess.pending_permissions.is_empty()
                || sess.pending_ask.is_some()
                || sess.pending_plan.is_some()
            {
                label = format!(" {} {} !", idx + 1, title);
            } else if sess.streaming.is_some() {
                label = format!(" {} {} …", idx + 1, title);
            }
        }
        if !sub_badge.is_empty() {
            // 追加到尾列（替换末尾空格）。
            if label.ends_with(' ') {
                label.pop();
            }
            label.push_str(&sub_badge);
            label.push(' ');
        }
        let w = label.chars().count() + 1;
        if used + w > area.width as usize {
            overflow = true;
            break;
        }
        let style: Style = if is_active {
            theme::active_tab()
        } else if sub_running || sess.is_waiting_user() {
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
