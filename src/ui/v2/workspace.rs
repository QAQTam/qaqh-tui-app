//! V2 alternate-screen Workspace。
//!
//! Workspace 只负责管理面与只读观测；正文历史由 V2 fullscreen Agent View
//! 自己持有，因此这里不维护终端 scrollback。

use ratatui::Frame;
use ratatui::crossterm::event::MouseButton;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::edit_window;
use crate::app::settings::{FieldKind, ROWS, SettingsState};
use crate::app::{App, Overlay, WorkspaceHit};
use crate::protocol::ConfigDto;
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::button::{ButtonState, ButtonVisual};
use crate::ui::v2::hit::{HitMapBuilder, PointerTarget, line_region, z};
use crate::ui::v2::route::WorkspaceRoute;
use crate::ui::v2::transcript::render_transcript;
use qaqh_client::TimelineTurnState;

pub fn draw(
    f: &mut Frame,
    app: &App,
    route: &WorkspaceRoute,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
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
        } => draw_sessions(f, app, body, *selected, *show_archived, theme, hit_map),
        WorkspaceRoute::Settings => draw_settings(f, app, body, theme, hit_map),
        WorkspaceRoute::Help => draw_help(f, body, theme),
        WorkspaceRoute::History {
            selected,
            detail,
            scroll,
        } => draw_history(
            f,
            app,
            body,
            HistoryView {
                selected: *selected,
                detail: *detail,
                scroll: *scroll,
            },
            theme,
            hit_map,
        ),
        WorkspaceRoute::Todo => draw_todo(f, app, body, theme, hit_map),
        WorkspaceRoute::Subagents { selected, filter } => {
            draw_subagents(f, app, body, *selected, filter, theme, hit_map)
        }
        WorkspaceRoute::Subagent { seed } => draw_subagent(f, app, body, seed, theme),
    }
    let back = workspace_visual(app, WorkspaceHit::Back, false);
    let line = footer_line(route, theme, back);
    register_workspace_region(
        hit_map,
        footer_back_area(footer),
        footer,
        WorkspaceHit::Back,
        z::WORKSPACE_FOOTER,
        &line,
    );
    f.render_widget(Paragraph::new(line), footer);
}

/// 把一个 Workspace 语义目标登记进当前帧的 HitMap。
///
/// `rect` / `clip` 必须来自绘制阶段正在使用的布局；`line` 是真正渲染的那一行，
/// 锚点由 [`line_region`] 从它推导。
fn register_workspace_region(
    hit_map: &mut HitMapBuilder,
    rect: Rect,
    clip: Rect,
    target: WorkspaceHit,
    z: u16,
    line: &Line<'_>,
) {
    if let Some(region) = line_region(
        rect,
        clip,
        PointerTarget::Workspace(target),
        MouseButton::Left,
        true,
        z,
        line,
    ) {
        hit_map.push(region);
    }
}

/// body 内第 `offset` 行的整宽矩形。
fn row_rect(area: Rect, offset: usize) -> Rect {
    Rect {
        x: area.x,
        y: area.y.saturating_add(offset as u16),
        width: area.width,
        height: 1,
    }
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
        WorkspaceRoute::Subagents { filter, .. } => {
            let (agents, unread, revision, fact_seq) = app
                .active_team_state()
                .map(|team| {
                    (
                        team.roster(Some(filter)).len(),
                        team.inbox().len(),
                        team.revision(),
                        team.last_fact_seq(),
                    )
                })
                .unwrap_or((0, 0, 0, 0));
            let filter = if filter.is_empty() {
                String::new()
            } else {
                format!(" · filter {filter}")
            };
            (
                "Subagents",
                format!("{agents} agents · {unread} unread · r{revision}/f{fact_seq}{filter}"),
            )
        }
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
        WorkspaceRoute::Subagents { .. } => {
            "↑↓/j/k 选择 · Enter transcript · 输入 path prefix 过滤 · Backspace 清除 · r 刷新"
        }
        WorkspaceRoute::Subagent { .. } => {
            "Ctrl+↓ 返回父会话 · PgUp/PgDn 滚动 · Ctrl+Home/End 顶部/底部"
        }
    };
    Line::from(vec![
        Span::styled(BACK_LABEL, back.surface_style(theme, Color::Reset)),
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

/// 一行 / 一个按钮在这一帧的视觉。
///
/// 颜色与优先级规则统一在 `ui::v2::button`（spec §5），这里只把"指针语义状态"
/// 翻成 `ButtonVisual`。`focused` 是调用方的选中/焦点语义（列表行是 selected）。
fn workspace_visual(app: &App, target: WorkspaceHit, focused: bool) -> ButtonVisual {
    ButtonVisual::derive(
        true,
        focused,
        app.workspace_hover == Some(target),
        app.workspace_pressed == Some(target),
    )
}

fn draw_sessions(
    f: &mut Frame,
    app: &App,
    area: Rect,
    selected: usize,
    show_archived: bool,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
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
            let open = if app.tabs.contains(&meta.meta.session_id) {
                "▣"
            } else if meta.meta.archived {
                "▤"
            } else {
                " "
            };
            let activity = app
                .activity_cache
                .get(&meta.meta.session_id)
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
                    format!("  #{}", meta.meta.session_id),
                    Style::new().fg(theme.text.dim),
                ),
            ]);
            let visual = workspace_visual(app, WorkspaceHit::SessionRow(index), is_selected);
            let line = line
                .patch_style(visual.surface_style(theme, theme.chrome.selection))
                .patch_style(Style::new().fg(theme.text.primary));
            // 第 `index` 个会话画在 body 的第 `index - start` 行，登记用的就是这一行。
            register_workspace_region(
                hit_map,
                row_rect(area, index.saturating_sub(start)),
                area,
                WorkspaceHit::SessionRow(index),
                z::WORKSPACE_ROW,
                &line,
            );
            lines.push(line);
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
/// history 视图参数：列表/详情 + 选中项 + 详情滚动量。
#[derive(Debug, Clone, Copy)]
struct HistoryView {
    selected: usize,
    detail: bool,
    scroll: usize,
}

fn draw_history(
    f: &mut Frame,
    app: &App,
    area: Rect,
    view: HistoryView,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
    let HistoryView {
        selected,
        detail,
        scroll,
    } = view;
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
        let visual = workspace_visual(app, WorkspaceHit::HistoryTurn(index), is_selected);
        let line = line.style(visual.surface_style(theme, theme.surface.highlight));
        register_workspace_region(
            hit_map,
            row_rect(area, index.saturating_sub(start)),
            area,
            WorkspaceHit::HistoryTurn(index),
            z::WORKSPACE_ROW,
            &line,
        );
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect, theme: &Theme, hit_map: &mut HitMapBuilder) {
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
        // 设置页的键盘焦点由 `▶` 前缀表达，不靠底色（spec §5.2 的 Focused 档
        // 在这里刻意留空），所以 `focused = false`。
        let visual = workspace_visual(app, WorkspaceHit::SettingsRow(index), false);
        if visual.state() != ButtonState::Idle
            && let Some(line) = lines.get_mut(*line_index)
        {
            *line = line
                .clone()
                .style(visual.surface_style(theme, Color::Reset));
        }
    }
    // 设置项可能被 focus 顶到滚动位置；只登记当前视口里的行，屏幕 y 由
    // `line_index - scroll` 推出，和 `settings_hit_test` 的公式同源。
    for (index, line_index) in row_lines.iter().enumerate() {
        if *line_index < scroll || *line_index >= scroll.saturating_add(height) {
            continue;
        }
        let Some(line) = lines.get(*line_index) else {
            continue;
        };
        register_workspace_region(
            hit_map,
            row_rect(area, line_index.saturating_sub(scroll)),
            area,
            WorkspaceHit::SettingsRow(index),
            z::WORKSPACE_ROW,
            line,
        );
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

/// Workspace 的命中不再有独立入口：`draw` 在真实 render 位置把每个目标登记进
/// `HitMapBuilder`（P0-B-4 删掉了旧的 `workspace_hit_test` / `settings_hit_test`），
/// 事件只查已发布的 `FrameHitMap`。
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
        let visual = workspace_visual(app, WorkspaceHit::TodoTask(task_index), false);
        if visual.state() != ButtonState::Idle {
            let style = visual.surface_style(theme, Color::Reset);
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

fn draw_todo(f: &mut Frame, app: &App, area: Rect, theme: &Theme, hit_map: &mut HitMapBuilder) {
    let layout = todo_layout(app, area, theme);
    let height = usize::from(area.height).max(1);
    let visible_end = layout.top.saturating_add(height);
    // 一个 todo 可能占多行（详情展开），命中区覆盖它当前可见的那几行。
    for (index, range) in layout.task_ranges.iter().enumerate() {
        let start = range.start.max(layout.top);
        let end = range.end.min(visible_end);
        if start >= end {
            continue;
        }
        let Some(line) = layout.lines.get(start) else {
            continue;
        };
        let rect = Rect {
            x: area.x,
            y: area.y.saturating_add((start - layout.top) as u16),
            width: area.width,
            height: (end - start) as u16,
        };
        register_workspace_region(
            hit_map,
            rect,
            area,
            WorkspaceHit::TodoTask(index),
            z::WORKSPACE_ROW,
            line,
        );
    }
    let visible = layout
        .lines
        .iter()
        .skip(layout.top)
        .take(height)
        .cloned()
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(visible), area);
}

fn draw_subagents(
    f: &mut Frame,
    app: &App,
    area: Rect,
    selected: usize,
    filter: &str,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
    let Some(team) = app.active_team_state() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                " 正在加载 Team projection…",
                Style::new().fg(theme.text.dim),
            )),
            area,
        );
        return;
    };
    if area.width < 2 || area.height == 0 {
        return;
    }
    let [roster_area, inbox_area] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);
    draw_team_roster(
        f,
        app,
        team,
        roster_area,
        TeamRosterView { selected, filter },
        theme,
        hit_map,
    );
    draw_team_inbox(f, team, inbox_area, theme);
}

struct TeamRosterView<'a> {
    selected: usize,
    filter: &'a str,
}

fn draw_team_roster(
    f: &mut Frame,
    app: &App,
    team: &crate::app::team::TeamState,
    area: Rect,
    view: TeamRosterView<'_>,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
    let TeamRosterView { selected, filter } = view;
    let roster = team.roster(Some(filter));
    let height = usize::from(area.height).max(1);
    let start = selected.saturating_sub(height.saturating_sub(1));
    let visible = roster.iter().enumerate().skip(start).take(height);
    let mut lines = Vec::new();
    for (index, agent) in visible {
        let focused = index == selected;
        let visual = workspace_visual(app, WorkspaceHit::SubagentRow(index), focused);
        let role = agent.role.as_deref().unwrap_or("-");
        let nickname = agent.nickname.as_deref().unwrap_or("-");
        let status = crate::app::team::status_label(agent.status);
        let residency = crate::app::team::residency_label(agent.residency);
        let line = Line::from(vec![
            Span::styled(if focused { "› " } else { "  " }, visual.foreground(theme)),
            Span::styled(
                format!(
                    "{:<18}",
                    crate::app::truncate_str(agent.agent_path.as_str(), 18)
                ),
                visual.foreground(theme),
            ),
            Span::styled(
                format!(" {:<8}", crate::app::truncate_str(role, 8)),
                Style::new().fg(theme.text.dim),
            ),
            Span::styled(
                format!(" {:<8}", crate::app::truncate_str(nickname, 8)),
                Style::new().fg(theme.text.dim),
            ),
            Span::styled(
                format!(" {:<9}", status),
                Style::new().fg(theme.text.primary),
            ),
            Span::styled(
                format!(" {residency}"),
                if agent.residency == qaqh_client::ClientV2TeamAgentResidency::Unloaded {
                    Style::new().fg(theme.text.dim)
                } else {
                    Style::new().fg(theme.accent.assistant)
                },
            ),
        ])
        .style(visual.surface_style(theme, Color::Reset));
        let rect = row_rect(area, index.saturating_sub(start));
        register_workspace_region(
            hit_map,
            rect,
            area,
            WorkspaceHit::SubagentRow(index),
            z::WORKSPACE_ROW,
            &line,
        );
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            if filter.is_empty() {
                "  暂无 agent；等待 TeamSnapshot".to_owned()
            } else {
                format!("  无匹配 path prefix：{filter}")
            },
            Style::new().fg(theme.text.dim),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_team_inbox(f: &mut Frame, team: &crate::app::team::TeamState, area: Rect, theme: &Theme) {
    let mut lines = vec![Line::from(Span::styled(
        " INBOX",
        Style::new()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    ))];
    if team.inbox().is_empty() {
        lines.push(Line::from(Span::styled(
            "  无未读消息",
            Style::new().fg(theme.text.dim),
        )));
    } else {
        for message in team.inbox() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        " {} → {}",
                        message.author.as_str(),
                        message.recipient.as_str()
                    ),
                    Style::new().fg(theme.text.primary),
                ),
                Span::styled(
                    format!("  [{}]", crate::app::team::delivery_label(message.delivery)),
                    Style::new().fg(theme.accent.assistant),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!("   task {}", message.task_id.as_deref().unwrap_or("(none)")),
                Style::new().fg(theme.text.dim),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
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
    use crate::ui::v2::hit::{FrameHitMap, FrameId};
    use crate::ui::v2::route::ScreenRoute;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    /// 造一个空 HitMapBuilder；测试直接调 `draw` 时用它吸收登记结果。
    fn scratch_map(route: &WorkspaceRoute, width: u16, height: u16) -> HitMapBuilder {
        HitMapBuilder::new(
            FrameId::new(1),
            ScreenRoute::Workspace(route.clone()),
            ratatui::layout::Size::new(width, height),
            0,
        )
    }

    /// 跑真实 `workspace::draw` 并把这一帧的 HitMap 取出来。
    fn draw_workspace_to_map(
        app: &App,
        route: &WorkspaceRoute,
        width: u16,
        height: u16,
    ) -> (FrameHitMap, TestBackend) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut builder = scratch_map(route, width, height);
        terminal
            .draw(|frame| draw(frame, app, route, &theme(), &mut builder))
            .expect("draw workspace");
        let map = builder.finish();
        assert!(
            map.validate().is_ok(),
            "绘制阶段登记的 HitMap 必须通过几何校验：{:?}",
            map.validate().err()
        );
        let probe = map.probe(terminal.backend().buffer(), map.frame_id, &map.route);
        assert!(
            probe.is_ok(),
            "真实帧必须通过 strict 探针：{:?}",
            probe.err()
        );
        (map, terminal.backend().clone())
    }

    fn region_for<'a>(
        map: &'a FrameHitMap,
        target: &WorkspaceHit,
    ) -> &'a crate::ui::v2::hit::HitRegion {
        let wanted = PointerTarget::Workspace(*target);
        map.regions
            .iter()
            .find(|region| region.target == wanted)
            .unwrap_or_else(|| panic!("HitMap 必须登记 {target:?}"))
    }

    /// 每个登记区的视觉锚点在真实 buffer 里必须非空——P0-C strict probe 的预演。
    fn assert_anchors_non_empty(backend: &TestBackend, map: &FrameHitMap) {
        let buffer = backend.buffer();
        for region in &map.regions {
            let position = region.anchor.position;
            let cell = &buffer[(position.x, position.y)];
            assert!(
                !cell.symbol().trim().is_empty(),
                "目标 {:?} 的锚点 {:?} 落在空 cell 上",
                region.target,
                position
            );
        }
    }

    /// 命中区四角可达、外扩一格不可达。
    fn assert_region_reachable(map: &FrameHitMap, target: &WorkspaceHit) {
        let rect = region_for(map, target).rect;
        assert!(!rect.is_empty(), "{target:?} 的矩形不能为空");
        let wanted = PointerTarget::Workspace(*target);
        for (x, y) in [
            (rect.x, rect.y),
            (rect.right() - 1, rect.y),
            (rect.x, rect.bottom() - 1),
            (rect.right() - 1, rect.bottom() - 1),
            (rect.x + rect.width / 2, rect.y + rect.height / 2),
        ] {
            let hit = map
                .resolve(x, y, MouseButton::Left)
                .expect("同 z 区域不得重叠");
            assert_eq!(
                hit.map(|region| &region.target),
                Some(&wanted),
                "({x},{y}) 应命中 {target:?}"
            );
        }
        for (x, y) in [
            (rect.x.saturating_sub(1), rect.y),
            (rect.x, rect.y.saturating_sub(1)),
            (rect.right(), rect.y),
            (rect.x, rect.bottom()),
        ] {
            if x >= map.terminal_size.width
                || y >= map.terminal_size.height
                || crate::ui::v2::hit::contains(rect, x, y)
            {
                continue;
            }
            let hit = map
                .resolve(x, y, MouseButton::Left)
                .expect("同 z 区域不得重叠");
            assert_ne!(
                hit.map(|region| &region.target),
                Some(&wanted),
                "({x},{y}) 在 {target:?} 外扩一格内，不该命中"
            );
        }
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
        let mut hit_map = scratch_map(&route, 100, 24);
        terminal
            .draw(|frame| draw(frame, app, &route, &theme(), &mut hit_map))
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
        let route = WorkspaceRoute::Help;
        let mut hit_map = scratch_map(&route, 100, 30);
        terminal
            .draw(|frame| draw(frame, &app, &route, &theme(), &mut hit_map))
            .expect("draw help");
        let output = text(&terminal);
        assert!(output.contains("/settings"));
        assert!(output.contains("/sessions"));
        assert!(output.contains("/workspace"));
    }

    #[test]
    fn subagents_workspace_renders_roster_inbox_and_unloaded_state() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("root".into());
        app.sessions.insert(
            "root".into(),
            crate::app::session::SessionState::new("root".into()),
        );
        let snapshot: qaqh_client::ClientV2TeamSnapshot =
            serde_json::from_value(serde_json::json!({
                "root_session_id": "root",
                "agents": [
                    {
                        "agent_id": "root",
                        "agent_path": "/root",
                        "role": "root",
                        "status": "running",
                        "residency": "loaded"
                    },
                    {
                        "agent_id": "child",
                        "agent_path": "/root/reviewer",
                        "nickname": "reviewer",
                        "role": "review",
                        "status": "running",
                        "residency": "unloaded",
                        "parent_agent_path": "/root"
                    }
                ],
                "unread_messages": [
                    {
                        "message_id": "msg-1",
                        "author": "/root",
                        "recipient": "/root/reviewer",
                        "task_id": "task-1",
                        "delivery": "steer",
                        "created_at_ms": 1
                    }
                ],
                "revision": 3,
                "last_fact_seq": 9
            }))
            .expect("team snapshot");
        app.teams
            .entry("root".into())
            .or_default()
            .replace_from_snapshot(snapshot);
        let route = WorkspaceRoute::Subagents {
            selected: 1,
            filter: String::new(),
        };
        let (map, backend) = draw_workspace_to_map(&app, &route, 100, 24);
        let rendered: String = backend
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("/root/reviewer"), "{rendered}");
        assert!(rendered.contains("unloaded"), "{rendered}");
        assert!(rendered.contains("INBOX"), "{rendered}");
        assert!(rendered.contains("steer"), "{rendered}");
        assert!(rendered.contains("task-1"), "{rendered}");
        assert_region_reachable(&map, &WorkspaceHit::SubagentRow(0));
        assert_region_reachable(&map, &WorkspaceHit::SubagentRow(1));
    }

    fn app_with_session_list() -> App {
        use qaqh_client::{SessionListEntry, SessionMeta};

        let (mut app, _rx) = App::new_for_test();
        app.session_list_cache = (0..6)
            .map(|index| SessionListEntry {
                meta: SessionMeta {
                    session_id: format!("seed-{index}"),
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

    /// 真实 `draw` 生成的 HitMap 必须把会话窗口里的每一行映射成语义目标，
    /// 且窗口外的行不登记（不可见即不可点）。
    #[test]
    fn sessions_draw_registers_visible_rows_and_back() {
        let app = app_with_session_list();
        let route = WorkspaceRoute::Sessions {
            selected: 5,
            show_archived: false,
        };
        let (map, backend) = draw_workspace_to_map(&app, &route, 100, 5);
        assert_anchors_non_empty(&backend, &map);

        // body 只有 3 行，selected=5 居中后窗口是 3..6。
        for index in 3..6 {
            assert_region_reachable(&map, &WorkspaceHit::SessionRow(index));
        }
        assert_region_reachable(&map, &WorkspaceHit::Back);
        assert!(
            !map.regions.iter().any(
                |region| region.target == PointerTarget::Workspace(WorkspaceHit::SessionRow(0))
            ),
            "滚出视口的行不能登记"
        );
    }

    /// history 列表登记每个可见回合；detail 模式没有行目标，只剩 Back。
    #[test]
    fn history_draw_registers_rows_only_in_list_mode() {
        let app = app_with_turns();
        let list = WorkspaceRoute::History {
            selected: 0,
            detail: false,
            scroll: 0,
        };
        let (map, backend) = draw_workspace_to_map(&app, &list, 100, 8);
        assert_anchors_non_empty(&backend, &map);
        assert_region_reachable(&map, &WorkspaceHit::HistoryTurn(0));
        assert_region_reachable(&map, &WorkspaceHit::HistoryTurn(1));
        assert_region_reachable(&map, &WorkspaceHit::Back);

        let detail = WorkspaceRoute::History {
            selected: 0,
            detail: true,
            scroll: 0,
        };
        let (detail_map, detail_backend) = draw_workspace_to_map(&app, &detail, 100, 8);
        assert_anchors_non_empty(&detail_backend, &detail_map);
        assert!(
            !detail_map.regions.iter().any(|region| matches!(
                region.target,
                PointerTarget::Workspace(WorkspaceHit::HistoryTurn(_))
            )),
            "detail 视图不该登记回合行"
        );
        assert_region_reachable(&detail_map, &WorkspaceHit::Back);
    }

    /// settings 只登记可见的设置行；section 头不可点。
    #[test]
    fn settings_draw_registers_visible_rows() {
        let (mut app, _rx) = App::new_for_test();
        app.overlays
            .push(Overlay::Settings(SettingsState::default()));
        let route = WorkspaceRoute::Settings;
        let (map, backend) = draw_workspace_to_map(&app, &route, 100, 24);
        assert_anchors_non_empty(&backend, &map);

        assert_region_reachable(&map, &WorkspaceHit::SettingsRow(0));
        assert_region_reachable(&map, &WorkspaceHit::Back);

        let body = workspace_areas(Rect::new(0, 0, 100, 24))[1];
        assert_eq!(
            map.resolve(body.x + 5, body.y, MouseButton::Left)
                .expect("同 z 区域不得重叠"),
            None,
            "section 头不是可点目标"
        );
    }

    fn app_with_todo_tasks() -> App {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        let mut session = SessionState::new("seed".into());
        session.dashboard = Some(qaqh_client::DomainDashboardSnapshot {
            session_id: "seed".into(),
            documents: Vec::new(),
            recent_edits: Vec::new(),
            tasks: vec![
                qaqh_client::DashboardTask {
                    id: "t1".into(),
                    subject: "完成鼠标交互".into(),
                    description: String::new(),
                    status: "in_progress".into(),
                    evidence: None,
                },
                qaqh_client::DashboardTask {
                    id: "t2".into(),
                    subject: "补齐命中测试".into(),
                    description: String::new(),
                    status: "pending".into(),
                    evidence: None,
                },
            ],
            current_todo_id: Some("t1".into()),
        });
        app.sessions.insert("seed".into(), session);
        app
    }

    /// todo 的每个任务行都要登记成独立目标。
    #[test]
    fn todo_draw_registers_task_rows() {
        let app = app_with_todo_tasks();
        let route = WorkspaceRoute::Todo;
        let (map, backend) = draw_workspace_to_map(&app, &route, 100, 12);
        assert_anchors_non_empty(&backend, &map);
        assert_region_reachable(&map, &WorkspaceHit::TodoTask(0));
        assert_region_reachable(&map, &WorkspaceHit::TodoTask(1));
        assert_region_reachable(&map, &WorkspaceHit::Back);
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
        let mut hit_map = scratch_map(&route, 100, 8);
        terminal
            .draw(|frame| draw(frame, &app, &route, &theme, &mut hit_map))
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
            session_id: "seed".into(),
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
        let route = WorkspaceRoute::Todo;
        let mut hit_map = scratch_map(&route, 24, 8);
        terminal
            .draw(|frame| draw(frame, &app, &route, &theme(), &mut hit_map))
            .expect("draw todo");
        let output: String = text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(output.contains("完成视觉重构"));
    }
}
