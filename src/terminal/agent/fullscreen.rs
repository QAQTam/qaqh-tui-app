//! V2 fullscreen agent shell.
//!
//! Owns fullscreen view state, transcript cache, hit-testing, message menus and
//! fullscreen rendering. Shared composer/status helpers remain in the parent
//! `agent` module.

use super::*;
use crate::ui::v2::fullscreen as ui_fullscreen;
use crate::ui::v2::fullscreen::{FullscreenState, MessageAction, MessageMenu, MessageRole};
use crate::ui::v2::hit::{
    AgentTarget, HitMapBuilder, PointerTarget, ScrollbarPart, VisualAnchor, anchor_region,
    line_region, z,
};
use crate::ui::v2::scrollbar::ScrollbarMetrics;
use ratatui::crossterm::event::MouseButton;

const NARROW_VIEWPORT_WIDTH: u16 = 40;

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
    let area = frame.area();
    let rendered = render_fullscreen_agent(app, area.width, area.height, theme, view);
    frame.render_widget(Paragraph::new(rendered.lines), area);
    if let Some(cursor) = rendered.cursor {
        frame.set_cursor_position((
            area.x.saturating_add(cursor.x),
            area.y.saturating_add(cursor.y),
        ));
    }

    let (body, _) = fullscreen_layout(app, area, theme);
    if !body.is_empty()
        && let Some(session) = app.active_session()
    {
        ui_fullscreen::draw_scrollbar(
            frame,
            body,
            view.transcript.lines.len(),
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
            view.transcript.lines.len(),
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
    register_agent_messages(hit_map, body, view);
    let show_back_to_latest = view.can_scroll()
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
    if let Some(menu) = view.menu.as_ref() {
        ui_fullscreen::draw_message_menu(frame, area, menu, theme);
        register_agent_menu(hit_map, area, menu);
    }
}

/// 把 transcript 里当前可见的消息行登记成 HitRegion。
///
/// 行窗口就是 `render_fullscreen_agent` 写回 `view.visible_start` 的那一份；
/// 行宽避开最右侧的滚动条列，所以 P2 接滚动条时不会和消息行抢同一列。
fn register_agent_messages(hit_map: &mut HitMapBuilder, body: Rect, view: &FullscreenView) {
    if body.is_empty() || view.body_height == 0 {
        return;
    }
    let clip = Rect::new(
        body.x,
        body.y,
        body.width.saturating_sub(1).max(1),
        body.height,
    );
    let visible_end = view
        .visible_start
        .saturating_add(usize::from(view.body_height));
    for span in &view.transcript.spans {
        let start = span.start.max(view.visible_start);
        let end = span.end.min(visible_end);
        if start >= end {
            continue;
        }
        let Some(line) = view.transcript.lines.get(start) else {
            continue;
        };
        let target = match span.kind {
            SpanKind::Message(role) => PointerTarget::Agent(AgentTarget::Message {
                turn_id: span.turn_id.clone(),
                block_id: span.block_id.clone(),
                role,
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
        view.transcript.clear();
        view.body_area = Rect::new(0, 0, width, height);
        view.visible_start = 0;
        view.body_height = height;
        view.close_menu();
        return render_brand(app, width, height, theme);
    }

    let area = Rect::new(0, 0, width, height.max(1));
    let (body_area, bottom_area) = fullscreen_layout(app, area, theme);
    view.body_area = body_area;
    view.body_height = body_area.height;
    // 右侧固定留一列给滚动条，避免内容宽度在“出现/消失滚动条”时抖动。
    let history_width = body_area.width.saturating_sub(1).max(1);
    let (mut lines, visible_start) = render_fullscreen_history(
        app,
        history_width,
        body_area.height,
        theme,
        &mut view.transcript,
    );
    view.visible_start = visible_start;
    while lines.len() < usize::from(body_area.height) {
        lines.push(Line::default());
    }
    lines.truncate(usize::from(body_area.height));

    let bottom = render_fullscreen_chrome(app, width, bottom_area.height, theme);
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
    AgentRender { lines, cursor }
}

fn fullscreen_layout(app: &App, area: Rect, theme: &Theme) -> (Rect, Rect) {
    if area.height == 0 {
        return (area, Rect::new(area.x, area.y, area.width, 0));
    }

    let reserve_body = u16::from(area.height > 1);
    let max_bottom = area.height.saturating_sub(reserve_body).max(1);
    let desired_bottom =
        u16::try_from(fullscreen_chrome_layout(app, area.width, max_bottom, theme).height())
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

fn fullscreen_chrome_layout(app: &App, width: u16, available: u16, theme: &Theme) -> AgentLayout {
    let available = usize::from(available.max(1));
    let Some(session) = app.active_session() else {
        return AgentLayout {
            live_rows: 0,
            slash_rows: 0,
            stream_rows: 0,
            thinking_rows: 0,
            composer_rows: 0,
            status_rows: 0,
            shortcuts_rows: 0,
        };
    };

    let narrow = width < NARROW_VIEWPORT_WIDTH;
    let min_composer = if narrow {
        1
    } else {
        usize::from(theme.spacing.composer_min_height.max(1))
    };
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
        shortcuts_rows: if narrow {
            0
        } else {
            usize::from(theme.spacing.shortcuts_height.max(1))
        },
    };

    while layout.height() > available {
        if layout.slash_rows > 0 {
            layout.slash_rows -= 1;
        } else if layout.shortcuts_rows > 0 {
            layout.shortcuts_rows = 0;
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
    layout
}

fn render_fullscreen_chrome(app: &App, width: u16, height: u16, theme: &Theme) -> AgentRender {
    let Some(session) = app.active_session() else {
        return render_brand(app, width, height, theme);
    };
    let height = usize::from(height.max(1));
    let layout =
        fullscreen_chrome_layout(app, width, u16::try_from(height).unwrap_or(u16::MAX), theme);
    let mut lines = slash_menu_lines(app, width, theme, layout.slash_rows);
    if layout.thinking_rows > 0 {
        lines.push(thinking_line(session, width, theme));
    }
    let composer_start = lines.len();
    let composer = composer_lines(
        &session.composer.input,
        session.composer.cursor,
        width,
        theme,
        layout.composer_rows,
    );
    lines.extend(composer.lines);
    while lines.len() < composer_start.saturating_add(layout.composer_rows) {
        lines.push(Line::default());
    }
    if layout.status_rows > 0 {
        lines.push(status_line(app, width, theme));
    }
    if layout.shortcuts_rows > 0 {
        lines.push(shortcuts_line(app, width, theme));
    }
    lines.truncate(height);

    let cursor_y = composer_start
        .saturating_add(composer.cursor_row)
        .min(height.saturating_sub(1)) as u16;
    AgentRender {
        lines,
        cursor: Some(Position::new(composer.cursor_x, cursor_y)),
    }
}

#[derive(Debug, Default)]
pub(super) struct FullscreenView {
    /// Single source of truth for pointer transitions.
    pub(super) pointer_state: super::pointer::PointerState,
    /// Rendering mirror derived from `pointer_state` after every event batch.
    pub(super) pointer: FullscreenState,
    pub(super) transcript: FullscreenTranscriptCache,
    pub(super) body_area: Rect,
    pub(super) visible_start: usize,
    pub(super) body_height: u16,
    pub(super) menu: Option<MessageMenu>,
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
}

impl FullscreenView {
    fn can_scroll(&self) -> bool {
        self.transcript.lines.len() > usize::from(self.body_height)
    }

    pub(super) fn max_offset(&self) -> usize {
        self.transcript
            .lines
            .len()
            .saturating_sub(usize::from(self.body_height))
    }

    pub(super) fn scroll_up(&mut self, app: &mut App, lines: usize) {
        if !self.can_scroll() {
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
            .is_some_and(|session| session.scroll.offset >= self.max_offset());
        let (has_more, loading) = app.active_session().map_or((false, false), |session| {
            (session.timeline.has_more, session.loading_older)
        });
        self.scroll_up(app, 20);
        if at_limit && has_more && !loading {
            app.load_older();
        }
    }

    pub(super) fn clamp_scroll(&mut self, app: &mut App) {
        let max_offset = self.max_offset();
        let Some(seed) = app.active_seed() else {
            return;
        };
        if let Some(session) = app.sessions.get_mut(&seed) {
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
    seed: String,
    version: u64,
    width: u16,
    expanded_tools_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FullscreenBlockKey {
    turn_id: String,
    block_id: String,
    revision: u64,
    state: BlockState,
    width: u16,
    content_hash: u64,
}

impl FullscreenTranscriptCache {
    pub(super) fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn clear(&mut self) {
        self.key = None;
        self.blocks.clear();
        self.spans.clear();
        self.lines.clear();
    }

    pub(super) fn sync(&mut self, app: &App, width: u16, theme: &Theme) {
        let Some(session) = app.active_session() else {
            self.clear();
            return;
        };
        let key = FullscreenTranscriptKey {
            seed: session.seed.clone(),
            version: session.timeline.version,
            width,
            expanded_tools_revision: session.expanded_tools_revision,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }

        // Live reasoning 仍在 composer 上方单独显示，避免“单行思考链”在历史区
        // 重复；其余 live block（尤其流式 assistant）必须进入全屏历史，否则全屏
        // 模式下只能看到最后一行。
        let blocks: Vec<_> =
            adapter::from_turns_with_expanded(&session.timeline.turns, &session.expanded_tools)
                .into_iter()
                .filter(|block| {
                    !(block.state == BlockState::Live
                        && matches!(block.kind, BlockKind::Thinking { .. }))
                })
                .collect();

        let mut used = HashSet::with_capacity(blocks.len());
        let mut spans = Vec::with_capacity(blocks.len());
        let mut lines = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            let start = lines.len();
            let block_key = FullscreenBlockKey::from_block(block, width);
            used.insert(block_key.clone());
            let rendered = self.blocks.entry(block_key).or_insert_with(|| {
                #[cfg(test)]
                {
                    self.render_misses = self.render_misses.saturating_add(1);
                }
                crate::ui::v2::transcript::render_block(block, usize::from(width), theme)
            });
            lines.extend(rendered.iter().cloned());
            let kind = match &block.kind {
                BlockKind::User { .. } => SpanKind::Message(MessageRole::User),
                BlockKind::Assistant { .. } => SpanKind::Message(MessageRole::Assistant),
                BlockKind::Tool(_) => SpanKind::Tool,
                _ => continue,
            };
            spans.push(FullscreenBlockSpan {
                turn_id: block.turn_id.clone(),
                block_id: block.id.to_string(),
                kind,
                start,
                end: lines.len(),
            });
        }
        self.blocks.retain(|key, _| used.contains(key));
        self.spans = spans;
        self.lines = lines;
        self.key = Some(key);
    }
}

impl FullscreenBlockKey {
    fn from_block(block: &TranscriptBlock, width: u16) -> Self {
        let mut hasher = DefaultHasher::new();
        block.kind.hash(&mut hasher);
        Self {
            turn_id: block.turn_id.clone(),
            block_id: block.id.to_string(),
            revision: block.revision,
            state: block.state,
            width,
            content_hash: hasher.finish(),
        }
    }
}

fn render_fullscreen_history(
    app: &App,
    width: u16,
    height: u16,
    theme: &Theme,
    cache: &mut FullscreenTranscriptCache,
) -> (Vec<Line<'static>>, usize) {
    let Some(session) = app.active_session() else {
        cache.clear();
        return (Vec::new(), 0);
    };
    let height = usize::from(height);
    if height == 0 {
        return (Vec::new(), 0);
    }

    cache.sync(app, width, theme);
    let total = cache.lines.len();
    let top = crate::ui::viewport_top(total, height, session.scroll.follow, session.scroll.offset);
    let end = top.saturating_add(height).min(total);
    (cache.lines[top.min(total)..end].to_vec(), top)
}

/// 普通启动的品牌首屏：品牌标识 + 输入框 + 一行状态提示。
///
/// 这里不预造 session；`Enter` 由 app 层转成 `SessionCreate`，首条消息在 seed
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
    AgentRender { lines, cursor }
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
