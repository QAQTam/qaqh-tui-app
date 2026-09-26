//! V2 fullscreen Agent shell：事件循环、终端生命周期与输入分发。
//!
//! 当前只有一套生产 shell；App 自己持有 transcript 视口、滚动和鼠标命中状态。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyEventKind, MouseEvent, MouseEventKind, poll,
    read,
};
use ratatui::crossterm::{event::KeyCode, execute};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::session::SessionState;
use crate::app::{App, AppMsg, ConnPhase, ModalHit, StartupIntent, WorkspaceHit};
use crate::runtime::{Runtime, RuntimeMsg};
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::hit::{
    AgentTarget, FrameHitMap, FrameId, HitMapBuilder, HitProbe, PointerTarget, ProbeFailure,
    ScrollbarPart,
};
use crate::ui::v2::route::{self, ModalRoute, ScreenRoute};
use crate::ui::v2::scrollbar::ScrollbarMetrics;
use crate::ui::v2::transcript::{BlockKind, BlockState, TranscriptBlock};
use crate::ui::v2::workspace;
use qaqh_client::{ConversationMode, NoticeLevel, TimelineBlockKind};

mod fullscreen;
mod pointer;
use fullscreen::{
    FullscreenView, MessageHit, activate_message_action, draw_fullscreen_agent,
    handle_fullscreen_menu_key,
};
use pointer::{PointerAction, PointerEvent};

const TICK_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SLASH_ROWS: usize = 4;

/// 启动真实 V2 fullscreen Agent shell。
pub async fn run(no_spawn: bool, resume: bool) -> Result<()> {
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppMsg>();
    let (rt_tx, mut rt_rx) = mpsc::unbounded_channel::<RuntimeMsg>();
    {
        let bridge_tx = app_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rt_rx.recv().await {
                if bridge_tx.send(AppMsg::Runtime(msg)).is_err() {
                    break;
                }
            }
        });
    }

    let runtime = Runtime::start(rt_tx, !no_spawn)
        .await
        .context("连接 daemon 失败")?;

    let mut app = App::new(runtime.clone(), app_tx.clone());
    app.show_workspace = false;
    app.fetch_session_list();
    if resume {
        app.startup_intent = StartupIntent::Resume;
        app.session_cwd_filter = app.initial_cwd.clone().map(|cwd| {
            std::fs::canonicalize(&cwd)
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or(cwd)
        });
        app.open_session_list();
    }
    let theme = Theme::current();
    let mut terminal = match TerminalHost::init() {
        Ok(terminal) => terminal,
        Err(error) => {
            runtime.shutdown().await;
            return Err(error);
        }
    };

    let mut input = InputPump::new(app_tx.clone());
    spawn_tick(app_tx.clone());

    let mut fullscreen_view = FullscreenView::default();
    let result = run_loop(
        &mut terminal,
        &mut input,
        &mut app_rx,
        &mut app,
        &mut fullscreen_view,
        theme,
    )
    .await;

    input.suspend().await;
    terminal.disable_capture();
    runtime.shutdown().await;
    ratatui::restore();
    result
}

struct InputPump {
    tx: mpsc::UnboundedSender<AppMsg>,
    stop: Option<Arc<AtomicBool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl InputPump {
    fn new(tx: mpsc::UnboundedSender<AppMsg>) -> Self {
        let mut pump = Self {
            tx,
            stop: None,
            handle: None,
        };
        pump.resume();
        pump
    }

    fn resume(&mut self) {
        if self.handle.is_some() {
            return;
        }
        let tx = self.tx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        self.stop = Some(stop);
        self.handle = Some(thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match poll(Duration::from_millis(10)) {
                    Ok(true) => match read() {
                        Ok(event) => {
                            let msg = match event {
                                Event::Key(key) if key.kind == KeyEventKind::Press => {
                                    AppMsg::Key(key)
                                }
                                Event::Paste(text) => AppMsg::Paste(text),
                                Event::Resize(_, _) => AppMsg::Resize,
                                // Fullscreen shell captures mouse input globally.
                                Event::Mouse(mouse) => AppMsg::Mouse(mouse),
                                // 焦点丢失 = 合成 Leave：hover/pressed/capture 全部作废。
                                Event::FocusLost => AppMsg::FocusLost,
                                _ => continue,
                            };
                            if tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        }));
    }

    /// 暂停 crossterm 输入读取。
    ///
    /// 读取线程每次只 poll 10ms 后主动释放 crossterm 内部 event reader 锁；
    /// 终端重建时要读取 cursor
    /// position，若输入线程正阻塞在 `read/poll` 会等锁到超时（实测 2s 后 Agent
    /// View 直接退出）。因此所有可能重建终端对象的路径都必须先停止并 join 输入
    /// 线程，确保锁已经释放。
    async fn suspend(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        let Some(handle) = self.handle.take() else {
            return;
        };
        let _ = tokio::task::spawn_blocking(move || handle.join()).await;
    }
}

fn spawn_tick(tx: mpsc::UnboundedSender<AppMsg>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        loop {
            interval.tick().await;
            if tx.send(AppMsg::Tick).is_err() {
                break;
            }
        }
    });
}

async fn run_loop(
    terminal: &mut TerminalHost,
    input: &mut InputPump,
    app_rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    app: &mut App,
    fullscreen_view: &mut FullscreenView,
    theme: &'static Theme,
) -> Result<()> {
    // 已发布帧：只有真正 flush 成功的帧才会进来，鼠标事件只查它。
    let mut frames = FramePublisher::default();
    // `QAQH_HIT_PROBE=1|strict`（spec §8.1）；默认关闭。
    let probe = HitProbe::from_env();
    loop {
        if app.quit {
            break;
        }
        let route = route::resolve(app);
        if route != ScreenRoute::Agent {
            fullscreen_view.close_menu();
        }
        let size = terminal.terminal.size()?;
        if terminal.note_terminal_size(size.width, size.height) {
            fullscreen_view.close_menu();
            terminal.terminal.autoresize()?;
            // resize 后旧坐标不再对应屏幕上的任何东西。
            frames.invalidate();
            reset_pointer_state(app, fullscreen_view, PointerEvent::Resized);
        }

        if app.force_redraw {
            terminal.terminal.clear()?;
            app.force_redraw = false;
        }
        draw_and_publish(
            terminal,
            &mut frames,
            app,
            theme,
            &route,
            fullscreen_view,
            probe,
        )?;
        if route == ScreenRoute::Agent {
            fullscreen_view.clamp_scroll(app);
        }

        let Some(msg) = app_rx.recv().await else {
            break;
        };
        handle_message(app, msg, &mut frames, fullscreen_view);
        while let Ok(msg) = app_rx.try_recv() {
            handle_message(app, msg, &mut frames, fullscreen_view);
            if app.quit {
                break;
            }
        }
        if let Some(text) = app.pending_pager.take() {
            input.suspend().await;
            let result = terminal.run_pager(&text);
            input.resume();
            // pager 销毁并重建了 alternate screen，旧帧不再对应任何画面。
            frames.invalidate();
            clear_pointer_state(app, fullscreen_view);
            result?;
        }
    }
    Ok(())
}

/// 已发布帧的持有者。
///
/// 只有**真正 flush 成功**的帧才会出现在 `current` 里；鼠标事件只查它。
/// 任何会让画面与 `current` 不一致的操作都必须 `invalidate()`，让后续鼠标
/// 等到下一次成功绘制（spec §3.3）。
#[derive(Debug, Default)]
struct FramePublisher {
    current: Option<FrameHitMap>,
    next_frame_id: FrameId,
}

impl FramePublisher {
    /// 开始收集下一帧。帧序号只在 `publish` 成功后才推进。
    fn begin(
        &self,
        route: ScreenRoute,
        terminal_size: Size,
        scroll_offset: usize,
    ) -> HitMapBuilder {
        HitMapBuilder::new(self.next_frame_id, route, terminal_size, scroll_offset)
    }

    /// 发布一帧；几何不自洽或探针失败的帧**不发布**（鼠标等下一帧重绘）。
    ///
    /// `probe` 关闭时只跑 `validate` 几何门禁；`Warn`/`Strict` 时对真实渲染
    /// buffer 跑 spec §8.2 的全套自检。
    fn publish(
        &mut self,
        map: FrameHitMap,
        buffer: &Buffer,
        probe: HitProbe,
        expected_route: &ScreenRoute,
    ) -> Result<(), Vec<ProbeFailure>> {
        let failures = if probe.enabled() {
            map.probe(buffer, self.next_frame_id, expected_route).err()
        } else {
            map.validate().err().map(|_| Vec::new())
        };
        if let Some(failures) = failures {
            self.current = None;
            return Err(failures);
        }
        self.next_frame_id = map.frame_id.next();
        self.current = Some(map);
        Ok(())
    }

    fn invalidate(&mut self) {
        self.current = None;
    }

    fn route(&self) -> Option<&ScreenRoute> {
        self.current.as_ref().map(|frame| &frame.route)
    }

    /// 当前已发布帧；指针状态机只读它，不重算布局。
    fn current(&self) -> Option<&FrameHitMap> {
        self.current.as_ref()
    }

    /// 在已发布帧里查坐标；测试用 helper。生产事件走 `PointerState::handle`。
    #[cfg(test)]
    fn resolve(
        &self,
        column: u16,
        row: u16,
        button: ratatui::crossterm::event::MouseButton,
    ) -> Option<PointerTarget> {
        let frame = self.current.as_ref()?;
        match frame.resolve(column, row, button) {
            Ok(Some(region)) => Some(region.target.clone()),
            _ => None,
        }
    }
}

fn draw_and_publish(
    terminal: &mut TerminalHost,
    frames: &mut FramePublisher,
    app: &App,
    theme: &Theme,
    route: &ScreenRoute,
    fullscreen_view: &mut FullscreenView,
    probe: HitProbe,
) -> Result<()> {
    let size = terminal.terminal.size()?;
    let mut hit_map = frames.begin(
        route.clone(),
        Size::new(size.width, size.height),
        frame_scroll_offset(app, route),
    );
    let completed = terminal
        .terminal
        .draw(|frame| draw(frame, app, theme, route, fullscreen_view, &mut hit_map))?;
    match frames.publish(hit_map.finish(), completed.buffer, probe, route) {
        Ok(()) => Ok(()),
        Err(failures) => {
            let report = failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            if probe == HitProbe::Strict {
                return Err(anyhow::anyhow!(
                    "QAQH_HIT_PROBE=strict 失败（本帧不发布）：\n{report}"
                ));
            }
            // `Warn`：写结构化诊断，但不打断 TUI；本帧不发布，鼠标等下一帧。
            eprintln!("{report}");
            Ok(())
        }
    }
}

/// 这一帧对应的滚动量；用于 stale 判定与诊断。
fn frame_scroll_offset(app: &App, route: &ScreenRoute) -> usize {
    match route {
        ScreenRoute::Agent => app
            .active_session()
            .map_or(0, |session| session.scroll.offset),
        ScreenRoute::Modal(ModalRoute::Ask) => app
            .active_session()
            .and_then(|session| session.pending_ask.as_ref())
            .map_or(0, |panel| usize::from(panel.scroll)),
        ScreenRoute::Modal(ModalRoute::Plan) => app
            .active_session()
            .and_then(|session| session.pending_plan.as_ref())
            .map_or(0, |panel| panel.scroll),
        _ => 0,
    }
}

fn handle_message(
    app: &mut App,
    msg: AppMsg,
    frames: &mut FramePublisher,
    fullscreen_view: &mut FullscreenView,
) {
    // 每条消息都重新解析 route：批次里前面那条键可能已经切了页，后面的消息
    // 不能再用批次开始时的旧 route（spec §3.3 / P0-C-3）。
    let route = route::resolve(app);

    // 焦点丢失 = 合成 Leave：指针状态作废，但屏幕内容没变，帧仍然可信。
    if matches!(msg, AppMsg::FocusLost) {
        reset_pointer_state(app, fullscreen_view, PointerEvent::FocusLost);
        return;
    }

    if let AppMsg::Mouse(mouse) = msg {
        // 鼠标只能解释用户已经看到的那一帧。帧路由和当前路由不一致，说明中间
        // 切了页 / 开了弹窗；旧坐标必须作废，等下一次重绘。
        if frames.route() != Some(&route) {
            frames.invalidate();
            reset_pointer_state(app, fullscreen_view, PointerEvent::RouteChanged);
            return;
        }
        handle_pointer(app, frames, fullscreen_view, &route, mouse);
        return;
    }

    let is_key = matches!(msg, AppMsg::Key(_));
    handle_non_mouse_message(app, msg, &route, fullscreen_view);
    // 键可能改路由 / 滚动 / 模态；后端消息可能开新 overlay。只要画面可能变了，
    // 已发布帧立刻作废，后续鼠标等下一次重绘（spec §3.3 / P0-C-2）。
    if is_key || route::resolve(app) != route {
        frames.invalidate();
        clear_pointer_state(app, fullscreen_view);
    }
}

/// 帧失效时一并清掉指针的瞬时视觉状态：旧帧的 hover/pressed 不允许残留到新画面。
fn clear_pointer_state(app: &mut App, fullscreen_view: &mut FullscreenView) {
    reset_pointer_state(app, fullscreen_view, PointerEvent::Leave);
}

fn reset_pointer_state(app: &mut App, fullscreen_view: &mut FullscreenView, event: PointerEvent) {
    let _ = fullscreen_view.pointer_state.handle(None, event);
    sync_pointer_visual(app, fullscreen_view);
}

/// 把唯一状态机 `PointerState` 派生为绘制镜像。
///
/// `App::*_hover/pressed`、`FullscreenState`、`MessageMenu` 的鼠标字段都只是
/// 渲染缓存；生产路径不再直接写它们。
fn sync_pointer_visual(app: &mut App, fullscreen_view: &mut FullscreenView) {
    let visual = fullscreen_view.pointer_state.visual();
    app.modal_hover = match visual.hovered.as_ref() {
        Some(PointerTarget::Modal(hit)) => Some(*hit),
        _ => None,
    };
    app.modal_pressed = match visual.pressed.as_ref() {
        Some(PointerTarget::Modal(hit)) => Some(*hit),
        _ => None,
    };
    app.workspace_hover = match visual.hovered.as_ref() {
        Some(PointerTarget::Workspace(hit)) => Some(*hit),
        _ => None,
    };
    app.workspace_pressed = match visual.pressed.as_ref() {
        Some(PointerTarget::Workspace(hit)) => Some(*hit),
        _ => None,
    };

    let back = PointerTarget::Agent(AgentTarget::BackToLatest);
    fullscreen_view.pointer.back_to_latest_hover = visual.hovered.as_ref() == Some(&back);
    fullscreen_view.pointer.back_to_latest_pressed = visual.pressed.as_ref() == Some(&back);

    let menu_hover = visual
        .hovered
        .as_ref()
        .and_then(|target| menu_row_for(fullscreen_view, Some(target)));
    let menu_pressed = visual
        .pressed
        .as_ref()
        .and_then(|target| menu_row_for(fullscreen_view, Some(target)));
    if let Some(menu) = fullscreen_view.menu.as_mut() {
        menu.hover = menu_hover;
        menu.pressed = menu_pressed;
    }
}

fn handle_non_mouse_message(
    app: &mut App,
    msg: AppMsg,
    route: &ScreenRoute,
    fullscreen_view: &mut FullscreenView,
) {
    if *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && matches!(&msg, AppMsg::Paste(_))
    {
        return;
    }
    if *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && let AppMsg::Key(key) = &msg
    {
        handle_fullscreen_menu_key(app, fullscreen_view, key);
        return;
    }
    if let AppMsg::Key(key) = &msg
        && *route == ScreenRoute::Agent
    {
        match key.code {
            KeyCode::PageUp => {
                fullscreen_view.page_up(app);
                return;
            }
            KeyCode::PageDown => {
                fullscreen_view.scroll_down(app, 20);
                return;
            }
            _ => {}
        }
    }
    if let AppMsg::Key(key) = &msg
        && key.code == KeyCode::Esc
        && route::resolve(app) == ScreenRoute::Workspace(route::WorkspaceRoute::Todo)
    {
        clear_pointer_state(app, fullscreen_view);
        app.show_workspace = false;
        return;
    }
    if matches!(msg, AppMsg::Key(_)) {
        clear_pointer_state(app, fullscreen_view);
    }
    app.handle(msg);
}

/// 鼠标事件 → `PointerState` → 语义动作。命中只来自已发布帧，不再重算布局。
fn handle_pointer(
    app: &mut App,
    frames: &mut FramePublisher,
    fullscreen_view: &mut FullscreenView,
    route: &ScreenRoute,
    mouse: MouseEvent,
) {
    let event = match mouse.kind {
        MouseEventKind::Moved => PointerEvent::Moved {
            column: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::Down(button) => PointerEvent::Down {
            button,
            column: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::Up(button) => PointerEvent::Up {
            button,
            column: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::Drag(button) => PointerEvent::Drag {
            button,
            column: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::ScrollUp => PointerEvent::ScrollUp {
            column: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::ScrollDown => PointerEvent::ScrollDown {
            column: mouse.column,
            row: mouse.row,
        },
        _ => return,
    };
    let action = fullscreen_view
        .pointer_state
        .handle(frames.current(), event);
    dispatch_pointer_action(app, frames, fullscreen_view, route, action);
    sync_pointer_visual(app, fullscreen_view);
}

fn dispatch_pointer_action(
    app: &mut App,
    frames: &mut FramePublisher,
    fullscreen_view: &mut FullscreenView,
    route: &ScreenRoute,
    action: PointerAction,
) {
    match action {
        PointerAction::None | PointerAction::Redraw => {}
        PointerAction::Invalidate => frames.invalidate(),
        PointerAction::Pressed {
            target,
            column,
            row,
        } => {
            if fullscreen_view.menu.is_some()
                && menu_row_for(fullscreen_view, Some(&target)).is_none()
            {
                fullscreen_view.close_menu();
                fullscreen_view.pointer_state.clear();
                frames.invalidate();
                return;
            }
            if let PointerTarget::Agent(AgentTarget::Message {
                turn_id,
                block_id,
                role,
            }) = target
            {
                fullscreen_view.open_menu(
                    MessageHit {
                        turn_id,
                        block_id,
                        role,
                    },
                    column,
                    row,
                );
                fullscreen_view.pointer_state.clear();
                frames.invalidate();
            }
        }
        PointerAction::Activate { target, row, .. } => match target {
            PointerTarget::Modal(hit) => {
                dispatch_modal_hit(app, hit);
                fullscreen_view.pointer_state.clear();
                frames.invalidate();
            }
            PointerTarget::Workspace(hit) => {
                dispatch_workspace_hit(app, hit);
                fullscreen_view.pointer_state.clear();
                frames.invalidate();
            }
            PointerTarget::Agent(AgentTarget::BackToLatest) => {
                app.scroll_bottom();
                frames.invalidate();
            }
            PointerTarget::Agent(AgentTarget::MenuAction(action)) => {
                if action.enabled() {
                    activate_message_action(app, fullscreen_view, action);
                    fullscreen_view.pointer_state.clear();
                    frames.invalidate();
                }
            }
            PointerTarget::Scrollbar(ScrollbarPart::Track) => {
                if let Some(metrics) = scrollbar_metrics(app, fullscreen_view) {
                    set_scroll_offset(app, fullscreen_view, metrics.offset_for_track_row(row));
                    frames.invalidate();
                }
            }
            PointerTarget::Agent(AgentTarget::Thinking { block_id, .. }) => {
                if app.toggle_thinking_expanded(&block_id) {
                    frames.invalidate();
                }
            }
            PointerTarget::Agent(AgentTarget::Tool { block_id, .. }) => {
                if app.toggle_tool_expanded(&block_id) {
                    frames.invalidate();
                }
            }
            PointerTarget::Agent(AgentTarget::Message { .. }) => {
                // Message rows open their menu on press; a stale release is a no-op.
            }
            _ => {}
        },
        PointerAction::Scroll { up, .. } => {
            match route {
                ScreenRoute::Workspace(workspace_route) => {
                    handle_workspace_scroll(app, workspace_route, up)
                }
                ScreenRoute::Agent => {
                    if up {
                        fullscreen_view.scroll_up(app, 3);
                    } else {
                        fullscreen_view.scroll_down(app, 3);
                    }
                }
                ScreenRoute::Modal(_) => {}
            }
            frames.invalidate();
        }
        PointerAction::CaptureStarted { .. } | PointerAction::CaptureEnded { .. } => {}
        PointerAction::CaptureDragged {
            target,
            row,
            grab_offset,
            ..
        } => {
            if target == PointerTarget::Scrollbar(ScrollbarPart::Thumb)
                && let Some(metrics) = scrollbar_metrics(app, fullscreen_view)
            {
                set_scroll_offset(
                    app,
                    fullscreen_view,
                    metrics.offset_for_drag_row(row, grab_offset.1),
                );
                frames.invalidate();
            }
        }
        PointerAction::CaptureCancelled { .. } => {}
    }
}

fn scrollbar_metrics(app: &App, view: &FullscreenView) -> Option<ScrollbarMetrics> {
    let session = app.active_session()?;
    let body = view.body_area;
    let track = Rect::new(
        body.x.saturating_add(body.width.saturating_sub(1)),
        body.y,
        1,
        body.height,
    );
    ScrollbarMetrics::new(
        track,
        view.transcript.line_count(),
        usize::from(view.body_height),
        session.scroll.follow,
        session.scroll.offset,
    )
}

fn set_scroll_offset(app: &mut App, view: &FullscreenView, offset: usize) {
    let max = view.max_offset();
    if let Some(session) = app.active_session_mut() {
        session.scroll.follow = false;
        session.scroll.offset = offset.min(max);
    }
}

/// 命中目标对应的菜单行下标；非菜单行（含菜单外框）返回 `None`。
fn menu_row_for(view: &FullscreenView, target: Option<&PointerTarget>) -> Option<usize> {
    let Some(PointerTarget::Agent(AgentTarget::MenuAction(action))) = target else {
        return None;
    };
    view.menu.as_ref().and_then(|menu| {
        menu.actions()
            .iter()
            .position(|candidate| candidate == action)
    })
}

fn handle_workspace_scroll(app: &mut App, route: &route::WorkspaceRoute, up: bool) {
    match route {
        route::WorkspaceRoute::Sessions { .. }
        | route::WorkspaceRoute::Settings
        | route::WorkspaceRoute::History { detail: false, .. }
        | route::WorkspaceRoute::Subagents { .. } => {
            app.workspace_move_selection(if up { -1 } else { 1 });
        }
        route::WorkspaceRoute::History { detail: true, .. } => {
            app.workspace_scroll_view(up, 3);
        }
        route::WorkspaceRoute::Todo | route::WorkspaceRoute::Subagent { .. } => {
            if up {
                app.scroll_up(3);
            } else {
                app.scroll_down(3);
            }
        }
        route::WorkspaceRoute::Help => {}
    }
}

fn dispatch_workspace_hit(app: &mut App, hit: WorkspaceHit) {
    match hit {
        WorkspaceHit::SessionRow(index) => app.workspace_open_session(index),
        WorkspaceHit::HistoryTurn(index) => app.workspace_open_history(index),
        WorkspaceHit::TodoTask(_) => app.workspace_toggle_todo_detail(),
        WorkspaceHit::SubagentRow(index) => app.workspace_open_subagent(index),
        WorkspaceHit::SettingsRow(index) => app.mouse_settings_row(index),
        WorkspaceHit::Back => app.workspace_back(),
    }
}

/// 命中 → 动作。全部复用键盘路径已有的方法，不另开语义。
fn dispatch_modal_hit(app: &mut App, hit: ModalHit) {
    match hit {
        ModalHit::AskOption { question, option } => app.mouse_ask_option(question, option),
        ModalHit::AskCustom { question } => app.mouse_ask_custom(question),
        ModalHit::PermissionApprove => app.respond_permission(true),
        ModalHit::PermissionDeny => app.respond_permission(false),
        ModalHit::PermissionTrust => app.mouse_permission_toggle_trust(),
        ModalHit::PlanApprove => app.respond_plan(true, false),
        ModalHit::PlanApproveAutonomous => app.respond_plan(true, true),
        ModalHit::PlanReject => app.mouse_plan_start_reject(),
    }
}

struct TerminalHost {
    terminal: DefaultTerminal,
    last_terminal_size: (u16, u16),
    capture_enabled: bool,
}

impl TerminalHost {
    /// alternate-screen fullscreen shell.
    ///
    /// `ratatui::init()` enables raw mode, enters the alternate screen and installs
    /// the panic restore hook. This host is the only owner of mouse / paste /
    /// focus-change capture; every enable has an idempotent disable counterpart.
    fn init() -> Result<Self> {
        let terminal = ratatui::init();
        let terminal_size = ratatui::crossterm::terminal::size().unwrap_or((0, 0));
        let mut host = Self {
            terminal,
            last_terminal_size: terminal_size,
            capture_enabled: false,
        };
        if let Err(error) = host.enable_capture() {
            host.disable_capture();
            ratatui::restore();
            return Err(error).context("启用终端输入");
        }
        Ok(host)
    }

    fn enable_capture(&mut self) -> Result<()> {
        if self.capture_enabled {
            return Ok(());
        }
        if let Err(error) = execute!(
            stdout(),
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        ) {
            let _ = execute!(
                stdout(),
                DisableFocusChange,
                DisableBracketedPaste,
                DisableMouseCapture
            );
            return Err(error.into());
        }
        self.capture_enabled = true;
        Ok(())
    }

    fn disable_capture(&mut self) {
        if !self.capture_enabled {
            return;
        }
        let _ = execute!(
            stdout(),
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        self.capture_enabled = false;
    }

    fn note_terminal_size(&mut self, width: u16, height: u16) -> bool {
        let next = (width, height);
        let changed = self.last_terminal_size != next;
        self.last_terminal_size = next;
        changed
    }

    /// Suspend TUI for `$PAGER`, then restore the fullscreen shell.
    fn run_pager(&mut self, text: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("qaqh-pager-{}.md", std::process::id()));
        if std::fs::write(&path, text).is_err() {
            return Ok(());
        }

        self.disable_capture();
        ratatui::restore();
        let command = format!(
            "{} {}",
            crate::app::pager::pager_cmd(std::env::var("PAGER").ok().as_deref()),
            crate::app::pager::shell_quote(&path.to_string_lossy()),
        );
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .status();
        if matches!(status.map(|status| status.code()), Ok(Some(127))) {
            let _ = std::process::Command::new("cat").arg(&path).status();
        }

        self.terminal = ratatui::init();
        if let Err(error) = self.enable_capture() {
            ratatui::restore();
            return Err(error).context("恢复终端输入");
        }
        let _ = self.terminal.clear();
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

impl Drop for TerminalHost {
    fn drop(&mut self) {
        self.disable_capture();
    }
}

fn draw(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    route: &ScreenRoute,
    fullscreen_view: &mut FullscreenView,
    hit_map: &mut HitMapBuilder,
) {
    let area = frame.area();
    match route {
        ScreenRoute::Agent => draw_fullscreen_agent(frame, app, theme, fullscreen_view, hit_map),
        ScreenRoute::Modal(modal) => {
            clear_screen(frame, theme);
            crate::ui::v2::modal::draw(frame, app, area, theme, *modal, hit_map);
        }
        ScreenRoute::Workspace(workspace_route) => {
            clear_screen(frame, theme);
            workspace::draw(frame, app, workspace_route, theme, hit_map);
        }
    }
}

fn clear_screen(frame: &mut Frame, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        ratatui::widgets::Block::default()
            .style(Style::new().bg(theme.surface.base).fg(theme.text.primary)),
        area,
    );
}

struct AgentRender {
    lines: Vec<Line<'static>>,
    cursor: Option<Position>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentLayout {
    live_rows: usize,
    slash_rows: usize,
    stream_rows: usize,
    thinking_rows: usize,
    composer_rows: usize,
    status_rows: usize,
    shortcuts_rows: usize,
}

impl AgentLayout {
    fn height(self) -> usize {
        self.live_rows
            .saturating_add(self.slash_rows)
            .saturating_add(self.stream_rows)
            .saturating_add(self.thinking_rows)
            .saturating_add(self.composer_rows)
            .saturating_add(self.status_rows)
            .saturating_add(self.shortcuts_rows)
    }
}

/// 稳定行数：未封口时最后一行始终留在 live tail；封口时才允许提交最后一行。
fn session_is_working(session: &SessionState) -> bool {
    session.streaming.is_some() || session.timeline.running_turn_id().is_some()
}

/// live transcript 是否需要占位。
///
/// 当前只有**运行中的工具**需要多行可变正文：进度、输出、状态会持续变化。
/// reasoning 走 thinking 行；assistant open block 的稳定行走流式提交，未完成
/// 尾行走单独的 stream 行，不再把整段 assistant 塞进 live transcript。
fn composer_visual_rows(
    session: &crate::app::session::SessionState,
    width: u16,
    theme: &Theme,
) -> usize {
    let prefix = format!("{} ", theme.glyph.user);
    let text_width = usize::from(width).saturating_sub(prefix.width()).max(1);
    build_composer_rows(&session.composer.input, session.composer.cursor, text_width)
        .0
        .len()
}

/// composer 上方的单行 thinking 状态。
///
/// - 始终保留 spinner：只要 turn/stream 仍在工作，即使当前没有 reasoning
///   正文，也不能把“正在工作”信号关掉。
/// - 文本只取 reasoning 当前行的尾部窗口；换行后只显示新行。
/// - 正文按字符做 shimmer，保证移动高光与字符边界一致。
fn thinking_line(session: &SessionState, width: u16, theme: &Theme) -> Line<'static> {
    let frame = crate::app::anim::frame_now();
    let spinner = crate::app::anim::claude_spinner_glyph(frame);
    let prefix = format!("{spinner} ");
    let prefix_width = spinner.width() + 1;
    let budget = usize::from(width).saturating_sub(prefix_width).max(1);

    let text = latest_reasoning_line(session).unwrap_or_else(|| {
        session
            .streaming
            .as_ref()
            .map(|state| format!("{}…", state.phase.label()))
            .unwrap_or_else(|| "working…".to_string())
    });
    let shown = tail_cols(&text, budget);

    let mut spans = vec![Span::styled(prefix, Style::new().fg(theme.accent.thinking))];
    spans.extend(shimmer_spans(&shown, frame, theme));
    Line::from(spans)
}

/// 当前 running turn 中最后一个 reasoning block 的当前行。
///
/// 使用 `rsplit('\n')` 而不是 `lines().last()`：如果模型刚输出换行，
/// 新行即使暂时为空也必须立刻替换旧行，不能继续显示旧内容。
fn latest_reasoning_line(session: &SessionState) -> Option<String> {
    let turn_id = session.timeline.running_turn_id()?;
    let turn = session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)?;
    for round in turn.rounds.iter().rev() {
        for block in round.blocks.iter().rev() {
            if block.kind == TimelineBlockKind::Reasoning {
                return Some(
                    block
                        .text
                        .rsplit('\n')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    None
}

/// 取文本尾部、保证最右侧（最新字符）可见；宽字符要么完整保留、要么整体舍弃。
fn tail_cols(line: &str, max_width: usize) -> String {
    let max_width = max_width.max(1);
    let mut used = 0usize;
    let mut chars = Vec::new();
    for ch in line.chars().rev() {
        let width = ch.width().unwrap_or(0);
        if used + width > max_width {
            break;
        }
        used += width;
        chars.push(ch);
    }
    chars.reverse();
    chars.into_iter().collect()
}

/// 字符级 shimmer：每个字符独立取色，形成一条从左向右移动的窄光带。
fn shimmer_spans(text: &str, frame: u64, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let period = chars.len().max(8) + 8;
    let center = (frame % period as u64) as isize - 4;
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = (index as isize - center).unsigned_abs();
            let style = match distance {
                0 => Style::new().fg(theme.text.bright),
                1 => Style::new().fg(theme.accent.thinking),
                2 => Style::new().fg(theme.text.muted),
                _ => Style::new().fg(theme.text.dim),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

struct ComposerRender {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_x: u16,
}

fn composer_lines(
    input: &[char],
    cursor: usize,
    width: u16,
    theme: &Theme,
    max_rows: usize,
) -> ComposerRender {
    let prefix = format!("{} ", theme.glyph.user);
    let continuation = " ".repeat(prefix.width());
    let prefix_width = prefix.width();
    let text_width = usize::from(width).saturating_sub(prefix_width).max(1);
    let (rows, cursor_row, cursor_col) = build_composer_rows(input, cursor, text_width);
    let max_rows = max_rows.max(1);
    let start = if rows.len() <= max_rows {
        0
    } else {
        cursor_row
            .saturating_sub(max_rows - 1)
            .min(rows.len().saturating_sub(max_rows))
    };
    let end = start.saturating_add(max_rows).min(rows.len());
    let mut lines = Vec::with_capacity(end.saturating_sub(start));
    for (visible_idx, row) in rows[start..end].iter().enumerate() {
        let actual_idx = start + visible_idx;
        let row_prefix = if actual_idx == 0 {
            prefix.clone()
        } else {
            continuation.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(row_prefix, Style::new().fg(theme.accent.user)),
            Span::styled(
                row.iter().collect::<String>(),
                Style::new().fg(theme.text.primary),
            ),
        ]));
    }
    let cursor_row = cursor_row
        .saturating_sub(start)
        .min(lines.len().saturating_sub(1));
    let cursor_x = prefix_width
        .saturating_add(cursor_col)
        .min(usize::from(width).saturating_sub(1)) as u16;
    ComposerRender {
        lines,
        cursor_row,
        cursor_x,
    }
}

/// 把 composer 字符流切成可视行，同时记录光标在其中的位置。
///
/// 这里不依赖终端写入光标；返回值只用于 `Frame::set_cursor_position`，因此
/// 中文、emoji 等宽字符可以按 `unicode-width` 正确计算列号。
fn build_composer_rows(
    input: &[char],
    cursor: usize,
    width: usize,
) -> (Vec<Vec<char>>, usize, usize) {
    let width = width.max(1);
    let cursor = cursor.min(input.len());
    let mut rows = vec![Vec::new()];
    let mut row_width = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for (idx, ch) in input.iter().enumerate() {
        if idx == cursor {
            cursor_row = rows.len() - 1;
            cursor_col = row_width;
        }
        let ch = match ch {
            '\n' | '\r' => {
                rows.push(Vec::new());
                row_width = 0;
                continue;
            }
            value if value.is_control() => '�',
            value => *value,
        };
        let char_width = ch.width().unwrap_or(0);
        if char_width > 0 && row_width > 0 && row_width + char_width > width {
            rows.push(Vec::new());
            row_width = 0;
        }
        rows.last_mut().expect("composer row").push(ch);
        row_width = row_width.saturating_add(char_width);
    }
    if cursor == input.len() {
        cursor_row = rows.len() - 1;
        cursor_col = row_width;
    }
    (rows, cursor_row, cursor_col)
}

fn slash_menu_rows(app: &App) -> usize {
    if !app.overlays.is_empty() || app.inspecting() {
        return 0;
    }
    app.slash_candidates().len().min(MAX_SLASH_ROWS)
}

fn slash_menu_lines(app: &App, width: u16, theme: &Theme, capacity: usize) -> Vec<Line<'static>> {
    if capacity == 0 {
        return Vec::new();
    }
    let candidates = app.slash_candidates();
    if candidates.is_empty() {
        return Vec::new();
    }
    let selected = app.slash_selected.min(candidates.len().saturating_sub(1));
    let start = selected
        .saturating_sub(capacity.saturating_sub(1))
        .min(candidates.len().saturating_sub(capacity));
    candidates[start..start + capacity]
        .iter()
        .enumerate()
        .map(|(offset, candidate)| {
            let idx = start + offset;
            let marker = if idx == selected { "▸" } else { " " };
            let style = if idx == selected {
                Style::new().fg(theme.accent.assistant)
            } else {
                Style::new().fg(theme.text.dim)
            };
            let mut spans = vec![Span::styled(
                format!(" {marker} /{:<9}", candidate.name),
                style,
            )];
            if width >= 50 {
                spans.push(Span::styled(
                    candidate.desc.to_string(),
                    Style::new().fg(theme.text.dim),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn status_line(app: &App, width: u16, theme: &Theme) -> Line<'static> {
    let (mark, label, color) = match app.conn_phase {
        ConnPhase::Opening => ("○", "opening", theme.semantic.warning),
        ConnPhase::Ready => ("●", "ready", theme.accent.success),
        ConnPhase::ReadyWithIssue => ("●", "degraded", theme.semantic.warning),
        ConnPhase::Lost => ("✗", "lost", theme.accent.error),
    };
    let mut spans = vec![Span::styled(
        format!(" {mark} {label}"),
        Style::new().fg(color),
    )];
    // V2 fullscreen 需要在底部状态区提供 toast 面。
    // status_line 只画连接相位与常驻信息，于是命令失败 / 应答超时 / 上传失败
    // 这类反馈在 Agent View 里**完全不可见**（后端 issue #41 第 2 项正是靠
    // `permission-hang` 把这个洞暴露出来的）。
    // 这里给 toast 最高优先级：有 toast 时让位掉 model / cwd / usage 这些常驻项，
    // 保证瞬时的错误提示一定画得出来。
    let toast = app.toasts.back();
    if let Some(session) = app.active_session() {
        spans.push(Span::styled(
            format!(" · {}", session.activity_label()),
            Style::new().fg(theme.text.secondary),
        ));
        if toast.is_none() && width >= 40 {
            if let Some(model) = session.display_model() {
                spans.push(Span::styled(
                    format!(" · {model}"),
                    Style::new().fg(theme.text.dim),
                ));
            }
            spans.push(Span::styled(
                format!(
                    " · {}",
                    match session.mode {
                        ConversationMode::Plan => "plan",
                        ConversationMode::Code => "code",
                    }
                ),
                Style::new().fg(theme.text.dim),
            ));
        }
        if toast.is_none()
            && width >= 70
            && let Some(cwd) = app.effective_cwd(None)
        {
            spans.push(Span::styled(
                format!(" · {}", crate::app::truncate_str(&cwd, 28)),
                Style::new().fg(theme.text.dim),
            ));
        }
        if toast.is_none() && width >= 50 {
            if let Some(usage) = &session.usage {
                spans.push(Span::styled(
                    format!(
                        " · ↑{}k ↓{}k",
                        usage.prompt_tokens / 1000,
                        usage.completion_tokens / 1000
                    ),
                    Style::new().fg(theme.text.dim),
                ));
            }
            if !session.composer.attachments.is_empty() {
                spans.push(Span::styled(
                    format!(" · ✎{}", session.composer.attachments.len()),
                    Style::new().fg(theme.semantic.warning),
                ));
            }
        }
        if width >= 70
            && let Some(error) = app.conn_error.as_deref()
        {
            // 连接诊断（含 `reconnect_message` 的 lagged/终止流文案）此前写死
            // 截断 28 列，恰好把**原因**切掉：实测
            // `timeline[0ea0909d]服务端终止流（l…`——连 "lagged" 都看不到，
            // U-07 的 e2e 因此断言不到。改为按剩余宽度给额度（至少 28 列，
            // 保住原来窄屏时的下限）。
            let used: usize = spans.iter().map(|span| span.content.width()).sum();
            let budget = (width as usize).saturating_sub(used + 8).max(28);
            spans.push(Span::styled(
                format!(" · {}", crate::app::truncate_str(error, budget)),
                Style::new().fg(theme.semantic.warning),
            ));
        }
    }
    if let Some(toast) = toast {
        let color = match toast.level {
            NoticeLevel::Info => theme.text.secondary,
            NoticeLevel::Warn => theme.semantic.warning,
            NoticeLevel::Error => theme.accent.error,
        };
        // 宽度感知的截断。此前写死 44 列，在 130 列的终端上白白切掉诊断的
        // **关键部分**：实测 `timeline[0ea0909d] 服务端终止流（lagged，…`
        // 被切成 `…服务端终止流（l…`——连 "lagged" 都看不到，e2e 因此断言不到
        // （U-07）。这里按「已用宽度 + 时间戳」算剩余额度，并留 8 列余量；
        // 至少给 24 列，避免窄屏时把提示压成一个词。
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let budget = (width as usize).saturating_sub(used + 8).max(24);
        spans.push(Span::styled(
            format!(" · {}", crate::app::truncate_str(&toast.text, budget)),
            Style::new().fg(color),
        ));
    }
    spans.push(Span::styled(
        format!(" {}", chrono::Local::now().format("%H:%M")),
        Style::new().fg(theme.text.dim),
    ));
    Line::from(spans)
}

fn shortcuts_line(app: &App, width: u16, theme: &Theme) -> Line<'static> {
    let slash_open = app.overlays.is_empty() && !app.slash_candidates().is_empty();
    let waiting = app
        .active_session()
        .is_some_and(|session| session.is_waiting_user());
    let streaming = app
        .active_session()
        .is_some_and(|session| session.streaming.is_some());
    let has_attachment = app
        .active_session()
        .is_some_and(|session| !session.composer.attachments.is_empty());
    let text = if waiting {
        " Enter 应答 · Esc 取消 · F1 帮助"
    } else if slash_open {
        " ↑↓ 选择 · Tab/Enter 补全 · Esc 关闭 · F1 帮助"
    } else if streaming {
        " Esc 中止 · Ctrl+Y 撤销 · Ctrl+E 压缩 · F1 帮助"
    } else if has_attachment {
        " Enter 发送 · Ctrl+A 附件 · Ctrl+Y 撤销 · F1 帮助"
    } else if app.active_session().is_some() {
        " Enter 发送 · Alt+T 展开思考 · Alt+E 展开工具 · Ctrl+P 模式 · F1 帮助"
    } else {
        " Ctrl+N 新建 · Ctrl+L 会话 · F1 帮助 · Ctrl+Q 退出"
    };
    let compact = if waiting {
        " Enter 应答 · Esc 取消"
    } else if slash_open {
        " ↑↓ 选择 · Enter 补全"
    } else if streaming {
        " Esc 中止 · Ctrl+E 压缩"
    } else if has_attachment {
        " Enter 发送 · Ctrl+A 附件"
    } else if app.active_session().is_some() {
        " Enter 发送 · Alt+Enter 换行"
    } else {
        " Ctrl+N 新建 · Ctrl+L 会话"
    };
    let text = if width < 60 { compact } else { text };
    Line::from(Span::styled(text, Style::new().fg(theme.text.dim)))
}

#[cfg(test)]
mod tests {
    use super::fullscreen::{FullscreenTranscriptCache, MessageHit, assistant_markdown};
    use super::*;
    use crate::app::Overlay;
    use crate::app::session::{PermissionPanel, SessionState};
    use crate::app::timeline_model::TimelineModel;
    use crate::theme::{ColorSupport, ThemeKind};
    use crate::ui::v2::fullscreen::{FullscreenState, MessageAction, MessageMenu, MessageRole};
    use crate::ui::v2::hit::{AgentTarget, FrameHitMap, HitRegion, PointerTarget, VisualAnchor, z};
    use qaqh_client::{
        PermissionCategory, PermissionRisk, SessionListEntry, SessionMeta, TimelineBlock,
        TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers, MouseButton};
    use ratatui::layout::Rect;

    fn test_theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    /// 测试用：跑一帧 Agent 绘制（HitMap 丢弃；只看画面）。
    fn draw_agent_frame(
        terminal: &mut Terminal<TestBackend>,
        app: &App,
        route: &ScreenRoute,
        view: &mut FullscreenView,
    ) {
        let size = terminal.size().expect("terminal size");
        let mut hit_map = HitMapBuilder::new(
            FrameId::new(1),
            route.clone(),
            ratatui::layout::Size::new(size.width, size.height),
            0,
        );
        terminal
            .draw(|frame| draw(frame, app, &test_theme(), route, view, &mut hit_map))
            .expect("draw fullscreen agent");
    }

    fn entry(seq: u64, turn: &str, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry {
            timeline_seq: seq,
            turn_id: turn.to_string(),
            round_num: Some(0),
            event,
        }
    }

    fn app_with_model(model: TimelineModel) -> App {
        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-1".to_string();
        let mut session = SessionState::new(seed.clone());
        session.timeline = model;
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    fn model_with_sealed_answer() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Sealed,
                    text: "answer".to_string(),
                    tool: None,
                },
            },
        ));
        model
    }

    fn model_with_thinking_history() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-thinking",
            TimelineEvent::TurnOpened {
                user_text: "think".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-thinking",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "thinking-1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Sealed,
                    text: "first thought\nsecond thought".to_string(),
                    tool: None,
                },
            },
        ));
        model.apply(&entry(
            3,
            "turn-thinking",
            TimelineEvent::TurnSealed {
                state: qaqh_client::TimelineTurnState::Completed,
                failure: None,
            },
        ));
        model
    }

    fn model_with_tool_card() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-tool",
            TimelineEvent::TurnOpened {
                user_text: "run tests".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-tool",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "tool-1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Sealed,
                    text: String::new(),
                    tool: Some(qaqh_client::TimelineTool {
                        tool_call_id: "call-1".to_string(),
                        name: "exec".to_string(),
                        state: qaqh_client::TimelineToolState::Succeeded,
                        summary: Some("cargo test".to_string()),
                        args_json: None,
                        output: Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight".to_string()),
                        diff: None,
                        progress: String::new(),
                        progress_truncated: false,
                        progress_stream: None,
                        progress_bytes_total: 0,
                        display: None,
                        failure: None,
                        permission: None,
                    }),
                },
            },
        ));
        model
    }

    fn model_with_many_sealed_turns(count: usize) -> TimelineModel {
        let mut model = TimelineModel::default();
        for index in 0..count {
            let turn = format!("turn-{index}");
            model.apply(&entry(
                index as u64 * 2 + 1,
                &turn,
                TimelineEvent::TurnOpened {
                    user_text: format!("user-{index}"),
                },
            ));
            model.apply(&entry(
                index as u64 * 2 + 2,
                &turn,
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_id: format!("block-{index}"),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: format!("answer-{index}"),
                        tool: None,
                    },
                },
            ));
        }
        model
    }

    #[test]
    fn fullscreen_agent_draw_uses_full_buffer_and_keeps_composer_visible() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .scroll
            .follow = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView {
            pointer: FullscreenState {
                back_to_latest_hover: true,
                back_to_latest_pressed: false,
            },
            ..Default::default()
        };

        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("answer-29"), "{text}");
        assert!(compact.contains("回到最新消息"), "{text}");
    }

    #[test]
    fn fullscreen_agent_draw_survives_resize() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        for (width, height) in [(80, 24), (40, 20), (20, 8), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            draw_agent_frame(&mut terminal, &app, &route, &mut view);
        }
    }

    #[test]
    fn fullscreen_context_menu_renders_copy_and_disabled_actions() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView {
            menu: Some(MessageMenu::new(
                "turn-1".into(),
                "b1".into(),
                MessageRole::Assistant,
                Position::new(20, 5),
            )),
            ..Default::default()
        };

        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("消息操作"), "{text}");
        assert!(compact.contains("复制成Markdown"), "{text}");
        assert!(compact.contains("重新回答"), "{text}");
        assert!(compact.contains("从这里继续"), "{text}");
    }

    #[test]
    fn user_undo_opens_second_confirmation() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.confirm_undo_turn("turn-1".into());
        assert!(matches!(
            app.overlays.last(),
            Some(Overlay::Confirm {
                action: crate::app::ConfirmAction::UndoTurn { seed, turn_id }
            }) if seed == "seed-1" && turn_id == "turn-1"
        ));
    }

    #[test]
    fn fullscreen_scroll_clamps_to_rendered_content() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcript.sync(&app, 79, &theme);
        view.body_height = 10;

        view.scroll_up(&mut app, usize::MAX / 2);
        let session = app.active_session().expect("session");
        assert!(!session.scroll.follow);
        assert_eq!(session.scroll.offset, view.max_offset());

        view.scroll_down(&mut app, usize::MAX / 2);
        assert!(app.active_session().expect("session").scroll.follow);
    }

    #[tokio::test]
    async fn fullscreen_page_up_requests_older_at_top() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcript.sync(&app, 79, &theme);
        view.body_height = 10;

        let session = app.sessions.get_mut("seed-1").expect("session");
        session.timeline.has_more = true;
        session.timeline.turns[0].turn_index = Some(1);
        session.scroll.offset = view.max_offset();

        view.page_up(&mut app);

        assert!(app.sessions["seed-1"].loading_older);
    }

    #[test]
    fn fullscreen_block_cache_reuses_unchanged_blocks() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        let theme = test_theme();
        let mut cache = FullscreenTranscriptCache::default();

        cache.sync(&app, 79, &theme);
        assert_eq!(cache.render_misses, 6);
        let before = cache.render_misses;

        cache.sync(&app, 79, &theme);
        assert_eq!(cache.render_misses, before, "same version must be a no-op");

        let session = app.sessions.get_mut("seed-1").expect("session");
        session.timeline.version = session.timeline.version.saturating_add(1);
        let turn = session.timeline.turns.last_mut().expect("turn");
        let block = turn
            .rounds
            .iter_mut()
            .flat_map(|round| &mut round.blocks)
            .find(|block| block.kind == TimelineBlockKind::Text)
            .expect("text block");
        block.text.push_str(" updated");

        cache.sync(&app, 79, &theme);
        assert_eq!(
            cache.render_misses,
            before + 1,
            "only the changed block should render again"
        );
    }

    #[test]
    fn assistant_markdown_prefers_the_clicked_block() {
        let app = app_with_model(model_with_many_sealed_turns(2));
        assert_eq!(
            assistant_markdown(&app, "turn-1", "block-1").as_deref(),
            Some("answer-1")
        );
    }

    /// 跑真实 Agent draw 并把这一帧的 HitMap 取出来。
    fn draw_agent_to_map(
        app: &App,
        view: &mut FullscreenView,
        width: u16,
        height: u16,
    ) -> (FrameHitMap, TestBackend) {
        let theme = test_theme();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut builder = HitMapBuilder::new(
            FrameId::new(1),
            ScreenRoute::Agent,
            ratatui::layout::Size::new(width, height),
            0,
        );
        terminal
            .draw(|frame| draw_fullscreen_agent(frame, app, &theme, view, &mut builder))
            .expect("draw fullscreen agent");
        let map = builder.finish();
        assert!(
            map.validate().is_ok(),
            "Agent HitMap 必须通过几何校验：{:?}",
            map.validate().err()
        );
        let probe = map.probe(terminal.backend().buffer(), map.frame_id, &map.route);
        assert!(
            probe.is_ok(),
            "Agent 真实帧必须通过 strict 探针：{:?}",
            probe.err()
        );
        (map, terminal.backend().clone())
    }

    /// 命中区四角可达、外扩一格不可达。
    fn assert_target_reachable(map: &FrameHitMap, target: &PointerTarget) {
        let region = map
            .regions
            .iter()
            .find(|region| &region.target == target)
            .unwrap_or_else(|| panic!("HitMap 必须登记 {target:?}"));
        let rect = region.rect;
        assert!(!rect.is_empty(), "{target:?} 的矩形不能为空");
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
                Some(target),
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
                Some(target),
                "({x},{y}) 在 {target:?} 外扩一格内，不该命中"
            );
        }
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

    #[test]
    fn agent_draw_registers_visible_message_rows() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let (map, backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert_anchors_non_empty(&backend, &map);

        let messages: Vec<PointerTarget> = map
            .regions
            .iter()
            .filter(|region| {
                matches!(
                    region.target,
                    PointerTarget::Agent(AgentTarget::Message { .. })
                )
            })
            .map(|region| region.target.clone())
            .collect();
        assert!(!messages.is_empty(), "可见消息必须登记成 HitRegion");
        for target in &messages {
            assert_target_reachable(&map, target);
        }
        // 贴底时最后一个回合的回复一定在视口里。
        assert!(
            map.regions.iter().any(|region| region.target
                == PointerTarget::Agent(AgentTarget::Message {
                    turn_id: "turn-2".into(),
                    block_id: "block-2".into(),
                    role: MessageRole::Assistant,
                })),
            "贴底时应能看到最后一个回合的回复"
        );
    }

    #[test]
    fn alt_e_toggles_latest_tool_card() {
        let mut app = app_with_model(model_with_tool_card());
        app.show_workspace = false;
        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::ALT,
        )));
        assert!(
            app.sessions["seed-1"].expanded_tools.contains("tool-1"),
            "Alt+E must provide the keyboard path before mouse wiring"
        );
    }

    #[test]
    fn alt_t_toggles_latest_thinking_history() {
        let mut app = app_with_model(model_with_thinking_history());
        app.show_workspace = false;
        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::ALT,
        )));
        assert!(
            app.sessions["seed-1"]
                .expanded_thinking
                .contains("thinking-1"),
            "Alt+T must provide the keyboard path for thinking history"
        );
    }

    #[test]
    fn thinking_click_toggles_expansion_via_presented_frame() {
        let mut app = app_with_model(model_with_thinking_history());
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let target = frames
            .current()
            .expect("published frame")
            .regions
            .iter()
            .find_map(|region| match &region.target {
                PointerTarget::Agent(AgentTarget::Thinking { block_id, .. })
                    if block_id == "thinking-1" =>
                {
                    Some((region.target.clone(), region.rect))
                }
                _ => None,
            })
            .expect("thinking block must be registered");
        assert_target_reachable(frames.current().unwrap(), &target.0);

        let (column, row) = (target.1.x + 1, target.1.y);
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
            )),
            &mut frames,
            &mut view,
        );
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row,
            )),
            &mut frames,
            &mut view,
        );
        assert!(
            app.sessions["seed-1"]
                .expanded_thinking
                .contains("thinking-1"),
            "click must toggle the historical thinking block"
        );
    }

    #[test]
    fn tool_card_click_toggles_expansion_via_presented_frame() {
        let mut app = app_with_model(model_with_tool_card());
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let target = frames
            .current()
            .expect("published frame")
            .regions
            .iter()
            .find_map(|region| match &region.target {
                PointerTarget::Agent(AgentTarget::Tool { block_id, .. })
                    if block_id == "tool-1" =>
                {
                    Some((region.target.clone(), region.rect))
                }
                _ => None,
            })
            .expect("tool card must be registered");
        assert_target_reachable(frames.current().unwrap(), &target.0);

        let (column, row) = (target.1.x + 1, target.1.y);
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
            )),
            &mut frames,
            &mut view,
        );
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row,
            )),
            &mut frames,
            &mut view,
        );
        assert!(
            app.sessions["seed-1"].expanded_tools.contains("tool-1"),
            "click must toggle the tool card through the same App path"
        );
    }

    #[test]
    fn agent_draw_registers_back_to_latest_only_when_scrolled() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .scroll
            .follow = false;
        let mut view = FullscreenView::default();
        let (map, backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert_anchors_non_empty(&backend, &map);
        assert_target_reachable(&map, &PointerTarget::Agent(AgentTarget::BackToLatest));

        // 贴底（follow=true）时不画也不登记。
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let (map, _backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert!(
            !map.regions
                .iter()
                .any(|region| region.target == PointerTarget::Agent(AgentTarget::BackToLatest)),
            "贴底时不该有回到最新按钮"
        );
    }

    #[test]
    fn agent_draw_registers_scrollbar_track_and_thumb() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.scroll.follow = false;
        session.scroll.offset = 20;
        let mut view = FullscreenView::default();
        let (map, backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert_anchors_non_empty(&backend, &map);

        let thumb = PointerTarget::Scrollbar(ScrollbarPart::Thumb);
        let track = PointerTarget::Scrollbar(ScrollbarPart::Track);
        assert_target_reachable(&map, &thumb);
        let track_region = map
            .regions
            .iter()
            .find(|region| region.target == track)
            .expect("scrollbar track must be registered");
        let thumb_region = map
            .regions
            .iter()
            .find(|region| region.target == thumb)
            .expect("scrollbar thumb must be registered");
        let row = if thumb_region.rect.y > track_region.rect.y {
            track_region.rect.y
        } else {
            thumb_region.rect.bottom()
        };
        let hit = map
            .resolve(track_region.rect.x, row, MouseButton::Left)
            .expect("track hit")
            .expect("track point");
        assert_eq!(hit.target, track);
    }

    #[test]
    fn agent_scrollbar_track_click_and_thumb_drag_update_scroll_state() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .scroll
            .follow = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let route = ScreenRoute::Agent;
        let metrics = scrollbar_metrics(&app, &view).expect("scrollbar metrics");

        let track_row = metrics.track.y.saturating_add(1);
        handle_pointer(
            &mut app,
            &mut frames,
            &mut view,
            &route,
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                metrics.track.x,
                track_row,
            ),
        );
        handle_pointer(
            &mut app,
            &mut frames,
            &mut view,
            &route,
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                metrics.track.x,
                track_row,
            ),
        );
        assert_eq!(
            app.sessions["seed-1"].scroll.offset,
            metrics
                .offset_for_track_row(track_row)
                .min(view.max_offset())
        );

        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let metrics = scrollbar_metrics(&app, &view).expect("scrollbar metrics");
        let drag_row = metrics.track.bottom().saturating_sub(1);
        handle_pointer(
            &mut app,
            &mut frames,
            &mut view,
            &route,
            left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                metrics.thumb.x,
                metrics.thumb.y,
            ),
        );
        handle_pointer(
            &mut app,
            &mut frames,
            &mut view,
            &route,
            left_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                metrics.thumb.x,
                drag_row,
            ),
        );
        handle_pointer(
            &mut app,
            &mut frames,
            &mut view,
            &route,
            left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                metrics.thumb.x,
                drag_row,
            ),
        );
        assert_eq!(
            app.sessions["seed-1"].scroll.offset,
            metrics
                .offset_for_drag_row(drag_row, 0)
                .min(view.max_offset())
        );
    }

    #[test]
    fn agent_draw_registers_menu_rows_and_blocks_click_through() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        // 先画一帧拿到 transcript spans，再在 assistant 消息上开菜单。
        let _ = draw_agent_to_map(&app, &mut view, 80, 24);
        view.open_menu(
            MessageHit {
                turn_id: "turn-2".into(),
                block_id: "block-2".into(),
                role: MessageRole::Assistant,
            },
            10,
            5,
        );
        let (map, backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert_anchors_non_empty(&backend, &map);

        let copy = PointerTarget::Agent(AgentTarget::MenuAction(MessageAction::CopyMarkdown));
        assert_target_reachable(&map, &copy);

        // disabled 行照样登记，但 enabled=false；点它只能落到菜单外框。
        let retry = PointerTarget::Agent(AgentTarget::MenuAction(MessageAction::Retry));
        let retry_region = map
            .regions
            .iter()
            .find(|region| region.target == retry)
            .expect("disabled 行也要登记");
        assert!(!retry_region.enabled);
        let hit = map
            .resolve(
                retry_region.rect.x + 2,
                retry_region.rect.y,
                MouseButton::Left,
            )
            .expect("同 z 区域不得重叠");
        assert_eq!(
            hit.map(|region| &region.target),
            Some(&PointerTarget::Agent(AgentTarget::MenuRoot)),
            "disabled 行必须被菜单外框吃掉"
        );

        // 菜单外框可命中，且优先级高于底下的消息行。
        let root = PointerTarget::Agent(AgentTarget::MenuRoot);
        let root_region = map
            .regions
            .iter()
            .find(|region| region.target == root)
            .expect("菜单外框");
        let hit = map
            .resolve(root_region.rect.x, root_region.rect.y, MouseButton::Left)
            .expect("同 z 区域不得重叠");
        assert_eq!(hit.map(|region| &region.target), Some(&root));
    }

    /// 菜单与「回到最新」按钮重叠时，菜单必须赢——z 层级而不是绘制顺序决定命中。
    #[test]
    fn agent_menu_wins_over_back_to_latest_when_they_overlap() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .scroll
            .follow = false;
        let mut view = FullscreenView::default();
        let _ = draw_agent_to_map(&app, &mut view, 80, 24);

        let back = crate::ui::v2::fullscreen::back_to_latest_rect(view.body_area)
            .expect("滚动状态应有回到最新按钮");
        view.open_menu(
            MessageHit {
                turn_id: "turn-29".into(),
                block_id: "block-29".into(),
                role: MessageRole::Assistant,
            },
            back.x,
            back.y,
        );
        let (map, _backend) = draw_agent_to_map(&app, &mut view, 80, 24);

        let back_target = PointerTarget::Agent(AgentTarget::BackToLatest);
        assert!(
            map.regions
                .iter()
                .any(|region| region.target == back_target),
            "滚动状态仍要登记回到最新"
        );
        let root = PointerTarget::Agent(AgentTarget::MenuRoot);
        let root_region = map
            .regions
            .iter()
            .find(|region| region.target == root)
            .expect("菜单外框");
        assert!(
            crate::ui::v2::hit::contains(back, root_region.rect.x, root_region.rect.y),
            "构造点必须同时落在两个浮层里，才算真的验证了遮挡"
        );
        let hit = map
            .resolve(root_region.rect.x, root_region.rect.y, MouseButton::Left)
            .expect("同 z 区域不得重叠");
        assert_eq!(
            hit.map(|region| &region.target),
            Some(&root),
            "菜单必须压在回到最新之上"
        );
    }
    fn permission_panel() -> PermissionPanel {
        PermissionPanel {
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
        }
    }

    /// 用真实 `draw` 给当前 app 发布一帧（走和 run_loop 一样的 builder → publish 路径）。
    /// 用真实 `draw` 给当前 app 发布一帧（走和 run_loop 一样的 builder → probe → publish
    /// 路径）。**固定用 `HitProbe::Strict`**：每条走这条 helper 的测试同时都在证明
    /// 该路由的真实帧能通过 strict 探针。
    fn publish_frame(
        app: &App,
        view: &mut FullscreenView,
        width: u16,
        height: u16,
    ) -> FramePublisher {
        let route = route::resolve(app);
        let mut frames = FramePublisher::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut builder = frames.begin(route.clone(), Size::new(width, height), 0);
        let completed = terminal
            .draw(|frame| draw(frame, app, &test_theme(), &route, view, &mut builder))
            .expect("draw frame");
        frames
            .publish(builder.finish(), completed.buffer, HitProbe::Strict, &route)
            .unwrap_or_else(|failures| {
                panic!(
                    "strict 探针必须通过真实帧：\n{}",
                    failures
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            });
        frames
    }

    fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn message_rect(frames: &FramePublisher) -> Rect {
        frames
            .current()
            .expect("已发布帧")
            .regions
            .iter()
            .find(|region| {
                matches!(
                    region.target,
                    PointerTarget::Agent(AgentTarget::Message { .. })
                )
            })
            .expect("可见消息必须登记")
            .rect
    }

    /// 帧序号只在**成功发布**时推进；几何不自洽的帧一律不发布。
    #[test]
    fn frame_publisher_advances_id_only_on_valid_publish() {
        let blank = Buffer::empty(Rect::new(0, 0, 20, 10));
        let mut frames = FramePublisher::default();
        assert!(frames.route().is_none());
        assert!(frames.resolve(0, 0, MouseButton::Left).is_none());

        let builder = frames.begin(ScreenRoute::Agent, Size::new(20, 10), 0);
        frames
            .publish(builder.finish(), &blank, HitProbe::Off, &ScreenRoute::Agent)
            .expect("空 HitMap 是合法帧");
        assert_eq!(frames.route(), Some(&ScreenRoute::Agent));
        assert_eq!(frames.next_frame_id, FrameId::new(1));

        frames.invalidate();
        assert!(frames.route().is_none());
        assert_eq!(frames.next_frame_id, FrameId::new(1), "失效不推进帧序号");

        // 空 rect 的帧：validate 必须拦下，且不能推进序号。
        let mut builder = frames.begin(ScreenRoute::Agent, Size::new(20, 10), 0);
        builder.push(HitRegion::new(
            Rect::ZERO,
            Rect::new(0, 0, 20, 10),
            Rect::ZERO,
            PointerTarget::Agent(AgentTarget::BackToLatest),
            MouseButton::Left,
            true,
            z::AGENT_OVERLAY,
            VisualAnchor::non_empty(Position::new(0, 0)),
        ));
        assert!(
            frames
                .publish(builder.finish(), &blank, HitProbe::Off, &ScreenRoute::Agent)
                .is_err(),
            "空 rect 的帧不得发布"
        );
        assert!(frames.route().is_none());
        assert_eq!(frames.next_frame_id, FrameId::new(1));

        // strict 探针下，锚点落在空白 buffer 上必须失败。
        let mut builder = frames.begin(ScreenRoute::Agent, Size::new(20, 10), 0);
        builder.push(HitRegion::new(
            Rect::new(0, 0, 4, 1),
            Rect::new(0, 0, 20, 10),
            Rect::new(0, 0, 4, 1),
            PointerTarget::Agent(AgentTarget::BackToLatest),
            MouseButton::Left,
            true,
            z::AGENT_OVERLAY,
            VisualAnchor::non_empty(Position::new(0, 0)),
        ));
        let failures = frames
            .publish(
                builder.finish(),
                &blank,
                HitProbe::Strict,
                &ScreenRoute::Agent,
            )
            .expect_err("空白 buffer 上的锚点必须被探针抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "anchor_missing"),
            "{failures:?}"
        );
        assert!(frames.route().is_none(), "探针失败的帧不得发布");
    }

    /// P0-C 的硬回归锁：同一批里前面那条键切了路由，后面那条鼠标必须被丢掉，
    /// 不能拿旧帧坐标去解释新画面（spec §3.3）。
    #[tokio::test]
    async fn batch_key_route_change_drops_following_mouse() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        assert_eq!(frames.route(), Some(&ScreenRoute::Agent));
        let message = message_rect(&frames);

        // 批次第一条：Ctrl+L 打开会话列表（Agent → Workspace）
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        handle_message(&mut app, AppMsg::Key(key), &mut frames, &mut view);
        assert!(
            matches!(route::resolve(&app), ScreenRoute::Workspace(_)),
            "Ctrl+L 应打开 Workspace"
        );
        assert!(frames.route().is_none(), "键之后已发布帧必须失效");

        // 批次第二条：同一坐标上的鼠标按下不得再解释成旧帧的消息行
        let mouse = left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            message.x + 2,
            message.y,
        );
        handle_message(&mut app, AppMsg::Mouse(mouse), &mut frames, &mut view);
        assert!(view.menu.is_none(), "旧帧坐标不得打开消息菜单");
    }

    /// 滚动会改变画面：旧帧立即失效，后续鼠标等下一次重绘。
    #[test]
    fn scroll_mouse_event_invalidates_the_frame() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .scroll
            .follow = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let before = app.active_session().expect("session").scroll.offset;

        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(MouseEventKind::ScrollUp, 5, 5)),
            &mut frames,
            &mut view,
        );

        assert!(frames.route().is_none(), "滚动之后旧帧必须失效");
        assert!(
            app.active_session().expect("session").scroll.offset > before,
            "滚动应真的发生"
        );
    }

    /// 点消息 → 开菜单 → 点菜单行 → 语义动作：整条链路都走已发布帧。
    #[test]
    fn agent_message_click_opens_menu_and_menu_row_activates() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);

        let user_message = frames
            .current()
            .expect("已发布帧")
            .regions
            .iter()
            .find(|region| {
                matches!(
                    &region.target,
                    PointerTarget::Agent(AgentTarget::Message {
                        role: MessageRole::User,
                        ..
                    })
                )
            })
            .expect("用户消息必须登记")
            .rect;

        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                user_message.x + 2,
                user_message.y,
            )),
            &mut frames,
            &mut view,
        );
        assert!(view.menu.is_some(), "点用户消息应打开消息菜单");
        assert!(frames.route().is_none(), "开菜单后旧帧必须失效");

        // 菜单已经画进下一帧，用新帧里的行坐标点击。
        frames = publish_frame(&app, &mut view, 80, 24);
        let undo = frames
            .current()
            .expect("已发布帧")
            .regions
            .iter()
            .find(|region| {
                region.target
                    == PointerTarget::Agent(AgentTarget::MenuAction(MessageAction::UndoFromHere))
            })
            .expect("撤销行必须登记")
            .rect;

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, undo.x + 2, undo.y)),
                &mut frames,
                &mut view,
            );
        }

        assert!(view.menu.is_none(), "激活菜单行后菜单应关闭");
        assert!(
            matches!(app.overlays.last(), Some(Overlay::Confirm { .. })),
            "撤销应从命中路径走到二次确认"
        );
        assert!(frames.route().is_none(), "动作改变画面后旧帧必须失效");
    }

    /// strict 探针在三条路由的真实帧上都必须通过（`publish_frame` 固定用 Strict）。
    #[test]
    fn strict_probe_passes_for_workspace_frames() {
        let (mut app, _rx) = App::new_for_test();
        app.session_list_cache = (0..4)
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
        app.overlays.push(Overlay::SessionList {
            selected: 0,
            show_archived: false,
        });
        let mut view = FullscreenView::default();

        let frames = publish_frame(&app, &mut view, 100, 20);

        assert!(
            matches!(frames.route(), Some(ScreenRoute::Workspace(_))),
            "会话列表必须走 Workspace 路由"
        );
        assert!(
            frames.current().expect("已发布帧").regions.iter().any(
                |region| region.target == PointerTarget::Workspace(WorkspaceHit::SessionRow(0))
            ),
            "会话行必须登记"
        );
    }

    /// 即使一张"错帧"里混进了底层 Workspace 目标，Modal 路由也不得把它 dispatch
    /// 出去（不可点穿透）。
    #[tokio::test]
    async fn modal_dispatch_ignores_underlying_targets() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .pending_permissions
            .push(permission_panel());
        let mut view = FullscreenView::default();

        let route = route::resolve(&app);
        assert!(matches!(route, ScreenRoute::Modal(_)));
        let mut frames = FramePublisher::default();
        let mut builder = frames.begin(route.clone(), Size::new(20, 6), 0);
        builder.push(HitRegion::new(
            Rect::new(0, 0, 5, 1),
            Rect::new(0, 0, 20, 6),
            Rect::new(0, 0, 5, 1),
            PointerTarget::Workspace(WorkspaceHit::Back),
            MouseButton::Left,
            true,
            z::WORKSPACE_ROW,
            VisualAnchor::non_empty(Position::new(0, 0)),
        ));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 6));
        buffer[(0, 0)].set_symbol("x");
        frames
            .publish(builder.finish(), &buffer, HitProbe::Off, &route)
            .expect("手工帧是合法的");

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, 0, 0)),
                &mut frames,
                &mut view,
            );
        }

        assert!(
            app.workspace_hover.is_none() && app.workspace_pressed.is_none(),
            "Modal 路由不得把点击穿透给 Workspace 目标"
        );
        assert!(
            app.active_session()
                .expect("session")
                .active_permission()
                .is_some(),
            "permission 不该被穿透的点击应答"
        );
    }

    /// 焦点丢失 = 合成 Leave：hover/pressed 全部作废，但屏幕没变，帧仍可信。
    #[test]
    fn focus_lost_clears_pointer_state() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.modal_hover = Some(ModalHit::PermissionApprove);
        app.workspace_hover = Some(WorkspaceHit::Back);
        app.workspace_pressed = Some(WorkspaceHit::Back);
        let mut view = FullscreenView {
            pointer: FullscreenState {
                back_to_latest_hover: true,
                back_to_latest_pressed: true,
            },
            menu: Some(MessageMenu::new(
                "turn-1".into(),
                "b1".into(),
                MessageRole::Assistant,
                Position::new(1, 1),
            )),
            ..Default::default()
        };
        if let Some(menu) = view.menu.as_mut() {
            menu.hover = Some(0);
            menu.pressed = Some(0);
        }
        let mut frames = publish_frame(&app, &mut view, 80, 24);

        handle_message(&mut app, AppMsg::FocusLost, &mut frames, &mut view);

        assert!(app.modal_hover.is_none() && app.modal_pressed.is_none());
        assert!(app.workspace_hover.is_none() && app.workspace_pressed.is_none());
        assert!(!view.pointer.back_to_latest_hover);
        assert!(!view.pointer.back_to_latest_pressed);
        let menu = view.menu.as_ref().expect("焦点丢失不该关菜单");
        assert!(menu.hover.is_none() && menu.pressed.is_none());
        assert!(frames.route().is_some(), "焦点丢失不改画面，帧仍可信");
    }

    /// Modal 关闭后，旧按钮坐标不能再触发任何动作（不可点穿透）。
    #[tokio::test]
    async fn modal_close_makes_old_button_coordinates_inert() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.sessions
            .get_mut("seed-1")
            .expect("session")
            .pending_permissions
            .push(permission_panel());
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        assert!(matches!(frames.route(), Some(ScreenRoute::Modal(_))));

        let approve = frames
            .current()
            .expect("已发布帧")
            .regions
            .iter()
            .find(|region| region.target == PointerTarget::Modal(ModalHit::PermissionApprove))
            .expect("批准按钮必须登记")
            .rect;

        // Down + Up 落在同一个按钮上 → 复用键盘的应答路径，modal 下架。
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, approve.x + 1, approve.y)),
                &mut frames,
                &mut view,
            );
        }
        assert!(
            app.active_session()
                .expect("session")
                .active_permission()
                .is_none(),
            "批准应答应下架 modal"
        );
        assert!(frames.route().is_none(), "modal 关闭后旧帧必须失效");

        // 同一坐标再来一次：没有已发布帧，必须完全无效。
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, approve.x + 1, approve.y)),
                &mut frames,
                &mut view,
            );
        }
        assert_eq!(app.modal_pressed, None, "旧 modal 坐标不得再进入 pressed");
        assert!(frames.route().is_none());
    }
}
