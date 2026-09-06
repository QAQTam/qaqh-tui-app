//! 右侧 workspace 面板：todo 列表 + 最近改动 + 文档。
//!
//! 数据源（均为领域状态，非事件回放）：
//! - bootstrap control state 的 `dashboard_snapshot`（打开标签页即有初值）；
//! - agent 调 `todo` 工具时 daemon 即时推送的 `DashboardSnapshot`（replaceable）。
//!
//! 失败路径：若 SSE 丢帧，`DashboardUpdated` 为空实现，未来可走 `session.dashboard` 拉取兜底。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::render_line::wrap_text;
use crate::app::{App, truncate_str};
use crate::protocol::event::DashboardTask;
use crate::ui::theme;

pub const PREFERRED_WIDTH: u16 = 34;

/// 最低展示宽度：<80 列时根布局自动隐藏以保 transcript 可读性（见 `ui::mod`）。
pub const MIN_MAIN_WIDTH: u16 = 85;

fn glyph_and_style(status: &str, current: bool, frame: u64, phase: u64) -> (&'static str, Style) {
    match status {
        // in_progress：braille 八帧转轮；task_id 相位偏移避免所有行同步闪动。
        "in_progress" => (
            crate::app::anim::spinner_glyph(frame + phase),
            theme::warn(),
        ),
        "completed" => ("●", theme::ok()),
        "cancelled" => ("✕", theme::dim()),
        _ => (
            "○",
            if current {
                theme::accent()
            } else {
                theme::dim()
            },
        ),
    }
}

fn progress_bar(done: usize, total: usize, _width: usize) -> (String, Style) {
    if total == 0 {
        return (String::new(), theme::dim());
    }
    let ratio = done as f32 / total as f32;
    // 10 格进度
    let filled = (ratio * 10.0).round() as usize;
    let empty = 10usize.saturating_sub(filled);
    let bar = format!("{}{}", "▰".repeat(filled), "▱".repeat(empty));
    let pct = (ratio * 100.0) as usize;
    (
        format!("{bar} {pct}%"),
        if done == total {
            theme::ok()
        } else {
            theme::accent()
        },
    )
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(sess) = app.active_session() else {
        return;
    };
    // 标题计数：done/total
    let title_extra = if let Some(dash) = sess.dashboard.as_ref() {
        let total = dash.tasks.len();
        let done = dash
            .tasks
            .iter()
            .filter(|t| t.status == "completed")
            .count();
        if total > 0 {
            format!(" {done}/{total} ")
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::dim())
        .title(Line::from(vec![
            Span::styled(" workspace ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(
                title_extra.clone(),
                if sess.dashboard.is_some() {
                    theme::accent()
                } else {
                    theme::dim()
                },
            ),
            Span::styled("· F4 隐藏 ", theme::dim()),
            Span::styled("F6 详情", theme::dim()),
        ]))
        .title_bottom(Line::from(vec![Span::styled(
            sess.display_model().unwrap_or_else(|| "no model".into()),
            theme::dim(),
        )]));
    f.render_widget(block, area);
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let width = inner.width as usize;
    if width == 0 || inner.height == 0 {
        return;
    }

    let Some(dash) = sess.dashboard.as_ref() else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("  ◯  尚无 todo", theme::dim())),
                Line::from(""),
                Line::from(Span::styled("  agent 用", theme::dim())),
                Line::from(Span::styled(
                    "  todo 工具维护的",
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled("  任务会在这里", theme::dim())),
                Line::from(Span::styled("  实时更新。", theme::dim())),
                Line::from(""),
                Line::from(Span::styled("  ──────────────", theme::dim())),
                Line::from(Span::styled("  transcript 中", theme::dim())),
                Line::from(Span::styled("  仍保留调用轨迹", theme::dim())),
                Line::from(""),
                Line::from(Span::styled("  Tip: 让 agent", theme::dim())),
                Line::from(Span::styled("  `todo:create` 建计划", theme::dim())),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    let total = dash.tasks.len();
    let mut idle = 0usize;
    let mut doing = 0usize;
    let mut done = 0usize;
    let mut cancelled = 0usize;
    for t in &dash.tasks {
        match t.status.as_str() {
            "in_progress" => doing += 1,
            "completed" => done += 1,
            "cancelled" => cancelled += 1,
            _ => idle += 1,
        }
    }
    let all_settled = !dash.tasks.is_empty()
        && dash
            .tasks
            .iter()
            .all(|t| t.status == "completed" || t.status == "cancelled");

    // ── 顶部摘要 + 进度条 ──
    if total > 0 {
        // 计数行：更紧凑且带色点
        lines.push(Line::from(vec![
            Span::styled(
                format!(" ○{idle} "),
                if idle > 0 { Style::new() } else { theme::dim() },
            ),
            Span::styled(
                format!("◐{doing} "),
                if doing > 0 {
                    theme::warn()
                } else {
                    theme::dim()
                },
            ),
            Span::styled(
                format!("●{done} "),
                if done > 0 { theme::ok() } else { theme::dim() },
            ),
            Span::styled(
                if cancelled > 0 {
                    format!("✕{cancelled} ")
                } else {
                    String::new()
                },
                theme::dim(),
            ),
            Span::styled(format!("/{total}"), theme::dim()),
        ]));
        if !all_settled {
            let (bar, bar_style) = progress_bar(done, total, width);
            lines.push(Line::from(vec![Span::styled(bar, bar_style)]));
        } else {
            lines.push(Line::from(Span::styled(
                format!(" ✓ {done}/{total} 全部完成"),
                theme::ok(),
            )));
        }
        // 当前聚焦
        // 聚焦行：id 保留在此（工作视图唯一的 id 锚点）
        if let Some(cur) = dash.current_todo_id.as_deref()
            && let Some(task) = dash.tasks.iter().find(|t| t.id == cur)
        {
            let tag = format!(
                " ▶ [{}] {} ",
                task.id,
                truncate_str(&task.subject, width.saturating_sub(10))
            );
            lines.push(Line::from(vec![Span::styled(
                tag,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]));
        }
        lines.push(Line::from(Span::styled(
            "─".repeat(width.min(28)),
            Style::new().fg(Color::Indexed(236)),
        )));
    } else {
        lines.push(Line::from(Span::styled(" ○  空任务列表", theme::dim())));
        lines.push(Line::from(Span::styled(
            "─".repeat(width.min(28)),
            Style::new().fg(Color::Indexed(236)),
        )));
        lines.push(Line::from(Span::styled(
            " 让 agent 创建首个 todo",
            theme::dim(),
        )));
        lines.push(Line::from(vec![
            Span::styled("  ", theme::dim()),
            Span::styled("todo:create", Style::new().fg(Color::Cyan)),
        ]));
    }

    if all_settled {
        for task in &dash.tasks {
            let phase = task.id.bytes().map(|b| b as u64).sum::<u64>();
            let (glyph, glyph_style) =
                glyph_and_style(&task.status, false, crate::app::anim::frame_now(), phase);
            let subject = truncate_str(&task.subject, width.saturating_sub(7));
            lines.push(Line::from(vec![
                Span::styled(format!(" {glyph} "), glyph_style),
                Span::styled(subject, theme::dim()),
            ]));
        }
    } else {
        // 保持服务端原始顺序（计划顺序），突出 in_progress；
        // 非详情模式下已落定项折叠为摘要行，控制列表长度。
        let mut settled_hidden = 0usize;
        for task in &dash.tasks {
            let settled = task.status == "completed" || task.status == "cancelled";
            if !app.show_todo_detail && settled {
                settled_hidden += 1;
                continue;
            }
            push_task(
                &mut lines,
                task,
                dash.current_todo_id.as_deref(),
                width,
                app.show_todo_detail,
            );
        }
        if settled_hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("  · 已落定 {} 条（F6 详情/展开）", settled_hidden),
                theme::dim(),
            )));
        }
        if !app.show_todo_detail && !dash.tasks.is_empty() && settled_hidden == 0 {
            lines.push(Line::from(Span::styled(
                "  … F6 展开描述/证据",
                theme::dim(),
            )));
        }
    }

    // ── 文档区（若有） ──
    if !dash.documents.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" ── ", theme::dim()),
            Span::styled("文档", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ·{} ", dash.documents.len()), theme::dim()),
        ]));
        for doc in dash.documents.iter().take(4) {
            let stale = if doc.is_stale { " ⟡" } else { "" };
            let path = truncate_str(&doc.path, width.saturating_sub(5 + stale.width()));
            lines.push(Line::from(vec![
                Span::styled(" · ", theme::dim()),
                Span::styled(path, Style::new()),
                Span::styled(stale, theme::warn()),
            ]));
        }
        if dash.documents.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("  … +{} 更多", dash.documents.len() - 4),
                theme::dim(),
            )));
        }
    }

    // ── 最近改动 ──
    if !dash.recent_edits.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" ── ", theme::dim()),
            Span::styled("最近改动", Style::new().add_modifier(Modifier::BOLD)),
        ]));
        for edit in dash.recent_edits.iter().take(4) {
            let one = truncate_str(edit, width.saturating_sub(3));
            lines.push(Line::from(vec![
                Span::styled(" · ", theme::dim()),
                Span::styled(one, theme::dim()),
            ]));
        }
        if dash.recent_edits.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("  … +{} 更多", dash.recent_edits.len() - 4),
                theme::dim(),
            )));
        }
    } else if dash.tasks.is_empty() && dash.documents.is_empty() {
        // 空态补充提示
    }

    // 底部留白避免贴边
    lines.push(Line::from(""));

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
    let phase = task.id.bytes().map(|b| b as u64).sum::<u64>();
    let (glyph, glyph_style) =
        glyph_and_style(&task.status, current, crate::app::anim::frame_now(), phase);
    let mark = if current { "▸" } else { " " };
    // id 不进标题行（与落定视图一致，标题行只留内容）；id 挂在详情行
    // （↳ [T3] …）与聚焦行（▶ [T3] …）作为对照 transcript 的锚点。
    let prefix = format!(" {mark}{glyph} ");

    let title_style = if current {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if task.status == "completed" {
        theme::dim()
    } else if task.status == "cancelled" {
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    // 标题可能需换行：只折 subject，续行对齐在标题起点。
    let segs = wrap_text(&task.subject, width.saturating_sub(prefix.width()));
    for (i, seg) in segs.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), glyph_style),
                Span::styled(seg, title_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(prefix.width()), theme::dim()),
                Span::styled(seg, title_style),
            ]));
        }
    }

    if !task.description.is_empty() && show_detail {
        let one = task.description.replace('\n', " ");
        let head = format!("↳ [{}] ", task.id);
        let desc = truncate_str(
            &one,
            width.saturating_sub(prefix.width() + head.width() + 1),
        );
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(prefix.width()), theme::dim()),
            Span::styled(head, theme::dim()),
            Span::styled(desc, theme::dim()),
        ]));
    }
    if let Some(evidence) = task
        .evidence
        .as_deref()
        .filter(|s| !s.is_empty())
        .filter(|_| show_detail)
    {
        let one = evidence.replace('\n', " ");
        let ev = truncate_str(&one, width.saturating_sub(prefix.width() + 3));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(prefix.width()), theme::dim()),
            Span::styled(format!("✓ {ev}"), Style::new().fg(Color::Green)),
        ]));
    }
    // 任务间微空隙（未完成任务更疏）
    if task.status != "completed" && task.status != "cancelled" {
        lines.push(Line::from(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, subject: &str, status: &str) -> DashboardTask {
        DashboardTask {
            id: id.into(),
            subject: subject.into(),
            description: "任务描述".into(),
            status: status.into(),
            evidence: None,
        }
    }

    fn joined(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn title_line_has_no_id_and_detail_line_carries_it() {
        // 标题行只留内容（与落定视图一致）；id 锚在详情行。
        let mut lines = Vec::new();
        push_task(&mut lines, &task("T3", "实现解码", "idle"), None, 40, true);
        let title = joined(&lines[0]);
        assert!(title.contains("实现解码"));
        assert!(!title.contains("T3"), "标题行不得携带 id：{title}");
        let desc = lines.iter().map(joined).find(|l| l.contains("↳")).unwrap();
        assert!(desc.contains("[T3]"), "id 应锚在详情行：{desc}");
    }

    #[test]
    fn wrap_continuation_aligns_without_id() {
        // 长标题折行：续行对齐在标题起点，且不混入 id。
        let mut lines = Vec::new();
        push_task(
            &mut lines,
            &task("T7", "第一段很长很长的任务标题第二段也很长", "idle"),
            None,
            20,
            false,
        );
        assert!(lines.len() >= 2, "应发生折行");
        let cont = joined(&lines[1]);
        assert!(!cont.contains("T7"));
        assert!(cont.starts_with("   "), "续行以 prefix 宽度对齐：{cont:?}");
    }

    #[test]
    fn detail_false_hides_description_line() {
        let mut lines = Vec::new();
        push_task(&mut lines, &task("T1", "标题", "idle"), None, 40, false);
        assert!(lines.iter().all(|l| !joined(l).contains("↳")));
    }
}
