//! V2 fullscreen agent shell.
//!
//! Owns fullscreen view state, transcript cache, hit-testing, message menus and
//! fullscreen rendering. Shared composer/status helpers remain in the parent
//! `agent` module.

use super::*;
use crate::ui::v2::adapter;
use crate::ui::v2::fullscreen as ui_fullscreen;
use crate::ui::v2::fullscreen::{FullscreenState, MessageAction, MessageMenu, MessageRole};
use crate::ui::v2::hit::{
    AgentTarget, HitMapBuilder, PointerTarget, ScrollbarPart, VisualAnchor, anchor_region,
    line_region, z,
};
use crate::ui::v2::scrollbar::ScrollbarMetrics;
use crate::ui::v2::sidebar;
use crate::ui::v2::transcript::{TodoBlock, compose_todo_panel};
use ratatui::crossterm::event::MouseButton;

const NARROW_VIEWPORT_WIDTH: u16 = 40;

/// 子代理预览条的最低可用宽度。
///
/// 条子本身约 16 列（缩进 + 方框），再窄就只剩它自己、连状态行都挤没了——那种
/// 宽度下 Ctrl+↑ 仍然是完整入口，不值得为它牺牲状态行。
const SUBAGENT_STRIP_MIN_WIDTH: usize = 32;

/// sticky 待办面板的行数上限。
///
/// 面板是**常驻摘要**不是主视图：完整清单在 F4 的 Workspace Todo 面板。取 6 行
/// （标题 + 5 项）——24 行终端下连 composer 带一共占掉约三分之一，再多就喧宾
/// 夺主了。
const TODO_PANEL_MAX_ROWS: usize = 6;

pub(super) fn handle_fullscreen_menu_key(
    app: &mut App,
    view: &mut FullscreenView,
    key: &ratatui::crossterm::event::KeyEvent,
) {
    use ratatui::crossterm::event::KeyModifiers;

    match key.code {
        KeyCode::Char('q' | 'c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.quit = true;
        }
        KeyCode::Up => {
            if let Some(menu) = view.menu.as_mut() {
                menu.move_selection(-1);
            }
        }
        KeyCode::Down => {
            if let Some(menu) = view.menu.as_mut() {
                menu.move_selection(1);
            }
        }
        KeyCode::Enter | KeyCode::Char('c') => {
            if let Some(action) = view.menu.as_ref().and_then(MessageMenu::activate) {
                activate_message_action(app, view, action);
            }
        }
        KeyCode::Esc => view.close_menu(),
        _ => {}
    }
}

pub(super) fn activate_message_action(
    app: &mut App,
    view: &mut FullscreenView,
    action: MessageAction,
) {
    match action {
        MessageAction::CopyMarkdown => {
            let copied = view.menu.as_ref().and_then(|menu| {
                assistant_markdown(app, &menu.turn_id, &menu.block_id).map(|markdown| {
                    crate::terminal::clipboard::copy_osc52(&markdown)
                        .map_err(|error| error.to_string())
                })
            });
            match copied {
                Some(Ok(())) => {
                    app.toast(NoticeLevel::Info, "已复制 Markdown");
                    view.close_menu();
                }
                Some(Err(error)) => {
                    app.toast(NoticeLevel::Error, format!("复制失败：{error}"));
                }
                None => {
                    app.toast(NoticeLevel::Error, "找不到可复制的助手正文");
                    view.close_menu();
                }
            }
        }
        MessageAction::UndoFromHere => {
            if let Some(turn_id) = view.menu.as_ref().map(|menu| menu.turn_id.clone()) {
                app.confirm_undo_turn(turn_id);
            }
            view.close_menu();
        }
        MessageAction::Retry | MessageAction::Fork => {}
    }
}

pub(super) fn assistant_markdown(app: &App, turn_id: &str, block_id: &str) -> Option<String> {
    let session = app.active_session()?;
    let turn = session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)?;
    let mut exact = None;
    let mut all = Vec::new();
    for block in turn.rounds.iter().flat_map(|round| &round.blocks) {
        if block.kind != TimelineBlockKind::Text || block.text.trim().is_empty() {
            continue;
        }
        all.push(block.text.as_str());
        if block.block_id == block_id {
            exact = Some(block.text.clone());
        }
    }
    exact.or_else(|| (!all.is_empty()).then(|| all.join("\n\n")))
}

/// 弹窗里的鼠标：移动只改悬停；按下记目标；**松开且仍在同一目标上**才提交。
///
/// 事件量：`EnableMouseCapture` 会开 `?1003h`（任意移动上报），移动事件可能很密。
/// 这里不排队也不重绘——`run_loop` 每次循环先把 `app_rx` 里积压的消息一次性抽干
/// 再画一帧，天然就是"合并到最新一帧"。
pub(super) fn draw_fullscreen_agent(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    view: &mut FullscreenView,
    hit_map: &mut HitMapBuilder,
) {
    let for_background = hit_map.is_suspended();
    draw_fullscreen_agent_inner(frame, app, theme, view, hit_map, for_background);
}

/// `for_background = true`：agent 视图这一帧是阻塞卡片的背景——照常渲染、
/// 命中登记由挂起的 builder 吞掉，但**不抢硬件光标**（光标属于前景卡片，
/// 比如 ask 的自定义输入行）。
pub(super) fn draw_fullscreen_agent_inner(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    view: &mut FullscreenView,
    hit_map: &mut HitMapBuilder,
    for_background: bool,
) {
    let full = frame.area();
    // 左侧常驻会话栏：有 active session 且终端够宽时才占位；
    // 其余渲染全部收缩到右侧剩余区域，命中几何随之一致。
    let rail = if app.active_session().is_some() {
        sidebar::rail_width(full.width)
    } else {
        0
    };
    let rail_area = Rect::new(full.x, full.y, rail, full.height);
    let area = Rect::new(
        full.x.saturating_add(rail),
        full.y,
        full.width.saturating_sub(rail),
        full.height,
    );
    let rendered = render_fullscreen_agent(app, area.width, area.height, theme, view);
    frame.render_widget(Paragraph::new(rendered.lines), area);
    // 子代理预览条：点它 = `Ctrl+↑`（进入 / 循环子代理视图）。几何来自渲染器
    // 自己写回的那一份，不在这里重算。
    if let Some(strip) = rendered.subagent_strip {
        let rect = Rect::new(
            area.x.saturating_add(strip.x),
            area.y.saturating_add(strip.y),
            strip.width,
            1,
        );
        if let Some(region) = anchor_region(
            rect,
            area,
            PointerTarget::Agent(AgentTarget::Subagents),
            MouseButton::Left,
            true,
            z::AGENT_OVERLAY,
            VisualAnchor::non_empty(Position::new(rect.x, rect.y)),
        ) {
            hit_map.push(region);
        }
    }
    if rail > 0 {
        sidebar::draw(
            frame,
            app,
            rail_area,
            theme,
            &view.pointer,
            &mut view.sidebar_anim,
            hit_map,
        );
    }
    if let Some(cursor) = rendered.cursor
        && !for_background
    {
        frame.set_cursor_position((
            area.x.saturating_add(cursor.x),
            area.y.saturating_add(cursor.y),
        ));
    }

    let (body, _) = fullscreen_layout(app, area, theme, view.current_todo.as_ref());
    if !body.is_empty()
        && let Some(session) = app.active_session()
    {
        let transcript_len = view.transcripts.len_for(&session.session_id);
        ui_fullscreen::draw_scrollbar(
            frame,
            body,
            transcript_len,
            usize::from(view.body_height),
            session.scroll.follow,
            session.scroll.offset,
            theme,
        );
        let track = Rect::new(
            body.x.saturating_add(body.width.saturating_sub(1)),
            body.y,
            1,
            body.height,
        );
        if let Some(metrics) = ScrollbarMetrics::new(
            track,
            transcript_len,
            usize::from(view.body_height),
            session.scroll.follow,
            session.scroll.offset,
        ) {
            if let Some(region) = anchor_region(
                metrics.track,
                body,
                PointerTarget::Scrollbar(ScrollbarPart::Track),
                MouseButton::Left,
                true,
                z::AGENT_SCROLLBAR,
                VisualAnchor::non_empty(Position::new(metrics.track.x, metrics.track.y)),
            ) {
                hit_map.push(region);
            }
            if let Some(region) = anchor_region(
                metrics.thumb,
                body,
                PointerTarget::Scrollbar(ScrollbarPart::Thumb),
                MouseButton::Left,
                true,
                z::AGENT_SCROLLBAR_THUMB,
                VisualAnchor::glyph(Position::new(metrics.thumb.x, metrics.thumb.y), '┃'),
            ) {
                hit_map.push(region);
            }
        }
    }
    // 消息行与浮层都按**刚画完的这一帧**登记：行窗口来自 render 写回的
    // `visible_start`，按钮/菜单矩形复用 ui_fullscreen 的渲染几何。
    let active_session_id = app.active_session_id();
    register_agent_messages(
        hit_map,
        body,
        view,
        active_session_id.as_deref().unwrap_or(""),
    );
    let show_back_to_latest = view.can_scroll(app)
        && app
            .active_session()
            .is_some_and(|session| !session.scroll.follow);
    if show_back_to_latest {
        ui_fullscreen::draw_back_to_latest(frame, body, view.pointer, theme);
        if let Some(rect) = ui_fullscreen::back_to_latest_rect(body)
            && let Some(region) = anchor_region(
                rect,
                body,
                PointerTarget::Agent(AgentTarget::BackToLatest),
                MouseButton::Left,
                true,
                z::AGENT_OVERLAY,
                VisualAnchor::non_empty(Position::new(rect.x, rect.y)),
            )
        {
            hit_map.push(region);
        }
    }
    if let Some(session) = app.active_session() {
        let loading = session.loading_older;
        let has_more = session.timeline.has_more;
        let truncated = session.timeline.truncated_before;
        if loading || has_more || truncated {
            let label = if loading {
                " ⋯ 正在加载更早消息… "
            } else if has_more {
                " ↑ 加载更早消息 "
            } else {
                " ↑ 更早消息不可用 "
            };
            let enabled = has_more && !loading;
            ui_fullscreen::draw_load_older(frame, body, view.pointer, theme, label, enabled);
            if enabled
                && let Some(rect) = ui_fullscreen::load_older_rect(body)
                && let Some(region) = anchor_region(
                    rect,
                    body,
                    PointerTarget::Agent(AgentTarget::LoadOlder),
                    MouseButton::Left,
                    true,
                    z::AGENT_OVERLAY,
                    VisualAnchor::non_empty(Position::new(rect.x, rect.y)),
                )
            {
                hit_map.push(region);
            }
        }
    }
    if let Some(menu) = view.menu.as_ref() {
        ui_fullscreen::draw_message_menu(frame, area, menu, theme);
        register_agent_menu(hit_map, area, menu);
    }
}

/// 把 transcript 里当前可见的消息行登记成 HitRegion。
///
/// 行窗口就是 `render_fullscreen_agent` 写回 `view.visible_start` 的那一份；
/// 行宽避开最右侧的滚动条列，所以 P2 接滚动条时不会和消息行抢同一列。
fn register_agent_messages(
    hit_map: &mut HitMapBuilder,
    body: Rect,
    view: &FullscreenView,
    session_id: &str,
) {
    if body.is_empty() || view.body_height == 0 {
        return;
    }
    let Some(cache) = view.transcripts.entries.get(session_id) else {
        return;
    };
    let clip = Rect::new(
        body.x,
        body.y,
        body.width.saturating_sub(1).max(1),
        body.height,
    );
    let visible_end = view
        .visible_start
        .saturating_add(usize::from(view.body_height));
    for span in &cache.spans {
        let start = span.start.max(view.visible_start);
        let end = span.end.min(visible_end);
        if start >= end {
            continue;
        }
        let Some(line) = cache.lines.get(start) else {
            continue;
        };
        let target = match span.kind {
            SpanKind::Message(role) => PointerTarget::Agent(AgentTarget::Message {
                turn_id: span.turn_id.clone(),
                block_id: span.block_id.clone(),
                role,
            }),
            SpanKind::Thinking => PointerTarget::Agent(AgentTarget::Thinking {
                turn_id: span.turn_id.clone(),
                block_id: span.block_id.clone(),
            }),
            SpanKind::Tool => PointerTarget::Agent(AgentTarget::Tool {
                turn_id: span.turn_id.clone(),
                block_id: span.block_id.clone(),
            }),
        };
        let rect = Rect::new(
            clip.x,
            clip.y.saturating_add((start - view.visible_start) as u16),
            clip.width,
            (end - start) as u16,
        );
        if let Some(region) = line_region(
            rect,
            clip,
            target,
            MouseButton::Left,
            true,
            z::AGENT_MESSAGE,
            line,
        ) {
            hit_map.push(region);
        }
    }
}

/// 登记消息菜单：外框是阻断层，每一行是一个独立目标（disabled 行仍登记但
/// `enabled = false`，`resolve` 会跳过它们，点击落到外框上）。
fn register_agent_menu(hit_map: &mut HitMapBuilder, area: Rect, menu: &MessageMenu) {
    let rect = ui_fullscreen::message_menu_rect(area, menu);
    if let Some(region) = anchor_region(
        rect,
        area,
        PointerTarget::Agent(AgentTarget::MenuRoot),
        MouseButton::Left,
        true,
        z::AGENT_MENU,
        VisualAnchor::non_empty(Position::new(rect.x, rect.y)),
    ) {
        hit_map.push(region);
    }
    for (index, action) in menu.actions().iter().enumerate() {
        let Some(row) = ui_fullscreen::message_menu_row_rect(area, menu, index) else {
            continue;
        };
        // 行内布局是 `" {marker} {glyph} "`，glyph 固定在行首 +3 列。
        let anchor = VisualAnchor::non_empty(Position::new(
            row.x.saturating_add(3).min(row.right().saturating_sub(1)),
            row.y,
        ));
        if let Some(region) = anchor_region(
            row,
            area,
            PointerTarget::Agent(AgentTarget::MenuAction(*action)),
            MouseButton::Left,
            action.enabled(),
            z::AGENT_MENU_ROW,
            anchor,
        ) {
            hit_map.push(region);
        }
    }
}
/// 全屏 shell：上半屏是 App 自己持有的 transcript 视口，下半屏是 slash 菜单、
/// 单行思考链、composer、status 与 shortcuts。
fn render_fullscreen_agent(
    app: &App,
    width: u16,
    height: u16,
    theme: &Theme,
    view: &mut FullscreenView,
) -> AgentRender {
    if app.active_session().is_none() {
        view.transcripts.clear();
        view.body_area = Rect::new(0, 0, width, height);
        view.visible_start = 0;
        view.body_height = height;
        view.close_menu();
        return render_brand(app, width, height, theme);
    }

    let area = Rect::new(0, 0, width, height.max(1));
    // 面板要占位，所以数据源必须在算布局之前刷新。
    view.sync_todo(app);
    let (body_area, bottom_area) = fullscreen_layout(app, area, theme, view.current_todo.as_ref());
    view.body_area = body_area;
    view.body_height = body_area.height;
    // 右侧固定留一列给滚动条，避免内容宽度在“出现/消失滚动条”时抖动。
    let history_width = body_area.width.saturating_sub(1).max(1);
    let (mut lines, visible_start) = render_fullscreen_history(
        app,
        history_width,
        body_area.height,
        theme,
        &mut view.transcripts,
    );
    view.visible_start = visible_start;
    while lines.len() < usize::from(body_area.height) {
        lines.push(Line::default());
    }
    lines.truncate(usize::from(body_area.height));

    let bottom = render_fullscreen_chrome(
        app,
        width,
        bottom_area.height,
        theme,
        view.current_todo.as_ref(),
    );
    // 预览条的行号是**块内相对**的，换算到整帧时加上历史区高度。
    let subagent_strip = bottom.subagent_strip.map(|strip| Rect {
        x: strip.x,
        y: body_area.height.saturating_add(strip.y),
        width: strip.width,
        height: 1,
    });
    lines.extend(bottom.lines);
    lines.truncate(usize::from(area.height));

    let cursor = bottom.cursor.map(|cursor| {
        Position::new(
            cursor.x,
            body_area
                .height
                .saturating_add(cursor.y)
                .min(area.height.saturating_sub(1)),
        )
    });
    AgentRender {
        lines,
        cursor,
        // 被裁掉（终端太矮）时不登记命中：画都没画出来就不该可点。
        subagent_strip: subagent_strip.filter(|strip| strip.y < area.height),
    }
}

fn fullscreen_layout(
    app: &App,
    area: Rect,
    theme: &Theme,
    todo: Option<&TodoBlock>,
) -> (Rect, Rect) {
    if area.height == 0 {
        return (area, Rect::new(area.x, area.y, area.width, 0));
    }

    let reserve_body = u16::from(area.height > 1);
    let max_bottom = area.height.saturating_sub(reserve_body).max(1);
    let desired_bottom =
        u16::try_from(fullscreen_chrome_layout(app, area.width, max_bottom, theme, todo).height())
            .unwrap_or(u16::MAX);
    let bottom_height = desired_bottom.clamp(1, max_bottom);
    let body_height = area.height.saturating_sub(bottom_height);

    (
        Rect::new(area.x, area.y, area.width, body_height),
        Rect::new(
            area.x,
            area.y.saturating_add(body_height),
            area.width,
            bottom_height,
        ),
    )
}

fn fullscreen_chrome_layout(
    app: &App,
    width: u16,
    available: u16,
    theme: &Theme,
    todo: Option<&TodoBlock>,
) -> AgentLayout {
    let available = usize::from(available.max(1));
    let Some(session) = app.active_session() else {
        return AgentLayout {
            live_rows: 0,
            slash_rows: 0,
            stream_rows: 0,
            thinking_rows: 0,
            composer_rows: 0,
            status_rows: 0,
            subagent_rows: 0,
            todo_rows: 0,
        };
    };

    let narrow = width < NARROW_VIEWPORT_WIDTH;
    // composer 输入带（通栏底色块）的最低高度：上下各一行留白夹住输入行，
    // 与消息区分界。窄屏同样生效；空间不够时由下方收缩循环压回 1 行。
    let min_composer = usize::from(theme.spacing.composer_min_height.max(1));
    let max_composer = usize::from(theme.spacing.composer_max_height.max(1)).max(min_composer);
    let preferred_composer = composer_visual_rows(session, width, theme)
        .clamp(min_composer.min(available), max_composer.min(available));
    let mut layout = AgentLayout {
        live_rows: 0,
        slash_rows: slash_menu_rows(app),
        stream_rows: 0,
        thinking_rows: usize::from(session_is_working(session)),
        composer_rows: preferred_composer,
        status_rows: usize::from(theme.spacing.status_height.max(1)),
        // 子代理预览条：**只在有子代理在跑时**占一行（done 就消失，数据源是 roster
        // 的 Running 状态），窄屏不挤——Ctrl+↑ 仍然可用。
        subagent_rows: usize::from(
            !narrow
                && usize::from(width) >= SUBAGENT_STRIP_MIN_WIDTH
                && !app.running_child_agent_ids().is_empty(),
        ),
        // 行数由面板自己算（标题 + 放得下的条目），上限 `TODO_PANEL_MAX_ROWS`。
        todo_rows: todo.map_or(0, |todo| {
            compose_todo_panel(todo, usize::from(width), TODO_PANEL_MAX_ROWS, theme).len()
        }),
    };

    // 收缩顺序 = 舍弃顺序：斜杠菜单（用户正打字，按 Esc 就没了）→ 子代理预览条
    // （Ctrl+↑ 等价）→ 待办面板（F4 里有完整清单）→ composer 留白 → 状态行 →
    // 思考行。
    shrink_chrome_layout(&mut layout, available);
    layout
}

/// 空间不足时按**舍弃优先级**逐行压缩（见 [`fullscreen_chrome_layout`]）。
///
/// 单独成函数是为了能脱离 `App` 直接测：这套顺序是版面契约，不是实现细节。
fn shrink_chrome_layout(layout: &mut AgentLayout, available: usize) {
    while layout.height() > available {
        if layout.slash_rows > 0 {
            layout.slash_rows -= 1;
        } else if layout.subagent_rows > 0 {
            layout.subagent_rows = 0;
        } else if layout.todo_rows > 0 {
            layout.todo_rows = 0;
        } else if layout.composer_rows > 1 {
            layout.composer_rows -= 1;
        } else if layout.status_rows > 0 {
            layout.status_rows = 0;
        } else if layout.thinking_rows > 0 {
            layout.thinking_rows = 0;
        } else {
            break;
        }
    }
}

fn render_fullscreen_chrome(
    app: &App,
    width: u16,
    height: u16,
    theme: &Theme,
    todo: Option<&TodoBlock>,
) -> AgentRender {
    let Some(session) = app.active_session() else {
        return render_brand(app, width, height, theme);
    };
    let height = usize::from(height.max(1));
    let layout = fullscreen_chrome_layout(
        app,
        width,
        u16::try_from(height).unwrap_or(u16::MAX),
        theme,
        todo,
    );
    let mut lines = slash_menu_lines(app, width, theme, layout.slash_rows);
    if layout.thinking_rows > 0 {
        lines.push(thinking_line(session, width, theme));
    }
    if layout.todo_rows > 0
        && let Some(todo) = todo
    {
        // 贴在输入带上沿：进行中的那项在第一行，扫一眼就知道现在在干什么。
        lines.extend(compose_todo_panel(
            todo,
            usize::from(width),
            layout.todo_rows,
            theme,
        ));
    }
    let composer_start = lines.len();
    let composer = composer_lines(
        &session.composer.input,
        session.composer.cursor,
        width,
        theme,
        layout.composer_rows,
    );
    // 输入带：composer 区整体铺 `chrome.composer_bg` 通栏底色，行尾由
    // `band_line` 补齐（Paragraph 不会为短行补格）；输入行不足最低高度时
    // 上下补白居中，让消息区与输入区形成一条明确的土金色分界。
    let band_pad_top = layout.composer_rows.saturating_sub(composer.lines.len()) / 2;
    for _ in 0..band_pad_top {
        lines.push(band_line(Line::default(), width, theme));
    }
    lines.extend(
        composer
            .lines
            .into_iter()
            .map(|line| band_line(line, width, theme)),
    );
    while lines.len() < composer_start.saturating_add(layout.composer_rows) {
        lines.push(band_line(Line::default(), width, theme));
    }
    if layout.status_rows > 0 {
        lines.push(status_line(app, width, theme));
    }
    // 子代理预览条（只有跑着的子代理才占这一行）。
    let mut subagent_strip = None;
    if layout.subagent_rows > 0
        && let Some(line) = subagent_strip_line(app, width, theme)
    {
        subagent_strip = Some(Rect::new(
            SUBAGENT_STRIP_INDENT as u16,
            u16::try_from(lines.len()).unwrap_or(u16::MAX),
            subagent_strip_width(),
            1,
        ));
        lines.push(line);
    }
    lines.truncate(height);

    let cursor_y = composer_start
        .saturating_add(band_pad_top)
        .saturating_add(composer.cursor_row)
        .min(height.saturating_sub(1)) as u16;
    AgentRender {
        lines,
        cursor: Some(Position::new(composer.cursor_x, cursor_y)),
        subagent_strip,
    }
}

/// 输入带的一行：已有 span 全部叠上 `chrome.composer_bg` 底色，并把行尾
/// 补空格铺满整行宽度，让 composer 区在画面上成为一条连续的土金色带。
fn band_line(line: Line<'static>, width: u16, theme: &Theme) -> Line<'static> {
    let bg = theme.chrome.composer_bg;
    let mut spans = line.spans;
    for span in &mut spans {
        span.style = span.style.bg(bg);
    }
    let filled: usize = spans.iter().map(|span| span.content.width()).sum();
    let width = usize::from(width);
    if width > filled {
        spans.push(Span::styled(
            " ".repeat(width - filled),
            Style::new().bg(bg),
        ));
    }
    Line::from(spans)
}

#[derive(Debug, Default)]
pub(super) struct FullscreenView {
    /// Single source of truth for pointer transitions.
    pub(super) pointer_state: super::pointer::PointerState,
    /// Rendering mirror derived from `pointer_state` after every event batch.
    pub(super) pointer: FullscreenState,
    /// 每会话独立的 transcript 渲染缓存（切 tab 零重渲染）。
    pub(super) transcripts: TranscriptCaches,
    /// 侧栏余晖动画状态。
    pub(super) sidebar_anim: sidebar::SidebarAnim,
    pub(super) body_area: Rect,
    pub(super) visible_start: usize,
    pub(super) body_height: u16,
    pub(super) menu: Option<MessageMenu>,
    /// 当前生效的待办清单（最后一次成功 `todo_write` 的入参）。
    ///
    /// 由 [`FullscreenView::sync_todo`] 按 `(session_id, timeline.version)` 缓存：
    /// 清单要参与**布局**（面板占几行），所以必须在本帧渲染前拿到，不能等
    /// transcript 缓存同步时再顺手算。
    pub(super) current_todo: Option<TodoBlock>,
    pub(super) current_todo_key: Option<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MessageHit {
    pub(super) turn_id: String,
    pub(super) block_id: String,
    pub(super) role: MessageRole,
}

impl FullscreenView {
    pub(super) fn open_menu(&mut self, hit: MessageHit, column: u16, row: u16) {
        self.menu = Some(MessageMenu::new(
            hit.turn_id,
            hit.block_id,
            hit.role,
            Position::new(column, row),
        ));
    }

    pub(super) fn close_menu(&mut self) {
        self.menu = None;
    }

    /// 刷新 sticky 待办面板的数据源。
    ///
    /// 倒扫整条 timeline 是 O(块数)，不能每帧做；按 `(session_id, timeline.version)`
    /// 缓存，版本没动就直接复用。切会话时 key 必然不同，无需显式失效。
    pub(super) fn sync_todo(&mut self, app: &App) {
        let Some(session) = app.active_session() else {
            self.current_todo = None;
            self.current_todo_key = None;
            return;
        };
        let key = (session.session_id.clone(), session.timeline.version);
        if self.current_todo_key.as_ref() == Some(&key) {
            return;
        }
        self.current_todo = adapter::current_todo(&session.timeline);
        self.current_todo_key = Some(key);
    }
}

impl FullscreenView {
    fn can_scroll(&self, app: &App) -> bool {
        app.active_session_id()
            .is_some_and(|id| self.transcripts.len_for(&id) > usize::from(self.body_height))
    }

    pub(super) fn max_offset(&self, app: &App) -> usize {
        app.active_session_id().map_or(0, |id| {
            self.transcripts
                .len_for(&id)
                .saturating_sub(usize::from(self.body_height))
        })
    }

    pub(super) fn scroll_up(&mut self, app: &mut App, lines: usize) {
        if !self.can_scroll(app) {
            app.scroll_bottom();
            return;
        }
        app.scroll_up(lines);
        self.clamp_scroll(app);
    }

    pub(super) fn scroll_down(&mut self, app: &mut App, lines: usize) {
        app.scroll_down(lines);
        self.clamp_scroll(app);
    }

    pub(super) fn page_up(&mut self, app: &mut App) {
        let at_limit = app
            .active_session()
            .is_some_and(|session| session.scroll.offset >= self.max_offset(app));
        let (has_more, loading) = app.active_session().map_or((false, false), |session| {
            (session.timeline.has_more, session.loading_older)
        });
        self.scroll_up(app, 20);
        if at_limit && has_more && !loading {
            app.load_older();
        }
    }

    pub(super) fn clamp_scroll(&mut self, app: &mut App) {
        let max_offset = self.max_offset(app);
        let Some(session_id) = app.active_session_id() else {
            return;
        };
        if let Some(session) = app.sessions.get_mut(&session_id) {
            if max_offset == 0 {
                session.scroll.follow = true;
                session.scroll.offset = 0;
            } else if session.scroll.follow {
                session.scroll.offset = 0;
            } else {
                session.scroll.offset = session.scroll.offset.min(max_offset);
            }
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct FullscreenTranscriptCache {
    key: Option<FullscreenTranscriptKey>,
    blocks: HashMap<FullscreenBlockKey, Vec<Line<'static>>>,
    spans: Vec<FullscreenBlockSpan>,
    lines: Vec<Line<'static>>,
    #[cfg(test)]
    pub(super) render_misses: usize,
}

#[derive(Debug, Clone, Copy)]
enum SpanKind {
    Message(MessageRole),
    Thinking,
    Tool,
}

#[derive(Debug, Clone)]
struct FullscreenBlockSpan {
    turn_id: String,
    block_id: String,
    kind: SpanKind,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FullscreenTranscriptKey {
    session_id: String,
    version: u64,
    width: u16,
    expanded_tools_revision: u64,
    expanded_thinking_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FullscreenBlockKey {
    turn_id: String,
    block_id: String,
    revision: u64,
    state: BlockState,
    width: u16,
    /// 展开标志（tool/thinking 专属；其余块恒 false）。翻转必须 miss：
    /// 展开不改 rev，键里没有它的话逐块缓存永远命中折叠渲染。
    expanded: bool,
    content_hash: u64,
}

impl FullscreenTranscriptCache {
    #[cfg(test)]
    pub(super) fn rendered_lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// 工具类命中区域（`block_id`, start, end）——测试用，不暴露内部结构。
    #[cfg(test)]
    pub(super) fn tool_spans(&self) -> Vec<(String, usize, usize)> {
        self.spans
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Tool))
            .map(|span| (span.block_id.clone(), span.start, span.end))
            .collect()
    }

    pub(super) fn sync(&mut self, session: &SessionState, width: u16, theme: &Theme) {
        let key = FullscreenTranscriptKey {
            session_id: session.session_id.clone(),
            version: session.timeline.version,
            width,
            expanded_tools_revision: session.expanded_tools_revision,
            expanded_thinking_revision: session.expanded_thinking_revision,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }

        // Live reasoning 仍在 composer 上方单独显示，避免“单行思考链”在历史区
        // 重复；其余 live block（尤其流式 assistant）必须进入全屏历史，否则全屏
        // 模式下只能看到最后一行。
        // 走**整个模型**而不是只有 `turns`：压缩分隔锚（W3/D10）与回合墙钟都在
        // 模型上，只读 turns 会把它们静默丢掉。
        let blocks: Vec<_> = adapter::from_model_with_expanded_blocks(
            &session.timeline,
            &session.expanded_tools,
            &session.expanded_thinking,
        )
        .into_iter()
        .filter(|block| {
            !(block.state == BlockState::Live && matches!(block.kind, BlockKind::Thinking { .. }))
        })
        .collect();

        let mut used = HashSet::with_capacity(blocks.len());
        let mut spans = Vec::with_capacity(blocks.len());
        let mut lines = Vec::new();
        let mut index = 0;
        while index < blocks.len() {
            if index > 0 {
                lines.push(Line::default());
            }
            let start = lines.len();
            // 连续查询族合并成一张卡（与 `render_transcript` 同一条规则，
            // 免得子代理视图和全屏视图长得不一样）。
            let group = crate::ui::v2::transcript::lookup_group_len(&blocks[index..]);
            let (span, rendered) = if group >= 2 {
                let members = &blocks[index..index + group];
                let block_key = FullscreenBlockKey::from_group(members, width);
                used.insert(block_key.clone());
                let rendered = self.blocks.entry(block_key).or_insert_with(|| {
                    #[cfg(test)]
                    {
                        self.render_misses = self.render_misses.saturating_add(1);
                    }
                    crate::ui::v2::transcript::render_lookup_group(
                        members,
                        usize::from(width),
                        theme,
                    )
                });
                // 合并卡只有一个命中区域，指向**首成员**：点它即展开，而展开态会
                // 让 `lookup_group_len` 归零——卡片自己拆回 N 张独立卡，正好是
                // 「给我看每一次调用」。
                let head = &members[0];
                index += group;
                (
                    Some(FullscreenBlockSpan {
                        turn_id: head.turn_id.clone(),
                        block_id: head.id.to_string(),
                        kind: SpanKind::Tool,
                        start,
                        end: start + rendered.len(),
                    }),
                    rendered,
                )
            } else {
                let block = &blocks[index];
                let block_key = FullscreenBlockKey::from_block(block, width);
                used.insert(block_key.clone());
                let rendered = self.blocks.entry(block_key).or_insert_with(|| {
                    #[cfg(test)]
                    {
                        self.render_misses = self.render_misses.saturating_add(1);
                    }
                    crate::ui::v2::transcript::render_block(block, usize::from(width), theme)
                });
                let kind = match &block.kind {
                    BlockKind::User { .. } => Some(SpanKind::Message(MessageRole::User)),
                    BlockKind::Assistant { .. } => Some(SpanKind::Message(MessageRole::Assistant)),
                    BlockKind::Thinking { .. } => Some(SpanKind::Thinking),
                    BlockKind::Tool(_) => Some(SpanKind::Tool),
                    _ => None,
                };
                let span = kind.map(|kind| FullscreenBlockSpan {
                    turn_id: block.turn_id.clone(),
                    block_id: block.id.to_string(),
                    kind,
                    start,
                    end: start + rendered.len(),
                });
                index += 1;
                (span, rendered)
            };
            lines.extend(rendered.iter().cloned());
            if let Some(span) = span {
                spans.push(span);
            }
        }
        self.blocks.retain(|key, _| used.contains(key));
        self.spans = spans;
        self.lines = lines;
        self.key = Some(key);
    }
}

impl FullscreenBlockKey {
    fn from_block(block: &TranscriptBlock, width: u16) -> Self {
        // 展开标志必须在键里（点击展开 bug 的根因）：`ceaf7ae` 把键改成
        // (block_id, revision) 后，展开翻转不动 rev——逐块缓存全部命中旧的
        // 折叠渲染，外层重建空转，表现为「点击后闪一下但没展开」。
        let expanded = match &block.kind {
            BlockKind::Tool(tool) => tool.expanded,
            BlockKind::Thinking { expanded, .. } => *expanded,
            _ => false,
        };
        Self {
            turn_id: block.turn_id.clone(),
            block_id: block.id.to_string(),
            revision: block.revision,
            state: block.state,
            width,
            expanded,
            // 内容身份由 (block_id, revision) 承担：rev 是「可见内容可能变化
            // 即自增」的权威计数（timeline_model::Block::touch），缓存键不再
            // 哈希块内容——那曾要求 BlockKind/ToolBlock derive Hash。
            content_hash: 0,
        }
    }

    /// 合并卡的缓存键：没有单一 block，就把**每个成员**的 id 与内容都揉进去，
    /// 任何一个成员变了都会 miss 重渲（缓存命中率不受影响——同一次同步里
    /// 逐帧比对的是同一组成员）。
    fn from_group(members: &[TranscriptBlock], width: u16) -> Self {
        let mut hasher = DefaultHasher::new();
        let mut revision = 0u64;
        let mut ids = Vec::with_capacity(members.len());
        for block in members {
            // 逐成员 revision（而不是 max）：max 不动的成员更新也要 miss。
            block.revision.hash(&mut hasher);
            block.state.hash(&mut hasher);
            revision = revision.max(block.revision);
            ids.push(block.id.to_string());
        }
        // 合并卡只在全员折叠时存在（`groupable` 要求 `!expanded`），展开态下
        // 成员拆回独立卡、各自走 `from_block`——这里恒为 false 即可，但显式
        // 写出来让键的语义完整。
        Self {
            turn_id: members
                .first()
                .map(|block| block.turn_id.clone())
                .unwrap_or_default(),
            block_id: ids.join("\u{1}"),
            revision,
            state: members
                .first()
                .map(|block| block.state)
                .unwrap_or(BlockState::Sealed),
            width,
            expanded: false,
            content_hash: hasher.finish(),
        }
    }
}

fn render_fullscreen_history(
    app: &App,
    width: u16,
    height: u16,
    theme: &Theme,
    caches: &mut TranscriptCaches,
) -> (Vec<Line<'static>>, usize) {
    let Some(session) = app.active_session() else {
        caches.clear();
        return (Vec::new(), 0);
    };
    let height = usize::from(height);
    if height == 0 {
        return (Vec::new(), 0);
    }

    let cache = caches.touch(&session.session_id);
    cache.sync(session, width, theme);
    let total = cache.lines.len();
    let top = crate::ui::viewport_top(total, height, session.scroll.follow, session.scroll.offset);
    let end = top.saturating_add(height).min(total);
    (cache.lines[top.min(total)..end].to_vec(), top)
}

/// 每会话独立的 transcript 渲染缓存（秒切的核心）。
///
/// 单例缓存时代：一切 tab，`retain` 把旧会话的全部已渲染块逐出，
/// 切回要重走 markdown + syntect 高亮整个会话——这就是切换卡顿的根源。
/// 现在切走只挪 LRU 顺位，切回直接命中已渲染行，零重渲染。
#[derive(Debug, Default)]
pub(super) struct TranscriptCaches {
    entries: HashMap<String, FullscreenTranscriptCache>,
    /// LRU 顺位；尾部 = 最近使用。
    lru: Vec<String>,
}

impl TranscriptCaches {
    /// active + 最近切走的 3 个。
    const CAP: usize = 4;

    /// 取该会话的缓存并刷新 LRU 顺位；超出容量时逐出最久未用的。
    pub(super) fn touch(&mut self, session_id: &str) -> &mut FullscreenTranscriptCache {
        if let Some(pos) = self.lru.iter().position(|id| id == session_id) {
            let id = self.lru.remove(pos);
            self.lru.push(id);
        } else {
            self.lru.push(session_id.to_string());
            self.entries
                .insert(session_id.to_string(), FullscreenTranscriptCache::default());
            while self.lru.len() > Self::CAP {
                let evicted = self.lru.remove(0);
                self.entries.remove(&evicted);
            }
        }
        self.entries
            .get_mut(session_id)
            .expect("entry inserted above")
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    pub(super) fn len_for(&self, session_id: &str) -> usize {
        self.entries
            .get(session_id)
            .map(|cache| cache.lines.len())
            .unwrap_or(0)
    }

    /// 测试用：某会话已渲染行的纯文本视图（点击展开的端到端断言用）。
    #[cfg(test)]
    pub(super) fn lines_for_test(&self, session_id: &str) -> &[Line<'static>] {
        self.entries
            .get(session_id)
            .map(|cache| cache.rendered_lines())
            .unwrap_or(&[])
    }

    /// 测试用：某会话缓存的累计重建次数。
    #[cfg(test)]
    pub(super) fn render_misses_for_test(&self, session_id: &str) -> usize {
        self.entries
            .get(session_id)
            .map(|cache| cache.render_misses)
            .unwrap_or(0)
    }
}

/// 普通启动的品牌首屏：品牌标识 + 输入框 + 一行状态提示。
///
/// 这里不预造 session；`Enter` 由 app 层转成 `SessionCreate`，首条消息在 session_id
/// 确认后补发。
fn render_brand(app: &App, width: u16, height: u16, theme: &Theme) -> AgentRender {
    let height = usize::from(height.max(1));
    let width = usize::from(width.max(1));
    let mut lines = brand_lines(width, theme);

    let box_width = width;
    let inner_width = box_width.saturating_sub(4).max(1);
    let composer = composer_lines(
        &app.draft_composer.input,
        app.draft_composer.cursor,
        u16::try_from(inner_width).unwrap_or(u16::MAX),
        theme,
        3,
    );
    let border_style = Style::new().fg(theme.chrome.border);
    let composer_start = lines.len();
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(box_width.saturating_sub(2))),
        border_style,
    )));
    for line in composer.lines {
        let mut spans = Vec::with_capacity(line.spans.len() + 2);
        spans.push(Span::styled("│ ".to_string(), border_style));
        spans.extend(line.spans);
        spans.push(Span::styled(" │".to_string(), border_style));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(box_width.saturating_sub(2))),
        border_style,
    )));
    lines.push(Line::default());

    let hint = if !app.pending_creates.is_empty() {
        " 正在创建会话…"
    } else {
        " Enter 创建会话并带入输入框 · Alt+Enter 换行 · Ctrl+L 会话 · F1 帮助 · Ctrl+Q 退出"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::new().fg(theme.text.dim),
    )));
    lines.truncate(height);

    let cursor_y = composer_start
        .saturating_add(1)
        .saturating_add(composer.cursor_row);
    let cursor_x = 2u16.saturating_add(composer.cursor_x);
    let cursor = (cursor_y < height && usize::from(cursor_x) < width)
        .then_some(Position::new(cursor_x, cursor_y as u16));
    AgentRender {
        lines,
        cursor,
        subagent_strip: None,
    }
}

fn brand_lines(width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let accent = Style::new()
        .fg(theme.accent.assistant)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let muted = Style::new().fg(theme.text.dim);
    if width < usize::from(NARROW_VIEWPORT_WIDTH) {
        return vec![
            Line::from(Span::styled("  QAQH", accent)),
            Line::from(Span::styled("  QAQ-Harness Terminal", muted)),
            Line::default(),
        ];
    }

    const ART: [&str; 6] = [
        "  ██████╗  █████╗  ██████╗ ██╗  ██╗",
        " ██╔═══██╗██╔══██╗██╔═══██╗██║  ██║",
        " ██║   ██║███████║██║   ██║███████║",
        " ██║   ██║██╔══██║██║   ██║██╔══██║",
        " ╚██████╔╝██║  ██║╚██████╔╝██║  ██║",
        "  ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝",
    ];
    let mut lines: Vec<Line<'static>> = ART
        .into_iter()
        .map(|text| centered_line(text, width, accent))
        .collect();
    lines.push(centered_line(
        "Q A Q - H A R N E S S   ·   T E R M I N A L",
        width,
        muted,
    ));
    lines.push(Line::default());
    lines
}

fn centered_line(text: &str, width: usize, style: Style) -> Line<'static> {
    let padding = width.saturating_sub(text.width()) / 2;
    Line::from(Span::styled(
        format!("{}{}", " ".repeat(padding), text),
        style,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> AgentLayout {
        AgentLayout {
            live_rows: 0,
            slash_rows: 3,
            stream_rows: 0,
            thinking_rows: 1,
            composer_rows: 3,
            status_rows: 1,
            subagent_rows: 1,
            todo_rows: 4,
        }
    }

    /// 空间不足时先丢**顺带看**的（斜杠菜单 → 子代理预览条 → 待办面板），
    /// 最后才动 composer / 状态行 / 思考行——输入带和「在跑什么」是底线。
    #[test]
    fn chrome_shrinks_optional_rows_before_the_composer() {
        let mut roomy = layout();
        let room = roomy.height();
        shrink_chrome_layout(&mut roomy, room);
        assert_eq!(roomy.todo_rows, 4, "够放就不动");
        assert_eq!(roomy, layout());

        // 少一行：只丢斜杠菜单，待办面板毫发无损。
        let mut tight = layout();
        let room = tight.height() - 1;
        shrink_chrome_layout(&mut tight, room);
        assert_eq!((tight.slash_rows, tight.todo_rows), (2, 4));
        assert_eq!(tight.height(), 12);

        // 只留得下 composer + 状态行 + 思考行：面板、子代理预览条、斜杠菜单全让路。
        let mut squeezed = layout();
        shrink_chrome_layout(&mut squeezed, 5);
        assert_eq!(squeezed.height(), 5);
        assert_eq!(
            (
                squeezed.slash_rows,
                squeezed.subagent_rows,
                squeezed.todo_rows
            ),
            (0, 0, 0)
        );
        assert_eq!((squeezed.composer_rows, squeezed.status_rows), (3, 1));
        assert_eq!(squeezed.thinking_rows, 1);
    }

    /// 极限压缩：composer 保底 1 行，再不够就依次丢状态行、思考行。
    #[test]
    fn chrome_never_shrinks_the_composer_below_one_row() {
        let mut squeezed = layout();
        shrink_chrome_layout(&mut squeezed, 2);
        assert_eq!(squeezed.composer_rows, 1);
        assert_eq!(squeezed.status_rows, 0);
        assert_eq!(squeezed.thinking_rows, 1);

        // 比底线还窄也不 panic（死循环 / 下溢都会在这里炸）。
        let mut impossible = layout();
        shrink_chrome_layout(&mut impossible, 0);
        assert_eq!(impossible.height(), 1);
        assert_eq!(impossible.composer_rows, 1);
    }
}
