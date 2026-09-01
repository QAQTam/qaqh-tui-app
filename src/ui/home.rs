//! 首页：无 tab 时的居中会话列表 + Logo + 快捷提示。
//!
//! 视觉复刻 opencode Home 的“弹性留白 + 最大宽居中”手法，适配 ratatui 0.30。

use chrono::DateTime;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::ui::{modal, theme};

const LOGO: &[&str] = &[
    "  ██████╗  █████╗  ██████╗ ██░  ██╗",
    " ██╔═══██╗██╔══██╗██╔═══██╗██║  ██║",
    " ██║   ██║███████║██║   ██║███████║",
    " ██║▄▄ ██║██╔══██║██║   ██║██╔══██║",
    " ╚██████╔╝██║  ██║╚██████╔╝██║  ██║",
    "  ╚══▀▀═╝ ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝",
];

fn centered_area(area: Rect, width: u16, height: u16) -> Rect {
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
    // 背景清透
    f.render_widget(Clear, area);

    // 外层垂直居中：上下弹性空白 1: 中心卡 :1
    let outer = centered_area(area, 88, area.height.saturating_sub(2).min(36));
    f.render_widget(Clear, outer);

    // 卡片边框：三层背景感用 borderActive + panel
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Indexed(236)))
        .title(Line::from(vec![
            Span::styled(" qaqh-tui ", theme::active_tab()),
            Span::styled(format!(" Ringing v{} ", crate::protocol::RINGING_VERSION), theme::dim()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" Ctrl+T 新建 ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("·", theme::dim()),
            Span::styled(" Ctrl+L 列表 ", theme::dim()),
            Span::styled("·", theme::dim()),
            Span::styled(" F10 设置 ", theme::dim()),
            Span::styled("·", theme::dim()),
            Span::styled(" F1 帮助 ", theme::dim()),
        ]));
    f.render_widget(block, outer);

    let inner = Rect {
        x: outer.x + 2,
        y: outer.y + 1,
        width: outer.width.saturating_sub(4),
        height: outer.height.saturating_sub(2),
    };

    // 垂直布局：logo(6) + gap(1) + subtitle(1) + gap(1) + list + footer(2)
    let [logo_area, sub_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .areas(inner);

    // ── Logo：双色阴影（左半 textMuted，右半 accent bold）复刻 opencode logo.ts 淡色阴影
    let logo_fg = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let logo_dim = Style::new().fg(Color::DarkGray);
    let mut logo_lines: Vec<Line> = Vec::new();
    for (i, row) in LOGO.iter().enumerate() {
        let style = if i < 3 { logo_fg } else { logo_dim };
        // 居中：按显示宽度算 pad
        let pad = inner.width.saturating_sub(row.width() as u16) / 2;
        logo_lines.push(Line::from(vec![
            Span::styled(" ".repeat(pad as usize), Style::new()),
            Span::styled(*row, style),
        ]));
    }
    f.render_widget(Paragraph::new(logo_lines), logo_area);

    // 副标题
    let subtitle = Line::from(vec![
        Span::styled("  QAQ-Harness  ·  终端原生  ·  ", theme::dim()),
        Span::styled(
            match app.conn_phase {
                crate::app::ConnPhase::Ready => "● 已连接",
                crate::app::ConnPhase::Opening => "○ 连接中…",
                crate::app::ConnPhase::Lost => "◌ 已断开",
            },
            if matches!(app.conn_phase, crate::app::ConnPhase::Ready) {
                Style::new().fg(Color::Green)
            } else {
                theme::warn()
            },
        ),
        Span::styled(format!("  ·  {} ", app.epoch.chars().take(8).collect::<String>()), theme::dim()),
    ]);
    // 居中副标题
    let sub_w = subtitle.width() as u16;
    let sub_x = logo_area.x + inner.width.saturating_sub(sub_w) / 2;
    f.render_widget(Paragraph::new(subtitle), Rect { x: sub_x, y: sub_area.y, width: sub_w, height: 1 });

    // ── 会话列表（复用 session_list 渲染，但更紧凑好看）
    let mut items: Vec<&crate::protocol::methods::SessionMetaView> = app
        .session_list_cache
        .iter()
        .filter(|m| app.home_show_archived || !m.archived)
        .filter(|m| !m.ephemeral)
        .collect();
    // 按更新时间倒序（已有 list_cache 顺序即服务端返回顺序，通常已按 updated_at 倒序）
    // 截断到可见高度 -2
    let max_visible = list_area.height.saturating_sub(2) as usize;
    let total = items.len();

    let mut lines: Vec<Line> = Vec::new();

    // 列表标题栏
    let list_title = Line::from(vec![
        Span::styled(" 最近会话 ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(
            if app.home_show_archived { "（含归档）" } else { "" },
            theme::dim(),
        ),
        Span::styled(format!(" — {} 个 ", total), theme::dim()),
    ]);
    lines.push(list_title);
    lines.push(Line::from(Span::styled("─".repeat(list_area.width as usize), Style::new().fg(Color::Indexed(236)))));

    if app.session_list_at.is_none() {
        lines.push(Line::from(Span::styled("  加载中…", theme::dim())));
    } else if items.is_empty() {
        lines.push(Line::from(Span::styled("  暂无会话", theme::dim())));
        lines.push(Line::from(vec![
            Span::styled("  按 ", theme::dim()),
            Span::styled("n", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" 新建首个会话，或 ", theme::dim()),
            Span::styled("Ctrl+T", theme::accent()),
            Span::styled(" 亦可", theme::dim()),
        ]));
        lines.push(Line::from(Span::styled("  Tip: 归档会话按 a 显示", theme::dim())));
    } else {
        // 计算选中可见窗口（居中）
        let sel = app.home_selected.min(total.saturating_sub(1));
        let start = if total <= max_visible {
            0
        } else if sel < max_visible / 2 {
            0
        } else if sel >= total - max_visible / 2 {
            total - max_visible
        } else {
            sel - max_visible / 2
        };
        let end = (start + max_visible).min(total);
        items = items[start..end].to_vec();

        for (idx, m) in items.iter().enumerate() {
            let real_idx = start + idx;
            let is_sel = real_idx == sel;
            let open = app.tabs.contains(&m.seed);
            let flag = if m.archived { "▤" } else if open { "▣" } else { " " };
            let title = crate::app::truncate_str(&m.display_title(), 32);
            let activity = app.activity_cache.get(&m.seed).map(|a| format!("{a:?}")).unwrap_or_default();
            let updated = m
                .updated_at
                .and_then(|ms| DateTime::from_timestamp_millis(ms as i64))
                .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                .unwrap_or_default();

            let bg = if is_sel {
                Style::new().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else if open {
                Style::new().bg(Color::Indexed(236))
            } else {
                Style::new()
            };
            let dim = if is_sel { Style::new().bg(Color::Cyan).fg(Color::Black) } else { theme::dim() };

            let mut spans = vec![
                Span::styled(format!(" {flag} "), if is_sel { bg } else if open { theme::accent() } else { theme::dim() }),
                Span::styled(format!("{title:<32}"), if is_sel { bg } else { Style::new().add_modifier(Modifier::BOLD) }),
                Span::styled(format!("{activity:<10}"), if is_sel { bg } else { dim }),
                Span::styled(format!("{updated:<11}"), dim),
            ];
            if m.running {
                spans.push(Span::styled(" ●", if is_sel { bg } else { theme::ok() }));
            }
            // 选中行整行背景：通过 Line 背景需要每 span 同 bg，已在上面处理
            let mut line = Line::from(spans);
            if is_sel {
                line = line.style(bg);
            }
            lines.push(line);
        }
        if total > max_visible {
            lines.push(Line::from(Span::styled(
                format!("  … 还有 {} 个（↑↓ 翻页）", total - max_visible),
                theme::dim(),
            )));
        }
    }

    // 脚部连接信息
    lines.push(Line::from(""));
    lines.push(modal::footer_line(&[
        ("↑↓", "选择"),
        ("Enter", "打开/恢复"),
        ("n", "新建"),
        ("r", "刷新"),
        ("a", "归档显隐"),
        ("D", "删除"),
    ]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_area);

    // ── 底部提示： cwd / version
    let hint = Line::from(vec![
        Span::styled(
            app.sessions
                .values()
                .next()
                .and_then(|s| s.meta.as_ref().and_then(|m| m.cwd.clone()))
                .unwrap_or_else(|| " ".into()),
            theme::dim(),
        ),
        Span::styled("  ·  ", theme::dim()),
        Span::styled("qaqh-tui 0.1.0", theme::dim()),
    ]);
    f.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), hint_area);
}
