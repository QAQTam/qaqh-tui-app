//! V2 alternate-screen Workspace。
//!
//! Workspace 只负责管理面与只读观测；正文历史由 V2 fullscreen Agent View
//! 自己持有，因此这里不维护终端 scrollback。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::edit_window;
use crate::app::settings::{FieldKind, ROWS, SettingsHit, SettingsState};
use crate::app::{App, Overlay, WorkspaceHit};
use crate::protocol::ConfigDto;
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::route::WorkspaceRoute;
use crate::ui::v2::transcript::render_transcript;
use qaqh_client::TimelineTurnState;

pub fn draw(f: &mut Frame, app: &App, route: &WorkspaceRoute, theme: &Theme) {
    let area = f.area();
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::new().bg(theme.surface.base).fg(theme.text.primary)),
        area,
    );

    let [header, body, footer] = workspace_areas(area);

    f.render_widget(Paragraph::new(header_line(route, app, theme)), header);
    match route {
        WorkspaceRoute::Sessions {
            selected,
            show_archived,
        } => draw_sessions(f, app, body, *selected, *show_archived, theme),
        WorkspaceRoute::Settings => draw_settings(f, app, body, theme),
        WorkspaceRoute::Help => draw_help(f, body, theme),
        WorkspaceRoute::History {
            selected,
            detail,
            scroll,
        } => draw_history(f, app, body, *selected, *detail, *scroll, theme),
        WorkspaceRoute::Todo => draw_todo(f, app, body, theme),
        WorkspaceRoute::Subagent { seed } => draw_subagent(f, app, body, seed, theme),
    }
    let back = footer_back_visual(app, WorkspaceHit::Back);
    f.render_widget(Paragraph::new(footer_line(route, theme, back)), footer);
}

fn header_line(route: &WorkspaceRoute, app: &App, theme: &Theme) -> Line<'static> {
    let (title, meta) = match route {
        WorkspaceRoute::Sessions { show_archived, .. } => (
            "Sessions",
            if let Some(cwd) = app.session_cwd_filter.as_deref() {
                format!("当前 cwd · {}", crate::app::truncate_str(cwd, 48))
            } else if *show_archived {
                "含归档".to_owned()
            } else {
                "活动会话".to_owned()
            },
        ),
        WorkspaceRoute::Settings => ("Settings", "daemon 配置".to_owned()),
        WorkspaceRoute::Help => ("Help", "按键与斜杠命令".to_owned()),
        WorkspaceRoute::History {
            selected, detail, ..
        } => (
            "History",
            if *detail {
                format!("第 {} 回合详情", selected + 1)
            } else {
                let total = app
                    .active_session()
                    .map(|s| s.timeline.turns.len())
                    .unwrap_or(0);
                format!("{} 个回合（当前窗口）", total)
            },
        ),
        WorkspaceRoute::Todo => (
            "Workspace",
            app.active_session()
                .and_then(|s| s.display_model())
                .unwrap_or_else(|| "no model".to_owned()),
        ),
        WorkspaceRoute::Subagent { seed } => ("Subagent", seed.clone()),
    };
    Line::from(vec![
        Span::styled(
            " QAQH ",
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("/ ", Style::new().fg(theme.text.dim)),
        Span::styled(
            title,
            Style::new()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  · {meta}"), Style::new().fg(theme.text.dim)),
    ])
}

fn footer_line(route: &WorkspaceRoute, theme: &Theme, back: ButtonVisual) -> Line<'static> {
    let hint = match route {
        WorkspaceRoute::Sessions { .. } => {
            "↑↓ 选择 · Enter 打开 · n 新建 · x 归档 · u 恢复 · D 删除 · a 归档显示 · r 刷新"
        }
        WorkspaceRoute::Settings => "↑↓ 选择 · Enter 编辑/应用 · ←→ 切换 · s 保存 · r 刷新",
        WorkspaceRoute::Help => "返回 Agent View",
        WorkspaceRoute::History { detail, .. } => {
            if *detail {
                "PgUp/PgDn 滚动 · e 导出此回合"
            } else {
                "↑↓ 选择回合 · Enter 查看 · PgUp/PgDn 翻页"
            }
        }
        WorkspaceRoute::Todo => "PgUp/PgDn 滚动 · F6 详情",
        WorkspaceRoute::Subagent { .. } => {
            "Ctrl+↓ 返回父会话 · PgUp/PgDn 滚动 · Ctrl+Home/End 顶部/底部"
        }
    };
    Line::from(vec![
        Span::styled(BACK_LABEL, button_style(back, theme)),
        Span::styled(format!(" {hint}"), Style::new().fg(theme.text.dim)),
    ])
}

const BACK_LABEL: &str = " [ ← 返回 ] ";

fn workspace_areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

fn footer_back_area(footer: Rect) -> Rect {
    Rect {
        width: (BACK_LABEL.width() as u16).min(footer.width),
        ..footer
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ButtonVisual {
    hovered: bool,
    pressed: bool,
}

fn workspace_visual(app: &App, target: WorkspaceHit) -> ButtonVisual {
    let hovered = app.workspace_hover == Some(target);
    ButtonVisual {
        hovered,
        pressed: hovered && app.workspace_pressed == Some(target),
    }
}

fn footer_back_visual(app: &App, target: WorkspaceHit) -> ButtonVisual {
    workspace_visual(app, target)
}

fn button_style(visual: ButtonVisual, theme: &Theme) -> Style {
    let surface = |color| {
        if color == Color::Reset {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(color)
        }
    };
    if visual.pressed {
        surface(theme.surface.highlight).add_modifier(Modifier::BOLD)
    } else if visual.hovered {
        surface(theme.surface.hover)
    } else {
        Style::new()
    }
}

fn interactive_row_style(
    selected: bool,
    visual: ButtonVisual,
    selected_bg: Color,
    theme: &Theme,
) -> Style {
    let bg = if visual.pressed {
        Some(theme.surface.highlight)
    } else if visual.hovered {
        Some(theme.surface.hover)
    } else if selected {
        Some(selected_bg)
    } else {
        None
    };
    let Some(bg) = bg else {
        return Style::new();
    };
    let style = if bg == Color::Reset {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().bg(bg)
    };
    if visual.pressed {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn draw_sessions(
    f: &mut Frame,
    app: &App,
    area: Rect,
    selected: usize,
    show_archived: bool,
    theme: &Theme,
) {
    let indices = app.filtered_sessions(show_archived);
    let entries: Vec<_> = indices
        .iter()
        .filter_map(|index| app.session_list_cache.get(*index))
        .collect();
    let height = usize::from(area.height).max(1);
    let (start, selected) = session_list_window(app, area.height, selected, show_archived);

    let mut lines = Vec::with_capacity(height);
    if app.session_list_at.is_none() {
        lines.push(Line::from(Span::styled(
            " 加载中…",
            Style::new().fg(theme.text.dim),
        )));
    } else if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            " 暂无会话 · n 新建",
            Style::new().fg(theme.text.dim),
        )));
    } else {
        let width = usize::from(area.width);
        for (index, meta) in entries.iter().enumerate().skip(start).take(height) {
            let is_selected = index == selected;
            let title_width = width.saturating_sub(38).max(12);
            let title = fit_width(&meta.meta.display_title(), title_width);
            let updated = chrono::DateTime::from_timestamp_millis(meta.meta.updated_at as i64)
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let marker = if is_selected { "▶" } else { " " };
            let open = if app.tabs.contains(&meta.meta.seed) {
                "▣"
            } else if meta.meta.archived {
                "▤"
            } else {
                " "
            };
            let activity = app
                .activity_cache
                .get(&meta.meta.seed)
                .map(|value| format!("{value:?}"))
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(
                    format!(" {marker} {open} "),
                    Style::new().fg(if open != " " {
                        theme.accent.assistant
                    } else {
                        theme.text.dim
                    }),
                ),
                Span::styled(
                    pad_width(&title, title_width),
                    Style::new().fg(theme.text.primary),
                ),
                Span::styled(
                    format!("  {}", pad_width(&fit_width(&activity, 12), 12)),
                    Style::new().fg(theme.text.dim),
                ),
                Span::styled(format!(" {updated}"), Style::new().fg(theme.text.muted)),
                Span::styled(
                    format!("  #{}", meta.meta.seed),
                    Style::new().fg(theme.text.dim),
                ),
            ]);
            let visual = workspace_visual(app, WorkspaceHit::SessionRow(index));
            lines.push(
                line.patch_style(interactive_row_style(
                    is_selected,
                    visual,
                    theme.chrome.selection,
                    theme,
                ))
                .patch_style(Style::new().fg(theme.text.primary)),
            );
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn list_window(count: usize, height: u16, selected: usize) -> (usize, usize) {
    let height = usize::from(height).max(1);
    if count == 0 {
        return (0, 0);
    }
    let selected = selected.min(count.saturating_sub(1));
    let start = selected
        .saturating_sub(height.saturating_sub(1) / 2)
        .min(count.saturating_sub(height));
    (start, selected)
}

fn session_list_window(
    app: &App,
    height: u16,
    selected: usize,
    show_archived: bool,
) -> (usize, usize) {
    if app.session_list_at.is_none() {
        return (0, 0);
    }
    let count = app.filtered_sessions(show_archived).len();
    list_window(count, height, selected)
}

/// `/history`：按回合浏览当前会话。
///
/// 数据源是 **timeline 模型**。列表回答"有哪些回合"，详情回答"这个回合到底
/// 说了什么"；两者都用 **同一份导出文本**，所以「详情里看到的 = 按 `e`
/// 导出的」。
fn draw_history(
    f: &mut Frame,
    app: &App,
    area: Rect,
    selected: usize,
    detail: bool,
    scroll: usize,
    theme: &Theme,
) {
    let Some(session) = app.active_session() else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 没有活动会话",
                Style::new().fg(theme.text.dim),
            ))),
            area,
        );
        return;
    };
    let turns = &session.timeline.turns;
    if turns.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 这个会话还没有回合",
                Style::new().fg(theme.text.dim),
            ))),
            area,
        );
        return;
    }
    let selected = selected.min(turns.len().saturating_sub(1));

    if detail {
        let number = session.timeline.turn_number(selected) as usize;
        let markdown = crate::app::export::export_turn_markdown(&turns[selected], number);
        let lines: Vec<Line<'static>> = markdown
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_owned(),
                    Style::new().fg(theme.text.primary),
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(visible_lines(&lines, area.height, false, scroll)),
            area,
        );
        return;
    }

    let height = usize::from(area.height).max(1);
    let (start, _) = list_window(turns.len(), area.height, selected);
    let width = usize::from(area.width);
    let mut lines = Vec::with_capacity(height);
    for (index, turn) in turns.iter().enumerate().skip(start).take(height) {
        let is_selected = index == selected;
        let number = session.timeline.turn_number(index);
        let state = match turn.state {
            TimelineTurnState::Running => "…",
            TimelineTurnState::Failed => "✗",
            TimelineTurnState::Cancelled => "⊘",
            TimelineTurnState::Completed => " ",
        };
        let tools: usize = turn
            .rounds
            .iter()
            .flat_map(|round| round.blocks.iter())
            .filter(|block| block.tool.is_some())
            .count();
        let preview = turn.user_text.lines().next().unwrap_or("").trim();
        let preview_width = width.saturating_sub(30).max(10);
        let marker = if is_selected { "▶" } else { " " };
        let mut spans = vec![Span::styled(
            format!(
                " {marker} {number:>3} {state} {}",
                fit_width(preview, preview_width)
            ),
            Style::new().fg(if is_selected {
                theme.text.bright
            } else {
                theme.text.primary
            }),
        )];
        let mut meta = format!("  {tools} 工具");
        if turn.offloaded {
            meta.push_str(" · 已卸载");
        }
        if !turn.sealed {
            meta.push_str(" · 未封口");
        }
        spans.push(Span::styled(meta, Style::new().fg(theme.text.dim)));
        let line = Line::from(spans);
        let visual = workspace_visual(app, WorkspaceHit::HistoryTurn(index));
        lines.push(line.style(interactive_row_style(
            is_selected,
            visual,
            theme.surface.highlight,
            theme,
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let Some(Overlay::Settings(state)) = app.overlays.last() else {
        return;
    };
    let width = usize::from(area.width);
    let (mut lines, row_lines) = settings_lines(state, app.config.as_ref(), width, theme);
    let height = usize::from(area.height).max(1);
    let focus = state.focus.min(ROWS.len().saturating_sub(1));
    let focus_line = row_lines.get(focus).copied().unwrap_or(0);
    let scroll = focus_line.saturating_sub(height.saturating_sub(1));
    for (index, line_index) in row_lines.iter().enumerate() {
        let visual = workspace_visual(app, WorkspaceHit::SettingsRow(index));
        if (visual.hovered || visual.pressed)
            && let Some(line) = lines.get_mut(*line_index)
        {
            *line = line.clone().style(button_style(visual, theme));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        area,
    );

    if let Some(buffer) = &state.editing {
        let line = row_lines.get(focus).copied().unwrap_or(0);
        if line >= scroll && line < scroll.saturating_add(height) {
            let value_x = area.x.saturating_add(22);
            let value_width = usize::from(area.width).saturating_sub(22).max(1);
            let (_, offset) = edit_window(&buffer.buf, buffer.cursor, value_width);
            let x = value_x.saturating_add(offset as u16);
            let y = area.y.saturating_add((line - scroll) as u16);
            if x < area.x.saturating_add(area.width) {
                f.set_cursor_position((x, y));
            }
        }
    }
}

pub fn settings_hit_test(app: &App, area: Rect, column: u16, row: u16) -> Option<SettingsHit> {
    let Some(Overlay::Settings(state)) = app.overlays.last() else {
        return None;
    };
    if column < area.x
        || column >= area.x.saturating_add(area.width)
        || row < area.y
        || row >= area.y.saturating_add(area.height)
    {
        return None;
    }
    let (_, row_lines) = settings_lines(
        state,
        app.config.as_ref(),
        usize::from(area.width),
        Theme::current(),
    );
    let focus = state.focus.min(ROWS.len().saturating_sub(1));
    let focus_line = row_lines.get(focus).copied().unwrap_or(0);
    let height = usize::from(area.height).max(1);
    let scroll = focus_line.saturating_sub(height.saturating_sub(1));
    let line = scroll.saturating_add(usize::from(row.saturating_sub(area.y)));
    row_lines
        .iter()
        .position(|row_line| *row_line == line)
        .map(SettingsHit::Row)
}

/// Workspace 的统一命中测试。
///
/// 每个页面的可见窗口都调用绘制路径正在使用的同一个 helper；这里不复制
/// `start/scroll` 公式，避免鼠标 hover 与视觉行错位。
pub fn workspace_hit_test(
    app: &App,
    route: &WorkspaceRoute,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<WorkspaceHit> {
    let [_, body, footer] = workspace_areas(area);
    if rect_contains(footer_back_area(footer), column, row) {
        return Some(WorkspaceHit::Back);
    }
    if !rect_contains(body, column, row) {
        return None;
    }
    let local_row = usize::from(row.saturating_sub(body.y));
    match route {
        WorkspaceRoute::Sessions {
            selected,
            show_archived,
        } => {
            let (start, count) = session_list_window(app, body.height, *selected, *show_archived);
            (local_row < count.saturating_sub(start))
                .then_some(WorkspaceHit::SessionRow(start.saturating_add(local_row)))
        }
        WorkspaceRoute::History {
            selected,
            detail: false,
            ..
        } => {
            let count = app
                .active_session()
                .map(|session| session.timeline.turns.len())
                .unwrap_or(0);
            let (start, _) = list_window(count, body.height, *selected);
            (local_row < count.saturating_sub(start))
                .then_some(WorkspaceHit::HistoryTurn(start.saturating_add(local_row)))
        }
        WorkspaceRoute::Todo => {
            let layout = todo_layout(app, body, Theme::current());
            let line = layout.top.saturating_add(local_row);
            layout
                .task_ranges
                .iter()
                .position(|range| range.contains(&line))
                .map(WorkspaceHit::TodoTask)
        }
        WorkspaceRoute::Settings => settings_hit_test(app, body, column, row)
            .map(|SettingsHit::Row(index)| WorkspaceHit::SettingsRow(index)),
        WorkspaceRoute::Help
        | WorkspaceRoute::History { detail: true, .. }
        | WorkspaceRoute::Subagent { .. } => None,
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn settings_lines(
    state: &SettingsState,
    loaded: Option<&ConfigDto>,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut row_lines = vec![0; ROWS.len()];
    let mut section = "";
    let value_width = width.saturating_sub(24).max(8);
    for (index, row) in ROWS.iter().enumerate() {
        if row.section != section {
            section = row.section;
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                format!(" ── {section}"),
                Style::new()
                    .fg(theme.accent.assistant)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        row_lines[index] = lines.len();
        let focused = index == state.focus;
        let marker = if focused { "▶" } else { " " };
        let label = pad_width(row.label, 18);
        let (value, value_style) = if focused {
            if let Some(buffer) = &state.editing {
                let (window, _) = edit_window(&buffer.buf, buffer.cursor, value_width);
                (
                    window,
                    Style::new()
                        .fg(theme.text.primary)
                        .add_modifier(Modifier::REVERSED),
                )
            } else {
                settings_value(state, loaded, row.id, value_width, theme)
            }
        } else {
            settings_value(state, loaded, row.id, value_width, theme)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} "),
                Style::new().fg(if focused {
                    theme.accent.user
                } else {
                    theme.text.dim
                }),
            ),
            Span::styled(
                label,
                Style::new()
                    .fg(if focused {
                        theme.text.primary
                    } else {
                        theme.text.secondary
                    })
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled("  ", Style::new()),
            Span::styled(value, value_style),
        ]));
    }
    (lines, row_lines)
}

fn settings_value(
    state: &SettingsState,
    loaded: Option<&ConfigDto>,
    id: crate::app::settings::FieldId,
    width: usize,
    theme: &Theme,
) -> (String, Style) {
    let dirty = state.dirty(id);
    let value = fit_width(
        &state.display(loaded, id),
        width.saturating_sub(if dirty { 2 } else { 0 }),
    );
    let style = if dirty {
        Style::new().fg(theme.semantic.warning)
    } else {
        match ROWS.iter().find(|row| row.id == id).map(|row| row.kind) {
            Some(FieldKind::Secret | FieldKind::Port) => Style::new().fg(theme.text.dim),
            _ => Style::new().fg(theme.text.primary),
        }
    };
    (if dirty { format!("{value} *") } else { value }, style)
}

fn draw_help(f: &mut Frame, area: Rect, theme: &Theme) {
    let entries = [
        ("F1 / /help", "打开本帮助 Workspace"),
        ("Ctrl+L / /sessions", "打开会话列表 Workspace"),
        ("Ctrl+, / F10 / /settings", "打开设置 Workspace"),
        ("F4 / /workspace", "打开 todo Workspace"),
        ("Ctrl+↑", "进入子代理只读观测"),
        ("Ctrl+↓ / Esc", "返回父会话 / Agent View"),
        ("Enter", "发送消息"),
        ("Alt+Enter / Ctrl+J", "composer 换行"),
        ("Esc", "中止当前回合 / 关闭 Modal"),
        ("Ctrl+P", "切换 plan/code 模式"),
        ("Ctrl+Y", "撤销最后一个回合"),
        ("Ctrl+E", "压缩上下文"),
        ("Ctrl+A", "添加附件"),
        ("PgUp/PgDn", "滚动 Workspace / 观测视图"),
        ("Ctrl+C ×2 / Ctrl+Q", "退出"),
    ];
    let mut lines = Vec::with_capacity(entries.len() + 3);
    lines.push(Line::from(Span::styled(
        " Agent View、Workspace 与 Modal 都在 fullscreen shell 内。",
        Style::new().fg(theme.text.secondary),
    )));
    lines.push(Line::default());
    for (key, description) in entries {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {key:<24}"),
                Style::new()
                    .fg(theme.accent.user)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(description, Style::new().fg(theme.text.primary)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

struct TodoLayout {
    lines: Vec<Line<'static>>,
    task_ranges: Vec<Range<usize>>,
    top: usize,
}

fn todo_layout(app: &App, area: Rect, theme: &Theme) -> TodoLayout {
    let Some(session) = app.active_session() else {
        return TodoLayout {
            lines: Vec::new(),
            task_ranges: Vec::new(),
            top: 0,
        };
    };
    let mut lines = Vec::new();
    let mut task_ranges = Vec::new();
    let Some(dashboard) = session.dashboard.as_ref() else {
        lines.push(Line::from(Span::styled(
            " 尚无 todo · agent 使用 todo 工具后在这里实时更新",
            Style::new().fg(theme.text.dim),
        )));
        return TodoLayout {
            top: viewport_top(
                lines.len(),
                area.height,
                session.scroll.follow,
                session.scroll.offset,
            ),
            lines,
            task_ranges,
        };
    };

    let total = dashboard.tasks.len();
    let done = dashboard
        .tasks
        .iter()
        .filter(|task| task.status == "completed")
        .count();
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {done}/{total} 已完成"),
            Style::new().fg(if total > 0 && done == total {
                theme.accent.success
            } else {
                theme.accent.running
            }),
        ),
        Span::styled(
            dashboard
                .current_todo_id
                .as_deref()
                .map(|id| format!("  · 当前 {id}"))
                .unwrap_or_default(),
            Style::new().fg(theme.text.dim),
        ),
    ]));
    lines.push(Line::default());
    for (task_index, task) in dashboard.tasks.iter().enumerate() {
        let start = lines.len();
        let (glyph, style) = match task.status.as_str() {
            "in_progress" => ("◐", Style::new().fg(theme.accent.running)),
            "completed" => ("●", Style::new().fg(theme.accent.success)),
            "cancelled" => ("✕", Style::new().fg(theme.text.dim)),
            _ => ("○", Style::new().fg(theme.text.secondary)),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {glyph} "), style),
            Span::styled(
                format!("[{}] {}", task.id, task.subject),
                Style::new().fg(theme.text.primary),
            ),
        ]));
        if app.show_todo_detail {
            if !task.description.is_empty() {
                push_wrapped(
                    &mut lines,
                    "     ",
                    &task.description,
                    usize::from(area.width),
                    Style::new().fg(theme.text.secondary),
                );
            }
            if let Some(evidence) = task.evidence.as_deref().filter(|value| !value.is_empty()) {
                push_wrapped(
                    &mut lines,
                    "     证据: ",
                    evidence,
                    usize::from(area.width),
                    Style::new().fg(theme.semantic.verify),
                );
            }
        }
        let range = start..lines.len();
        let visual = workspace_visual(app, WorkspaceHit::TodoTask(task_index));
        if visual.hovered || visual.pressed {
            let style = interactive_row_style(false, visual, Color::Reset, theme);
            for line in &mut lines[range.clone()] {
                *line = line.clone().style(style);
            }
        }
        task_ranges.push(range);
    }
    if !dashboard.documents.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " 文档",
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD),
        )));
        for document in &dashboard.documents {
            lines.push(Line::from(vec![
                Span::styled("  · ", Style::new().fg(theme.text.dim)),
                Span::styled(
                    fit_width(&document.path, usize::from(area.width).saturating_sub(8)),
                    Style::new().fg(theme.text.secondary),
                ),
                Span::styled(
                    if document.is_stale { "  ⟡" } else { "" },
                    Style::new().fg(theme.semantic.warning),
                ),
            ]));
        }
    }
    let top = viewport_top(
        lines.len(),
        area.height,
        session.scroll.follow,
        session.scroll.offset,
    );
    TodoLayout {
        lines,
        task_ranges,
        top,
    }
}

fn draw_todo(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let layout = todo_layout(app, area, theme);
    let visible = layout
        .lines
        .iter()
        .skip(layout.top)
        .take(usize::from(area.height).max(1))
        .cloned()
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(visible), area);
}

fn draw_subagent(f: &mut Frame, app: &App, area: Rect, seed: &str, theme: &Theme) {
    let Some(session) = app.sessions.get(seed) else {
        f.render_widget(
            Paragraph::new(Span::styled(
                " 子代理会话已不可用",
                Style::new().fg(theme.text.dim),
            )),
            area,
        );
        return;
    };
    let blocks = adapter::from_turns(&session.timeline.turns);
    let mut lines = render_transcript(&blocks, area.width, theme);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " 子代理暂无 transcript",
            Style::new().fg(theme.text.dim),
        )));
    }
    let visible = visible_lines(
        &lines,
        area.height,
        session.scroll.follow,
        session.scroll.offset,
    );
    f.render_widget(Paragraph::new(visible), area);
}

fn visible_lines(
    lines: &[Line<'static>],
    height: u16,
    follow: bool,
    offset: usize,
) -> Vec<Line<'static>> {
    let top = viewport_top(lines.len(), height, follow, offset);
    lines
        .iter()
        .skip(top)
        .take(usize::from(height).max(1))
        .cloned()
        .collect()
}

fn viewport_top(total: usize, height: u16, follow: bool, offset: usize) -> usize {
    let height = usize::from(height).max(1);
    if follow {
        total.saturating_sub(height)
    } else {
        total
            .saturating_sub(height)
            .saturating_sub(offset.min(total))
    }
}

fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: usize,
    style: Style,
) {
    let prefix_width = prefix.width();
    for (index, segment) in
        crate::app::render_line::wrap_text(text, width.saturating_sub(prefix_width).max(1))
            .into_iter()
            .enumerate()
    {
        let prefix = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::new()),
            Span::styled(segment, style),
        ]));
    }
}

fn fit_width(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in value.chars() {
        let width = ch.width().unwrap_or(0);
        if used.saturating_add(width) > max_width.saturating_sub(1) {
            break;
        }
        result.push(ch);
        used = used.saturating_add(width);
    }
    result.push('…');
    result
}

fn pad_width(value: &str, width: usize) -> String {
    let current = value.width();
    if current >= width {
        value.to_owned()
    } else {
        format!("{value}{}", " ".repeat(width - current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::SessionState;
    use crate::theme::{ColorSupport, ThemeKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// 造一个带两回合的会话（第二回合带一个工具块）。
    fn app_with_turns() -> App {
        use crate::app::timeline_model::{Block, Round, ToolCard, Turn};
        use qaqh_client::{
            TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState,
        };

        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-history".to_string();
        let mut session = SessionState::new(seed.clone());
        session.timeline.turns = vec![
            Turn {
                turn_id: "t1".into(),
                turn_index: Some(1),
                user_text: "第一个问题：读一下文件".into(),
                state: TimelineTurnState::Completed,
                failure: None,
                sealed: true,
                offloaded: false,
                thinking: Default::default(),
                rounds: vec![Round {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![Block {
                        block_id: "b1".into(),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: "第一回合的回答".into(),
                        tool: None,
                        last_fragment: 0,
                        rev: 0,
                    }],
                }],
            },
            Turn {
                turn_id: "t2".into(),
                turn_index: Some(2),
                user_text: "第二个问题：跑一下测试".into(),
                state: TimelineTurnState::Completed,
                failure: None,
                sealed: true,
                offloaded: false,
                thinking: Default::default(),
                rounds: vec![Round {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![
                        Block {
                            block_id: "b2".into(),
                            block_order: 0,
                            kind: TimelineBlockKind::Text,
                            state: TimelineBlockState::Sealed,
                            text: "第二回合的回答".into(),
                            tool: None,
                            last_fragment: 0,
                            rev: 0,
                        },
                        Block {
                            block_id: "b3".into(),
                            block_order: 1,
                            kind: TimelineBlockKind::Tool,
                            state: TimelineBlockState::Sealed,
                            text: String::new(),
                            tool: Some(ToolCard {
                                tool_call_id: "tc2".into(),
                                name: "bash".into(),
                                state: TimelineToolState::Succeeded,
                                summary: None,
                                args_json: Some(r#"{"command":"cargo test"}"#.into()),
                                output: Some("ok".into()),
                                diff: None,
                                progress: String::new(),
                                progress_truncated: false,
                                progress_bytes_total: 0,
                                progress_stream: None,
                                failure: None,
                                permission: None,
                                display: None,
                            }),
                            last_fragment: 0,
                            rev: 0,
                        },
                    ],
                }],
            },
        ];
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    fn draw_history_route(app: &App, selected: usize, detail: bool, scroll: usize) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let route = WorkspaceRoute::History {
            selected,
            detail,
            scroll,
        };
        terminal
            .draw(|frame| draw(frame, app, &route, &theme()))
            .expect("draw history");
        text(&terminal)
    }

    /// `/history` 列表：每个回合都要能看见它的**用户问题**（否则用户无法在
    /// 回合之间做选择），并且带回合号。
    #[test]
    fn history_workspace_lists_turn_previews() {
        let app = app_with_turns();
        // ⚠ 断言前必须去掉空白：TestBackend 的 buffer 里宽字符占一个续格，
        // 直接拼出来是「第 一 个 问 题」（续格被当成空格）。
        let rendered: String = draw_history_route(&app, 1, false, 0)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(rendered.contains("第一个问题"), "{rendered}");
        assert!(rendered.contains("第二个问题"), "{rendered}");
        assert!(rendered.contains("2个回合"), "头部应给出回合数\n{rendered}");
    }

    /// 详情：渲染的是**与 `e` 导出同一份** Markdown，所以内容必须真的出现
    /// （而不是"列表里有个编号、点进去空白"）。
    #[test]
    fn history_detail_renders_turn_markdown() {
        let app = app_with_turns();
        let rendered: String = draw_history_route(&app, 1, true, 0)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(rendered.contains("第二回合的回答"), "{rendered}");
        assert!(
            rendered.contains("cargotest"),
            "工具调用也要在详情里\n{rendered}"
        );
        assert!(rendered.contains("e导出此回合"), "底部键位提示\n{rendered}");
    }

    #[test]
    fn help_workspace_renders_all_entrypoints() {
        let (app, _rx) = App::new_for_test();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &WorkspaceRoute::Help, &theme()))
            .expect("draw help");
        let output = text(&terminal);
        assert!(output.contains("/settings"));
        assert!(output.contains("/sessions"));
        assert!(output.contains("/workspace"));
    }

    #[test]
    fn settings_hit_test_maps_visible_rows_to_focus_targets() {
        let (mut app, _rx) = App::new_for_test();
        app.overlays
            .push(Overlay::Settings(SettingsState::default()));
        let body = Rect::new(0, 1, 100, 22);

        assert_eq!(
            settings_hit_test(&app, body, 5, body.y + 1),
            Some(SettingsHit::Row(0))
        );
        assert_eq!(
            settings_hit_test(&app, body, 5, body.y),
            None,
            "section header is not clickable"
        );
    }

    fn app_with_session_list() -> App {
        use qaqh_client::{SessionListEntry, SessionMeta};

        let (mut app, _rx) = App::new_for_test();
        app.session_list_cache = (0..6)
            .map(|index| SessionListEntry {
                meta: SessionMeta {
                    seed: format!("seed-{index}"),
                    created_at: index,
                    ..SessionMeta::default()
                },
                running: false,
                workspace_id: None,
            })
            .collect();
        app.session_list_at = Some(std::time::Instant::now());
        app
    }

    #[test]
    fn workspace_hit_test_matches_session_window() {
        let app = app_with_session_list();
        let area = Rect::new(0, 0, 100, 5);
        let route = WorkspaceRoute::Sessions {
            selected: 5,
            show_archived: false,
        };

        assert_eq!(
            workspace_hit_test(&app, &route, area, 5, 1),
            Some(WorkspaceHit::SessionRow(3)),
            "可视窗口从 selected 居中后的第 3 行开始"
        );
        assert_eq!(
            workspace_hit_test(&app, &route, area, 5, 2),
            Some(WorkspaceHit::SessionRow(4))
        );
        assert_eq!(
            workspace_hit_test(&app, &route, area, 5, area.height - 1),
            Some(WorkspaceHit::Back)
        );
    }

    #[test]
    fn workspace_hit_test_maps_history_and_todo_rows() {
        let history_app = app_with_turns();
        let history = WorkspaceRoute::History {
            selected: 1,
            detail: false,
            scroll: 0,
        };
        let area = Rect::new(0, 0, 100, 8);
        assert_eq!(
            workspace_hit_test(&history_app, &history, area, 5, 1),
            Some(WorkspaceHit::HistoryTurn(0))
        );
        assert_eq!(
            workspace_hit_test(&history_app, &history, area, 5, 2),
            Some(WorkspaceHit::HistoryTurn(1))
        );

        let (mut todo_app, _rx) = App::new_for_test();
        todo_app.tabs.push("seed".into());
        let mut session = SessionState::new("seed".into());
        session.dashboard = Some(qaqh_client::DomainDashboardSnapshot {
            seed: "seed".into(),
            documents: Vec::new(),
            recent_edits: Vec::new(),
            tasks: vec![qaqh_client::DashboardTask {
                id: "t1".into(),
                subject: "完成鼠标交互".into(),
                description: "点击任务行应命中".into(),
                status: "in_progress".into(),
                evidence: None,
            }],
            current_todo_id: Some("t1".into()),
        });
        todo_app.sessions.insert("seed".into(), session);
        assert_eq!(
            workspace_hit_test(&todo_app, &WorkspaceRoute::Todo, area, 5, 3),
            Some(WorkspaceHit::TodoTask(0)),
            "todo 第 3 行是任务主体（前两行是摘要和空行）"
        );
    }

    #[test]
    fn workspace_hover_paints_session_row_background() {
        let mut app = app_with_session_list();
        app.workspace_hover = Some(WorkspaceHit::SessionRow(0));
        let route = WorkspaceRoute::Sessions {
            selected: 0,
            show_archived: false,
        };
        let theme = theme();
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &route, &theme))
            .expect("draw sessions");
        assert_eq!(
            terminal.backend().buffer()[(1, 1)].bg,
            theme.surface.hover,
            "悬停行应有 surface.hover 底色"
        );
    }

    #[test]
    fn narrow_todo_workspace_keeps_cjk_safe() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        let mut session = SessionState::new("seed".into());
        session.dashboard = Some(qaqh_client::DomainDashboardSnapshot {
            seed: "seed".into(),
            documents: Vec::new(),
            recent_edits: Vec::new(),
            tasks: vec![qaqh_client::DashboardTask {
                id: "t1".into(),
                subject: "完成视觉重构".into(),
                description: "中文描述不应越界".into(),
                status: "in_progress".into(),
                evidence: None,
            }],
            current_todo_id: Some("t1".into()),
        });
        app.sessions.insert("seed".into(), session);
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &WorkspaceRoute::Todo, &theme()))
            .expect("draw todo");
        let output: String = text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(output.contains("完成视觉重构"));
    }
}
