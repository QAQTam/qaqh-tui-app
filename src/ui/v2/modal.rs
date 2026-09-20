//! V2 blocking modal 渲染。
//!
//! permission / ask / plan 与确认、路径输入、思考回放统一走 alternate-screen
//! Modal。ask 采用 Grok 式单题分页：一页一个问题，左右切题，上下选选项，
//! Enter 选择并前进，Space 只选择不前进。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::session::{AskPanel, PermissionPanel, PlanPanel, option_shortcut_label};
use crate::app::{App, ConfirmAction, Overlay};
use crate::theme::Theme;
use crate::ui::v2::route::ModalRoute;

pub fn draw(f: &mut Frame, app: &App, area: Rect, theme: &Theme, route: ModalRoute) -> bool {
    match route {
        ModalRoute::Permission => {
            let Some(permission) = app
                .active_session()
                .and_then(|session| session.active_permission())
            else {
                return false;
            };
            draw_permission(f, permission, area, theme);
        }
        ModalRoute::Ask => {
            let Some(ask) = app
                .active_session()
                .and_then(|session| session.pending_ask.as_ref())
            else {
                return false;
            };
            draw_ask(f, ask, area, theme);
        }
        ModalRoute::Plan => {
            let Some(plan) = app
                .active_session()
                .and_then(|session| session.pending_plan.as_ref())
            else {
                return false;
            };
            draw_plan(f, plan, area, theme);
        }
        ModalRoute::Confirm => {
            let Some(Overlay::Confirm { action }) = app.overlays.last() else {
                return false;
            };
            draw_confirm(f, action, area, theme);
        }
        ModalRoute::AttachPath => {
            let Some(Overlay::AttachPath { input, cursor, .. }) = app.overlays.last() else {
                return false;
            };
            draw_input(
                f,
                input,
                *cursor,
                area,
                theme,
                InputSpec {
                    title: "附件路径",
                    prefix: "路径> ",
                    keys: &[("Enter", "上传"), ("Esc", "取消")],
                },
            );
        }
        ModalRoute::CwdInput => {
            let Some(Overlay::CwdInput { input, cursor }) = app.overlays.last() else {
                return false;
            };
            draw_input(
                f,
                input,
                *cursor,
                area,
                theme,
                InputSpec {
                    title: "新建会话目录",
                    prefix: "cwd> ",
                    keys: &[("Enter", "确认"), ("Esc", "取消")],
                },
            );
        }
        ModalRoute::Thinking => {
            let Some(Overlay::Thinking { scroll, body, .. }) = app.overlays.last() else {
                return false;
            };
            draw_thinking(f, *scroll, body, area, theme);
        }
    }
    true
}

fn draw_permission(f: &mut Frame, panel: &PermissionPanel, area: Rect, theme: &Theme) {
    let width = 72u16.min(area.width.saturating_sub(4));
    let height = 22u16.min(area.height.saturating_sub(4));
    let rect = centered_rect(width, height, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.warning))
            .title(format!(
                " ⚠ 工具权限 · {:?} · level {} ",
                panel.risk, panel.level
            )),
        rect,
    );
    let inner = inner_rect(rect);
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let mut lines = Vec::new();
    push_wrapped(
        &mut lines,
        "工具: ",
        &panel.tool_name,
        usize::from(inner.width),
        Style::new().fg(theme.accent.tool),
    );
    if let Some(action) = panel.action_summary.as_deref() {
        push_wrapped(
            &mut lines,
            "执行: ",
            action,
            usize::from(inner.width),
            Style::new().fg(theme.semantic.command),
        );
    }
    if !panel.reason.is_empty() {
        push_wrapped(
            &mut lines,
            "原因: ",
            &panel.reason,
            usize::from(inner.width),
            Style::new().fg(theme.text.secondary),
        );
    }
    push_wrapped(
        &mut lines,
        "类别: ",
        &format!("{:?}（影响等级 {}）", panel.category, panel.level),
        usize::from(inner.width),
        Style::new().fg(theme.text.secondary),
    );
    if !panel.consequence.is_empty() {
        push_wrapped(
            &mut lines,
            "后果: ",
            &panel.consequence,
            usize::from(inner.width),
            Style::new().fg(theme.text.secondary),
        );
    }
    for path in panel.paths.iter().take(6) {
        push_wrapped(
            &mut lines,
            "路径: ",
            path,
            usize::from(inner.width),
            Style::new().fg(theme.semantic.path),
        );
    }
    lines.push(Line::default());
    let trust = if panel.trust_folder { "[x]" } else { "[ ]" };
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {trust} 信任此目录"),
            Style::new().fg(if panel.trust_folder {
                theme.accent.success
            } else {
                theme.text.dim
            }),
        ),
        Span::styled(" · t 切换", Style::new().fg(theme.text.dim)),
    ]));
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );
    f.render_widget(
        Paragraph::new(footer(
            &[("a", "批准"), ("d/Esc", "拒绝"), ("t", "信任目录")],
            theme,
        )),
        footer_area,
    );
}

fn draw_plan(f: &mut Frame, panel: &PlanPanel, area: Rect, theme: &Theme) {
    let width = 88u16.min(area.width.saturating_sub(4));
    let height = area.height.saturating_sub(4);
    let rect = centered_rect(width, height, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    f.render_widget(Clear, rect);
    let review_type = if panel.review_type.is_empty() {
        "plan"
    } else {
        panel.review_type.as_str()
    };
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.plan))
            .title(format!(" 📋 计划评审 · {review_type} ")),
        rect,
    );
    let inner = inner_rect(rect);
    let footer_height = if panel.entering_message { 2 } else { 1 };
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(inner);
    let content_width = usize::from(inner.width);
    let wrapped = crate::app::render_line::wrap_text(&panel.plan_content, content_width);
    let start = panel.scroll.min(wrapped.len().saturating_sub(1));
    let available = usize::from(content_area.height);
    let mut lines: Vec<Line<'static>> = wrapped
        .into_iter()
        .skip(start)
        .take(available)
        .map(|line| Line::from(Span::styled(line, Style::new().fg(theme.text.primary))))
        .collect();
    if !panel.todo_items.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Todo",
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD),
        )));
        for item in &panel.todo_items {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  [{:?}] ", item.complexity),
                    Style::new().fg(theme.text.dim),
                ),
                Span::styled(item.title.clone(), Style::new().fg(theme.text.secondary)),
            ]));
        }
    }
    lines.truncate(available);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );

    let mut footer_lines = Vec::with_capacity(usize::from(footer_height));
    if panel.entering_message {
        footer_lines.push(Line::from(vec![
            Span::styled("  拒绝理由> ", Style::new().fg(theme.semantic.warning)),
            Span::styled(
                format!("{}_", panel.message),
                Style::new()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::REVERSED),
            ),
        ]));
        footer_lines.push(footer(&[("Enter", "提交拒绝"), ("Esc", "取消输入")], theme));
    } else {
        footer_lines.push(footer(
            &[
                ("a", "批准"),
                ("g", "批准+自主"),
                ("r", "拒绝并填写理由"),
                ("↑↓/PgUp/PgDn", "滚动"),
            ],
            theme,
        ));
    }
    f.render_widget(Paragraph::new(footer_lines), footer_area);
    if panel.entering_message {
        let y = footer_area.y;
        let x = inner
            .x
            .saturating_add("  拒绝理由> ".width() as u16)
            .saturating_add(panel.message.width() as u16);
        if x < inner.x.saturating_add(inner.width) {
            f.set_cursor_position((x, y));
        }
    }
}

fn draw_confirm(f: &mut Frame, action: &ConfirmAction, area: Rect, theme: &Theme) {
    let (title, body) = match action {
        ConfirmAction::DeleteSession(seed) => (
            "确认删除",
            format!("彻底删除会话 {seed}？磁盘数据不可恢复。"),
        ),
        ConfirmAction::ArchiveSession(seed) => ("确认归档", format!("归档会话 {seed}？")),
        ConfirmAction::CloseTab(seed) => {
            ("确认关闭", format!("关闭标签 {seed}？会话仍保留在列表中。"))
        }
    };
    let rect = centered_rect(64u16.min(area.width.saturating_sub(4)), 8, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.warning))
            .title(format!(" {title} ")),
        rect,
    );
    let inner = inner_rect(rect);
    let lines = vec![
        Line::from(Span::styled(body, Style::new().fg(theme.text.primary))),
        Line::default(),
        footer(&[("y", "确认"), ("n/Esc", "取消")], theme),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

struct InputSpec<'a> {
    title: &'a str,
    prefix: &'a str,
    keys: &'a [(&'a str, &'a str)],
}

fn draw_input(
    f: &mut Frame,
    input: &[char],
    cursor: usize,
    area: Rect,
    theme: &Theme,
    spec: InputSpec<'_>,
) {
    let rect = centered_rect(76u16.min(area.width.saturating_sub(4)), 7, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.chrome.border_active))
            .title(format!(" {} ", spec.title)),
        rect,
    );
    let inner = inner_rect(rect);
    let cursor = cursor.min(input.len());
    let before: String = input[..cursor].iter().collect();
    let before_width = before.width();
    let at = input
        .get(cursor)
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| " ".into());
    let line = Line::from(vec![
        Span::styled(spec.prefix, Style::new().fg(theme.accent.user)),
        Span::styled(before, Style::new().fg(theme.text.primary)),
        Span::styled(
            at,
            Style::new()
                .fg(theme.text.primary)
                .add_modifier(Modifier::REVERSED),
        ),
    ]);
    let footer_line = footer(spec.keys, theme);
    f.render_widget(
        Paragraph::new(vec![line, Line::default(), footer_line]),
        inner,
    );
    let x = inner
        .x
        .saturating_add(spec.prefix.width() as u16)
        .saturating_add(before_width as u16);
    if x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((x, inner.y));
    }
}

fn draw_thinking(f: &mut Frame, scroll: usize, body: &str, area: Rect, theme: &Theme) {
    let rect = centered_rect(
        88u16.min(area.width.saturating_sub(4)),
        area.height.saturating_sub(6),
        area,
    );
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.accent.thinking))
            .title(" 思考回放 · 当前回合 "),
        rect,
    );
    let inner = inner_rect(rect);
    let raw_lines: Vec<&str> = body.lines().collect();
    let start = scroll.min(raw_lines.len().saturating_sub(1));
    let mut lines = Vec::new();
    'outer: for raw in raw_lines.iter().skip(start) {
        for segment in crate::app::render_line::wrap_text(raw, usize::from(inner.width)) {
            if lines.len() >= usize::from(inner.height) {
                break 'outer;
            }
            lines.push(Line::from(Span::styled(
                segment,
                Style::new().fg(theme.text.secondary),
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "（无内容）",
            Style::new().fg(theme.text.dim),
        )));
    }
    lines.push(footer(
        &[("↑↓/PgUp/PgDn", "滚动"), ("e", "$PAGER"), ("Esc", "关闭")],
        theme,
    ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn inner_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x.saturating_add(1),
        y: rect.y.saturating_add(1),
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

fn draw_ask(f: &mut Frame, panel: &AskPanel, area: Rect, theme: &Theme) {
    let total = panel.questions.len().max(1);
    let focus = panel.focus.min(total.saturating_sub(1));
    let Some(question) = panel.questions.get(focus) else {
        return;
    };

    let width = 88u16.min(area.width.saturating_sub(4));
    let height = area.height.saturating_sub(4).min(30);
    let rect = centered_rect(width, height, area);
    if rect.width < 8 || rect.height < 4 {
        return;
    }

    f.render_widget(Clear, rect);
    let mode = match panel.mode {
        qaqh_client::AskMode::Single => "single",
        qaqh_client::AskMode::Batch => "batch",
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.chrome.border_active))
        .title(format!(" ❓ 问题 {}/{} · {mode} ", focus + 1, total));
    f.render_widget(block, rect);

    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    let inner_width = usize::from(inner.width);
    let mut lines = Vec::new();
    push_wrapped(
        &mut lines,
        "  ",
        &question.question,
        inner_width,
        Style::new()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    );
    lines.push(Line::default());

    let cursor = panel.option_cursor(focus);
    for (index, option) in question.options.iter().enumerate() {
        let selected = panel.selections[focus] == Some(index);
        let focused = cursor == index;
        let marker = if selected { "◉" } else { "○" };
        let shortcut = option_shortcut_label(index).unwrap_or('·');
        let option_style = if selected {
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text.primary)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if focused { " ▶ " } else { "   " },
                Style::new().fg(theme.accent.user),
            ),
            Span::styled(format!("{shortcut} "), Style::new().fg(theme.text.dim)),
            Span::styled(format!("{marker} "), option_style),
            Span::styled(option.clone(), option_style),
        ]));
    }

    if question.allow_custom {
        let custom_focused = cursor == question.options.len();
        let custom = panel.customs[focus].trim();
        let marker = if custom.is_empty() { "○" } else { "◉" };
        let text = if panel.editing_custom == Some(focus) {
            format!("自定义> {}_", panel.input)
        } else if custom.is_empty() {
            "自定义输入".to_string()
        } else {
            format!("自定义: {custom}")
        };
        lines.push(Line::from(vec![
            Span::styled(
                if custom_focused { " ▶ " } else { "   " },
                Style::new().fg(theme.accent.user),
            ),
            Span::styled("z ", Style::new().fg(theme.text.dim)),
            Span::styled(
                format!("{marker} "),
                Style::new().fg(theme.accent.assistant),
            ),
            Span::styled(text, Style::new().fg(theme.accent.assistant)),
        ]));
    }

    if let Some(error) = &panel.error {
        lines.push(Line::from(Span::styled(
            format!("  ✗ {error}"),
            Style::new().fg(theme.accent.error),
        )));
    }
    lines.push(Line::default());
    if panel.editing_custom.is_some() {
        lines.push(footer(
            &[("Enter", "提交并继续"), ("Esc", "取消输入")],
            theme,
        ));
    } else {
        lines.push(footer(
            &[
                ("←→", "切换问题"),
                ("↑↓", "选择选项"),
                ("Enter/Space", "选择"),
                ("1-9/a-f", "快捷键"),
                ("e/z", "自定义"),
                ("Esc", "跳过"),
            ],
            theme,
        ));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((panel.scroll, 0)),
        inner,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: usize,
    style: Style,
) {
    let wrapped = crate::app::render_line::wrap_text(text, width.saturating_sub(prefix.width()));
    for (index, segment) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix.width())
        };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::new()),
            Span::styled(segment, style),
        ]));
    }
}

fn footer(keys: &[(&str, &str)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, description)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(theme.text.dim)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::new()
                .fg(theme.accent.user)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {description}"),
            Style::new().fg(theme.text.dim),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::{PermissionPanel, PlanPanel};
    use crate::theme::{ColorSupport, ThemeKind};
    use qaqh_client::{
        AskMode, DomainAskQuestion as AskQuestion, PermissionCategory, PermissionRisk,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn panel() -> AskPanel {
        AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            AskMode::Batch,
            vec![
                AskQuestion {
                    id: "q1".into(),
                    question: "第一题".into(),
                    options: vec!["A".into(), "B".into()],
                    allow_custom: true,
                },
                AskQuestion {
                    id: "q2".into(),
                    question: "第二题：请选择完整方案".into(),
                    options: vec!["方案一".into(), "方案二".into(), "方案三".into()],
                    allow_custom: true,
                },
            ],
        )
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn ask_modal_renders_current_question_with_one_based_shortcuts() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let mut panel = panel();
        panel.focus = 1;
        panel.selections[1] = Some(1);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_ask(frame, &panel, frame.area(), &theme))
            .expect("draw ask");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("问题2/2"), "{text}");
        assert!(text.contains("第二题：请选择完整方案"), "{text}");
        assert!(text.contains("2◉方案二"), "{text}");
        assert!(!text.contains("[0]"), "{text}");
        assert!(!text.contains("第一题"), "{text}");
    }

    #[test]
    fn permission_modal_renders_action_and_risk() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let panel = PermissionPanel {
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            action_summary: Some("cargo test --all-targets".into()),
            reason: "运行测试".into(),
            paths: vec!["/tmp/project".into()],
            category: PermissionCategory::Exec,
            level: 2,
            risk: PermissionRisk::High,
            consequence: "会执行本地命令".into(),
            trust_folder: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_permission(frame, &panel, frame.area(), &theme))
            .expect("draw permission");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("工具权限"));
        assert!(text.contains("cargotest--all-targets"));
        assert!(text.contains("High"));
        assert!(text.contains("批准"));
    }

    #[test]
    fn plan_modal_renders_plan_and_review_actions() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let panel = PlanPanel {
            interaction_id: "plan-1".into(),
            turn_id: "turn-1".into(),
            plan_content: "第一阶段：完成视觉重构\n第二阶段：补齐测试".into(),
            review_type: "plan".into(),
            todo_items: Vec::new(),
            message: String::new(),
            entering_message: false,
            scroll: 0,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_plan(frame, &panel, frame.area(), &theme))
            .expect("draw plan");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("计划评审"));
        assert!(text.contains("第一阶段"));
        assert!(text.contains("批准+自主"));
        assert!(text.contains("拒绝并填写理由"));
    }
}
