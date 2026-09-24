//! V2 alternate-screen Workspace。
//!
//! Workspace 只负责管理面与只读观测；正文历史仍由 Agent View 的 inline
//! viewport 与终端 scrollback 承担，因此这里不会调用 `insert_before`。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::edit_window;
use crate::app::settings::{FieldKind, ROWS, SettingsState};
use crate::app::{App, Overlay};
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

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

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
    f.render_widget(Paragraph::new(footer_line(route, theme)), footer);
}

fn header_line(route: &WorkspaceRoute, app: &App, theme: &Theme) -> Line<'static> {
    let (title, meta) = match route {
        WorkspaceRoute::Sessions { show_archived, .. } => (
            "Sessions",
            if *show_archived {
                "含归档"
            } else {
                "活动会话"
            }
            .to_owned(),
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

fn footer_line(route: &WorkspaceRoute, theme: &Theme) -> Line<'static> {
    let text = match route {
        WorkspaceRoute::Sessions { .. } => {
            " ↑↓ 选择 · Enter 打开 · n 新建 · x 归档 · u 恢复 · D 删除 · a 归档显示 · r 刷新 · Esc 返回"
        }
        WorkspaceRoute::Settings => {
            " ↑↓ 选择 · Enter 编辑/应用 · ←→ 切换 · s 保存 · r 刷新 · Esc 返回"
        }
        WorkspaceRoute::Help => " Esc 返回 Agent View",
        WorkspaceRoute::History { detail, .. } => {
            if *detail {
                " PgUp/PgDn 滚动 · e 导出此回合 · Esc 返回列表"
            } else {
                " ↑↓ 选择回合 · Enter 查看 · PgUp/PgDn 翻页 · Esc 返回 Agent View"
            }
        }
        WorkspaceRoute::Todo => " F4/Esc 返回 · PgUp/PgDn 滚动 · F6 详情",
        WorkspaceRoute::Subagent { .. } => {
            " Ctrl+↓/Esc 返回父会话 · PgUp/PgDn 滚动 · Ctrl+Home/End 顶部/底部"
        }
    };
    Line::from(Span::styled(text, Style::new().fg(theme.text.dim)))
}

fn draw_sessions(
    f: &mut Frame,
    app: &App,
    area: Rect,
    selected: usize,
    show_archived: bool,
    theme: &Theme,
) {
    let entries: Vec<_> = app
        .session_list_cache
        .iter()
        .filter(|m| show_archived || !m.meta.archived)
        .filter(|m| !m.meta.ephemeral)
        .collect();
    let height = usize::from(area.height);
    let selected = selected.min(entries.len().saturating_sub(1));
    let start = selected
        .saturating_sub(height.saturating_sub(1) / 2)
        .min(entries.len().saturating_sub(height));

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
            lines.push(if is_selected {
                line.style(
                    Style::new()
                        .bg(theme.chrome.selection)
                        .fg(theme.text.primary),
                )
            } else {
                line
            });
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// `/history`：按回合浏览当前会话。
///
/// 数据源是 **timeline 模型**（不是终端 scrollback —— 那个读不回来，也没有
/// 搜索）。列表回答"有哪些回合"，详情回答"这个回合到底说了什么"；两者都用
/// **同一份导出文本**，所以「详情里看到的 = 按 `e` 导出的」。
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
    let start = selected
        .saturating_sub(height.saturating_sub(1) / 2)
        .min(turns.len().saturating_sub(height));
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
        lines.push(if is_selected {
            line.style(Style::new().bg(theme.surface.highlight))
        } else {
            line
        });
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let Some(Overlay::Settings(state)) = app.overlays.last() else {
        return;
    };
    let width = usize::from(area.width);
    let (lines, row_lines) = settings_lines(state, app.config.as_ref(), width, theme);
    let height = usize::from(area.height).max(1);
    let focus = state.focus.min(ROWS.len().saturating_sub(1));
    let focus_line = row_lines.get(focus).copied().unwrap_or(0);
    let scroll = focus_line.saturating_sub(height.saturating_sub(1));
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
        " 默认 Agent View 使用 inline viewport；Workspace 与 Modal 使用 alternate screen。",
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

fn draw_todo(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let Some(session) = app.active_session() else {
        return;
    };
    let mut lines = Vec::new();
    let Some(dashboard) = session.dashboard.as_ref() else {
        lines.push(Line::from(Span::styled(
            " 尚无 todo · agent 使用 todo 工具后在这里实时更新",
            Style::new().fg(theme.text.dim),
        )));
        f.render_widget(Paragraph::new(lines), area);
        return;
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
    for task in &dashboard.tasks {
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
    let visible = visible_lines(
        &lines,
        area.height,
        session.scroll.follow,
        session.scroll.offset,
    );
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
    let height = usize::from(height).max(1);
    let total = lines.len();
    let top = if follow {
        total.saturating_sub(height)
    } else {
        total
            .saturating_sub(height)
            .saturating_sub(offset.min(total))
    };
    lines.iter().skip(top).take(height).cloned().collect()
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
