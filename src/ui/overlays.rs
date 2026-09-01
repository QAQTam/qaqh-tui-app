//! 覆盖层：会话列表 / 配置 / 帮助 / 附件路径 / 确认框。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ConfirmAction, Overlay};
use crate::ui::{modal, theme};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(top) = app.overlays.last() else { return };
    match top {
        Overlay::SessionList { selected, show_archived } => {
            draw_session_list(f, app, area, *selected, *show_archived)
        }
        Overlay::Settings(_) => crate::ui::settings::draw(f, app, area),
        Overlay::Help => draw_help(f, area),
        Overlay::AttachPath { input, cursor, .. } => draw_attach(f, input, *cursor, area),
        Overlay::CwdInput { input, cursor } => draw_cwd(f, input, *cursor, area),
        Overlay::Confirm { action } => draw_confirm(f, action, area),
    }
}

fn box_frame(f: &mut Frame, area: Rect, width: u16, height: u16, title: &str) -> Rect {
    let rect = modal::centered_rect(width, height, area);
    f.render_widget(Clear, rect);
    let block = Block::new().borders(Borders::ALL).border_style(theme::accent()).title(format!(" {title} "));
    f.render_widget(block, rect);
    Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

fn draw_session_list(f: &mut Frame, app: &App, area: Rect, selected: usize, show_archived: bool) {
    let inner = box_frame(f, area, 86, area.height.saturating_sub(4), "会话列表");
    let mut lines: Vec<Line> = Vec::new();

    // 过滤 + 行构造（与 app 的选中逻辑共享同一过滤谓词）。
    let items: Vec<&crate::protocol::methods::SessionMetaView> = app
        .session_list_cache
        .iter()
        .filter(|m| show_archived || !m.archived)
        .filter(|m| !m.ephemeral)
        .collect();

    if app.session_list_at.is_none() {
        lines.push(Line::from(Span::styled(" 加载中…", theme::dim())));
    } else if items.is_empty() {
        lines.push(Line::from(Span::styled(" （无会话——按 n 新建）", theme::dim())));
    }
    for (idx, m) in items.iter().enumerate() {
        let selected_now = idx == selected;
        let open = app.tabs.contains(&m.seed);
        let activity = app.activity_cache.get(&m.seed).map(|a| format!("{a:?}")).unwrap_or_default();
        let updated = m
            .updated_at
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
            .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let flag = if m.archived { "▤" } else if open { "▣" } else { " " };
        let title = crate::app::truncate_str(&m.display_title(), 36);
        let mut spans = vec![
            Span::styled(format!(" {flag} "), if open { theme::accent() } else { theme::dim() }),
            Span::styled(format!("{title:<36}"), if selected_now {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            }),
            Span::styled(format!("{activity:<14}"), theme::dim()),
            Span::styled(format!("{updated:<12}"), theme::dim()),
            Span::styled(format!("#{}", m.seed), theme::dim()),
        ];
        if m.running {
            spans.push(Span::styled(" ●", theme::ok()));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(modal::footer_line(&[
        ("↑↓", "选择"),
        ("Enter", "打开/恢复"),
        ("n", "新建"),
        ("x", "归档"),
        ("u", "取消归档"),
        ("D", "删除"),
        ("a", "显示归档"),
        ("r", "刷新"),
        ("Esc", "关闭"),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let inner = box_frame(f, area, 78, 30, "按键帮助");
    let keys: Vec<(&str, &str)> = vec![
        ("Alt+1..9", "切换会话标签"),
        ("Alt+←/→", "前后切换标签"),
        ("Ctrl+T", "新建会话（回退链：QAQH_DEFAULT_CWD>启动目录>当前会话）"),
        ("/ + Tab/↑↓/Enter", "斜杠命令（/new [cwd] · /help · /clear）"),
        ("/new [cwd]", "留空静默用回退链，/new ? 或 /new Tab 打开二级 cwd 编辑（支持 ~/）"),
        ("Ctrl+W", "关闭当前标签（不影响会话本身）"),
        ("Ctrl+L", "会话列表（恢复/归档/删除）"),
        ("Ctrl+, / F10", "设置页（读写 daemon 配置）"),
        ("Enter", "发送消息"),
        ("Esc", "中止当前回合 / 关闭弹窗（/ 菜单下仅关闭菜单）"),
        ("Ctrl+P", "切换 plan/code 模式"),
        ("Ctrl+Y", "撤销最后一个回合"),
        ("Ctrl+E", "上下文压缩（compact）"),
        ("Ctrl+A", "添加附件（上传 → ContentRef）"),
        ("PgUp/PgDn", "滚动 transcript（PgUp 加载更早回合）"),
        ("Ctrl+Home/End", "顶部 / 底部"),
        ("F3", "展开/折叠思考块"),
        ("F4", "workspace 侧栏（todo 列表）开/关"),
        ("F6", "todo 详情展开/折叠"),
        ("F7", "工具输出展开/折叠（长输出/diff）"),
        ("F1", "本帮助"),
        ("Ctrl+C ×2 / Ctrl+Q", "退出"),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!(" {k:<20}"), Style::new().fg(ratatui::style::Color::Cyan)),
            Span::raw(d.to_owned()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " 协议：qaqh.Ringing v1 — open/lease/3×SSE/timeline 严格 +1 光标",
        theme::dim(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_attach(f: &mut Frame, input: &[char], cursor: usize, area: Rect) {
    let inner = box_frame(f, area, 70, 5, "附件路径（Enter 上传 · Esc 取消）");
    let cursor = cursor.min(input.len());
    let before: String = input[..cursor].iter().collect();
    let before_w = before.width() as u16;
    let at: String = input.get(cursor).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
    let line = Line::from(vec![
        Span::styled(" 路径> ", theme::accent()),
        Span::raw(before),
        Span::styled(at, Style::new().add_modifier(Modifier::REVERSED)),
    ]);
    f.render_widget(Paragraph::new(line), inner);
    let x = inner.x + " 路径> ".width() as u16 + before_w;
    if x < inner.x + inner.width {
        f.set_cursor_position((x, inner.y));
    }
}

fn draw_cwd(f: &mut Frame, input: &[char], cursor: usize, area: Rect) {
    let inner = box_frame(f, area, 80, 5, "新建会话 · cwd（绝对路径，留空走 QAQH_DEFAULT_CWD>启动目录>当前会话）Enter 确认 · Esc 取消");
    let cursor = cursor.min(input.len());
    let before: String = input[..cursor].iter().collect();
    let before_w = before.width() as u16;
    let at: String = input.get(cursor).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
    let line = Line::from(vec![
        Span::styled(" cwd> ", theme::accent()),
        Span::raw(before),
        Span::styled(at, Style::new().add_modifier(Modifier::REVERSED)),
    ]);
    f.render_widget(Paragraph::new(line), inner);
    let x = inner.x + " cwd> ".width() as u16 + before_w;
    if x < inner.x + inner.width {
        f.set_cursor_position((x, inner.y));
    }
}

fn draw_confirm(f: &mut Frame, action: &ConfirmAction, area: Rect) {
    let (title, body) = match action {
        ConfirmAction::DeleteSession(seed) => ("确认删除", format!("彻底删除会话 {seed}？（磁盘数据不可恢复）")),
        ConfirmAction::ArchiveSession(seed) => ("确认归档", format!("归档会话 {seed}？")),
        ConfirmAction::CloseTab(seed) => ("确认关闭", format!("关闭标签 {seed}？（会话保留在列表中）")),
    };
    let inner = box_frame(f, area, 60, 7, title);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(format!(" {body}"), Style::new())));
    lines.push(Line::from(""));
    lines.push(modal::footer_line(&[("y", "确认"), ("n/Esc", "取消")]));
    f.render_widget(Paragraph::new(lines), inner);
}
