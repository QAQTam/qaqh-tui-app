//! 底部状态栏：连接相位 / epoch / toast / 用量 / 时钟。

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::{App, ConnPhase};
use crate::protocol::event::NoticeLevel;
use crate::ui::theme;

pub fn draw(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let width = area.width as usize;
    let mut left: Vec<Span> = Vec::new();

    match app.conn_phase {
        ConnPhase::Ready => {
            left.push(Span::styled(" ● ready", theme::ok()));
            let ep = if app.epoch.len() > 8 { &app.epoch[..8] } else { &app.epoch };
            left.push(Span::styled(format!(" {ep}"), theme::dim()));
        }
        ConnPhase::Opening => {
            left.push(Span::styled(" ◌ connecting", theme::warn()));
        }
        ConnPhase::Lost => {
            left.push(Span::styled(" ✗ lost", theme::err()));
            if let Some(err) = &app.conn_error {
                left.push(Span::styled(format!(" {}", crate::app::truncate_str(err, 30)), theme::err()));
            }
        }
    }
    if app.pending_creates.len() > 0 {
        left.push(Span::styled(" · creating…", theme::dim()));
    }

    // 中间：最新 toast。
    let mut middle: Vec<Span> = Vec::new();
    if let Some(toast) = app.toasts.back() {
        let style = match toast.level {
            NoticeLevel::Info => theme::dim(),
            NoticeLevel::Warn => theme::warn(),
            NoticeLevel::Error => theme::err(),
        };
        let text = crate::app::truncate_str(&toast.text, width.saturating_sub(60).max(20));
        middle.push(Span::styled(format!(" {text}"), style));
    }

    // 右侧：用量 / 活动 / 时钟。
    let mut right: Vec<Span> = Vec::new();
    if let Some(sess) = app.active_session() {
        if let Some(usage) = sess.usage.as_ref() {
            right.push(Span::styled(
                format!(" ↑{}k ↓{}k", usage.prompt_tokens / 1000, usage.completion_tokens / 1000),
                theme::dim(),
            ));
            if let Some(limit) = sess.context_limit {
                let pct = if limit > 0 {
                    (usage.prompt_tokens as u64 * 100 / limit as u64).min(999)
                } else {
                    0
                };
                right.push(Span::styled(format!(" ({pct}%)"), theme::dim()));
            }
        }
        right.push(Span::styled(format!(" · {}", sess.activity_label()), theme::accent()));
    }
    let now = chrono::Local::now().format("%H:%M");
    right.push(Span::styled(format!(" · {now} "), theme::dim()));

    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let mid_budget = width.saturating_sub(left_w + right_w).max(0);
    let mid_w: usize = middle.iter().map(|s| s.content.chars().count()).sum();
    if mid_w > mid_budget {
        middle.clear();
    }

    let mut spans = left;
    let used = spans.iter().map(|s| s.content.chars().count()).sum::<usize>()
        + right_w
        + middle.iter().map(|s| s.content.chars().count()).sum::<usize>();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), Style::new()));
    }
    spans.extend(middle);
    spans.extend(right);

    f.render_widget(Line::from(spans), area);
}
