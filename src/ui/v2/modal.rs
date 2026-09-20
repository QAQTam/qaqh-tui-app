//! V2 blocking modal 渲染。
//!
//! 当前只接管 ask_user；permission / plan review 仍由后续 M5 切片迁移。
//! ask 采用 Grok 式单题分页：一页一个问题，左右切题，上下选选项，Enter 选择并
//! 前进，Space 只选择不前进。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::app::session::{AskPanel, option_shortcut_label};
use crate::theme::Theme;

pub fn draw(f: &mut Frame, app: &App, area: Rect, theme: &Theme) -> bool {
    let Some(session) = app.active_session() else {
        return false;
    };
    let Some(ask) = session.pending_ask.as_ref() else {
        return false;
    };
    draw_ask(f, ask, area, theme);
    true
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
    use crate::theme::{ColorSupport, ThemeKind};
    use qaqh_client::{AskMode, DomainAskQuestion as AskQuestion};
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
}
