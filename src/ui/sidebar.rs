//! 右侧 workspace 面板：todo 列表 + 最近改动。
//!
//! 数据源（均为领域状态，非事件回放）：
//! - bootstrap control state 的 `dashboard_snapshot`（打开标签页即有初值）；
//! - agent 调 `todo` 工具时 daemon 即时推送的 `DashboardSnapshot`（replaceable）。
//! transcript 中的 todo 工具卡是历史轨迹；本面板是活状态视图，二者互补。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::render_line::wrap_text;
use crate::app::{truncate_str, App};
use crate::protocol::event::DashboardTask;
use crate::ui::theme;

/// 期望宽度；根布局在主区宽度不足 100 列时自动隐藏。
pub const PREFERRED_WIDTH: u16 = 31;

fn glyph_and_style(status: &str, current: bool) -> (&'static str, Style) {
    // status 来自后端 todo.rs status_name：idle | in_progress | completed | cancelled
    match status {
        "in_progress" => ("◐", theme::warn()),
        "completed" => ("●", theme::ok()),
        "cancelled" => ("⊘", theme::dim()),
        _ => ("○", if current { theme::accent() } else { theme::dim() }),
    }
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(sess) = app.active_session() else { return };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::dim())
        .title(" workspace · F4/F6 ");
    f.render_widget(block, area);
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let width = inner.width as usize;

    let Some(dash) = sess.dashboard.as_ref() else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(" 尚无 todo。", theme::dim())),
                Line::from(""),
                Line::from(Span::styled(" agent 用 todo 工具维护的", theme::dim())),
                Line::from(Span::styled(" 列表会在这里实时更新；", theme::dim())),
                Line::from(Span::styled(" transcript 中保留工具调用轨迹。", theme::dim())),
            ]),
            inner,
        );
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // 借鉴 opencode sidebar-todo：空列表或全部完成/取消时不展开列表，
    // 只显示一行状态（减噪声；agent 再次 in_progress 时列表自动回来）。
    let total_all = dash.tasks.len();
    let all_settled = !dash.tasks.is_empty()
        && dash.tasks.iter().all(|t| t.status == "completed" || t.status == "cancelled");
    if all_settled {
        let done = dash.tasks.iter().filter(|t| t.status == "completed").count();
        lines.push(Line::from(Span::styled(
            format!(" ● {done}/{total_all} 全部完成"),
            theme::ok(),
        )));
        lines.push(Line::from(""));
        for task in &dash.tasks {
            let (glyph, glyph_style) = glyph_and_style(&task.status, false);
            let subject = truncate_str(&task.subject, width.saturating_sub(7));
            lines.push(Line::from(vec![
                Span::styled(format!(" {glyph} "), glyph_style),
                Span::styled(subject, theme::dim()),
            ]));
        }
        if !dash.recent_edits.is_empty() {
            lines.push(Line::from(Span::styled(" ── 最近改动 ", theme::dim())));
            for edit in dash.recent_edits.iter().take(3) {
                let one = truncate_str(edit, width.saturating_sub(3));
                lines.push(Line::from(vec![
                    Span::styled(" · ", theme::dim()),
                    Span::styled(one, theme::dim()),
                ]));
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    // 计数行。
    let (mut idle, mut doing, mut done) = (0usize, 0usize, 0usize);
    for t in &dash.tasks {
        match t.status.as_str() {
            "in_progress" => doing += 1,
            "completed" => done += 1,
            "cancelled" => {}
            _ => idle += 1,
        }
    }
    lines.push(Line::from(Span::styled(
        format!(" ○{idle}  ◐{doing}  ●{done}  /{}", dash.tasks.len()),
        theme::dim(),
    )));
    lines.push(Line::from(""));

    for task in &dash.tasks {
        push_task(&mut lines, task, dash.current_todo_id.as_deref(), width, app.show_todo_detail);
    }
    if !app.show_todo_detail {
        lines.push(Line::from(Span::styled(" （F6 展开详情）", theme::dim())));
    }

    if !dash.recent_edits.is_empty() {
        lines.push(Line::from(Span::styled(" ── 最近改动 ", theme::dim())));
        for edit in dash.recent_edits.iter().take(4) {
            let one = truncate_str(edit, width.saturating_sub(3));
            lines.push(Line::from(vec![
                Span::styled(" · ", theme::dim()),
                Span::styled(one, theme::dim()),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn push_task(
    lines: &mut Vec<Line<'static>>,
    task: &DashboardTask,
    current_id: Option<&str>,
    width: usize,
    show_detail: bool,
) {
    let current = current_id == Some(task.id.as_str());
    let (glyph, glyph_style) = glyph_and_style(&task.status, current);
    let mark = if current { "▸" } else { " " };
    let prefix = format!(" {mark}{glyph} ");
    let prefix_w = prefix.width();

    let title_style = if current {
        Style::new().add_modifier(Modifier::BOLD)
    } else if task.status == "completed" {
        theme::dim()
    } else {
        Style::new()
    };
    for (i, seg) in wrap_text(&task.subject, width.saturating_sub(prefix_w)).into_iter().enumerate() {
        let pfx = if i == 0 { prefix.clone() } else { " ".repeat(prefix_w) };
        lines.push(Line::from(vec![
            Span::styled(pfx, glyph_style),
            Span::styled(seg, title_style),
        ]));
    }

    if !task.description.is_empty() && show_detail {
        let one = task.description.replace('\n', " ");
        let desc = truncate_str(&one, width.saturating_sub(prefix_w + 1));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(prefix_w), theme::dim()),
            Span::styled(desc, theme::dim()),
        ]));
    }
    if let Some(evidence) = task.evidence.as_deref().filter(|s| !s.is_empty()).filter(|_| show_detail) {
        let one = evidence.replace('\n', " ");
        let ev = truncate_str(&one, width.saturating_sub(prefix_w + 3));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(prefix_w), theme::dim()),
            Span::styled(format!("✓ {ev}"), theme::dim()),
        ]));
    }
    lines.push(Line::from(""));
}
