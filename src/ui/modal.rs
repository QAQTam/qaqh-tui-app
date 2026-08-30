//! 交互弹窗：工具权限（tool 频道）、ask_user / plan review（control 频道）。
//! 优先级 permission > ask > plan（与 winui 一致）。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::session::{AskPanel, PermissionPanel, PlanPanel};
use crate::app::App;
use crate::ui::theme;

pub fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(sess) = app.active_session() else { return };
    if let Some(perm) = sess.active_permission() {
        draw_permission(f, perm, area);
    } else if let Some(ask) = &sess.pending_ask {
        draw_ask(f, ask, area);
    } else if let Some(plan) = &sess.pending_plan {
        draw_plan(f, plan, area);
    }
}

pub fn footer_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, (key, desc)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled((*key).to_owned(), Style::new().fg(ratatui::style::Color::Cyan).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {desc}"), theme::dim()));
    }
    Line::from(spans)
}

fn push_wrapped(out: &mut Vec<Line<'static>>, prefix: &str, text: &str, width: usize, style: Style) {
    let wrapped = crate::app::render_line::wrap_text(text, width.saturating_sub(prefix.width()));
    for (i, seg) in wrapped.into_iter().enumerate() {
        let pfx = if i == 0 { prefix.to_owned() } else { " ".repeat(prefix.width()) };
        out.push(Line::from(vec![Span::styled(pfx, theme::dim()), Span::styled(seg, style)]));
    }
}

fn draw_permission(f: &mut Frame, perm: &PermissionPanel, area: Rect) {
    let w = 64u16.min(area.width.saturating_sub(4));
    let content_lines = 10usize + perm.paths.len().min(6);
    let h = (content_lines as u16 + 4).min(area.height.saturating_sub(2));
    let rect = centered_rect(w, h, area);
    f.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::warn())
        .title(format!(" ⚠ 工具权限请求 · risk {:?} · level {} ", perm.risk, perm.level));
    f.render_widget(block, rect);

    let inner_w = rect.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    push_wrapped(&mut lines, "工具: ", &perm.tool_name, inner_w, theme::accent());
    if !perm.reason.is_empty() {
        push_wrapped(&mut lines, "原因: ", &perm.reason, inner_w, Style::new());
    }
    push_wrapped(&mut lines, "类别: ", &format!("{:?} (影响等级 {})", perm.category, perm.level), inner_w, Style::new());
    if !perm.consequence.is_empty() {
        push_wrapped(&mut lines, "后果: ", &perm.consequence, inner_w, Style::new());
    }
    for p in perm.paths.iter().take(6) {
        push_wrapped(&mut lines, "路径: ", p, inner_w, Style::new());
    }
    lines.push(Line::from(""));
    let trust = if perm.trust_folder { "[x]" } else { "[ ]" };
    lines.push(Line::from(vec![
        Span::styled(format!("  {trust} 信任此目录"), Style::new().fg(ratatui::style::Color::Cyan)),
        Span::styled("  （高风险且涉及路径时可用）", theme::dim()),
    ]));
    lines.push(Line::from(""));
    lines.push(footer_line(&[("a", "批准执行"), ("d", "拒绝"), ("t", "切换信任目录")]));

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
    );
}

fn draw_ask(f: &mut Frame, panel: &AskPanel, area: Rect) {
    let w = 70u16.min(area.width.saturating_sub(4));
    // 高度估算：每题 2 行 + 选项 + 输入 + 页脚。
    let est: usize = panel
        .questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let opts = q.options.len().min(6);
            let custom = if panel.editing_custom == Some(i) { 2 } else { 0 };
            3 + opts + custom
        })
        .sum::<usize>()
        + 4;
    let h = (est as u16 + 3).min(area.height.saturating_sub(2));
    let rect = centered_rect(w, h, area);
    f.render_widget(Clear, rect);
    let mode_tag = match panel.mode {
        crate::protocol::event::AskMode::Single => "single",
        crate::protocol::event::AskMode::Batch => "batch",
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::modal_border())
        .title(format!(" ❓ 需要你的回答 ({mode_tag}) "));
    f.render_widget(block, rect);

    let inner_w = rect.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (qi, q) in panel.questions.iter().enumerate() {
        let focus_mark = if qi == panel.focus { "▶" } else { " " };
        push_wrapped(&mut lines, &format!("{focus_mark} Q{}: ", qi + 1), &q.question, inner_w, Style::new().add_modifier(Modifier::BOLD));
        if !q.options.is_empty() {
            for (oi, opt) in q.options.iter().enumerate() {
                let selected = panel.selections[qi] == Some(oi);
                let mark = if selected { "◉" } else { "○" };
                let style = if selected {
                    Style::new().fg(ratatui::style::Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::new()),
                    Span::styled(format!("{mark} "), style),
                    Span::styled(format!("[{oi}] "), theme::dim()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        let custom = panel.customs[qi].trim();
        if !custom.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("    ✎ ", theme::accent()),
                Span::styled(custom.to_owned(), Style::new().fg(ratatui::style::Color::Cyan)),
            ]));
        }
        if panel.editing_custom == Some(qi) {
            lines.push(Line::from(vec![
                Span::styled("    自定义> ", theme::accent()),
                Span::styled(
                    format!("{}_", panel.input),
                    Style::new().add_modifier(Modifier::REVERSED),
                ),
            ]));
        }
    }
    if let Some(err) = &panel.error {
        lines.push(Line::from(Span::styled(format!("  ✗ {err}"), theme::err())));
    }
    lines.push(Line::from(""));
    if panel.editing_custom.is_some() {
        lines.push(footer_line(&[("Enter", "结束本输入"), ("Esc", "取消输入")]));
    } else {
        lines.push(footer_line(&[
            ("↑↓", "切换问题"),
            ("1-9", "选择选项"),
            ("e", "自定义输入"),
            ("Enter", "提交"),
            ("Esc", "跳过(中止回合)"),
        ]));
    }

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
    );
}

fn draw_plan(f: &mut Frame, panel: &PlanPanel, area: Rect) {
    let w = 78u16.min(area.width.saturating_sub(4));
    let h = area.height.saturating_sub(4);
    let rect = centered_rect(w, h, area);
    f.render_widget(Clear, rect);
    let review_tag = if panel.review_type.is_empty() { "plan" } else { &panel.review_type };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::modal_border())
        .title(format!(" 📋 计划评审 ({review_tag}) "));
    f.render_widget(block, rect);

    // 左侧内容 + 右侧 todo 列表（若有）。
    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    let cols = Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(inner);

    let content_w = cols[0].width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let plan_lines = crate::app::render_line::wrap_text(&panel.plan_content, content_w);
    let scroll = panel.scroll.min(plan_lines.len().saturating_sub(1));
    for seg in plan_lines.iter().skip(scroll).take(inner.height.saturating_sub(4) as usize) {
        lines.push(Line::from(Span::raw(seg.clone())));
    }
    if !panel.todo_items.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Todo:", Style::new().add_modifier(Modifier::BOLD))));
        for item in &panel.todo_items {
            lines.push(Line::from(vec![
                Span::styled(format!("  [{:?}] ", item.complexity), theme::dim()),
                Span::raw(item.title.clone()),
            ]));
        }
    }
    lines.push(Line::from(""));
    if panel.entering_message {
        lines.push(Line::from(vec![
            Span::styled("  拒绝理由> ", theme::warn()),
            Span::styled(format!("{}_", panel.message), Style::new().add_modifier(Modifier::REVERSED)),
        ]));
        lines.push(footer_line(&[("Enter", "提交拒绝"), ("Esc", "取消")]));
    } else {
        lines.push(footer_line(&[
            ("a", "批准"),
            ("g", "批准+自主"),
            ("r", "拒绝(输入理由)"),
            ("↑↓/PgUp/PgDn", "滚动"),
        ]));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), cols[0]);
    // 自定义光标位置：拒绝输入时。
    if panel.entering_message {
        let y = rect.y + rect.height - 2;
        let x = rect.x + 1 + "  拒绝理由> ".width() as u16 + panel.message.chars().count() as u16;
        if x < rect.x + rect.width - 1 {
            f.set_cursor_position((x, y));
        }
    }
}
