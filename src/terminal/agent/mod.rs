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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::session::SessionState;
use crate::app::{App, AppMsg, ConnPhase, ModalHit, StartupIntent, WorkspaceHit};
use crate::runtime::{Runtime, RuntimeMsg};
use crate::theme::Theme;
use crate::ui::v2::hit::{
    AgentTarget, FrameHitMap, FrameId, HitMapBuilder, HitProbe, PointerTarget, ProbeFailure,
    ScrollbarPart,
};
use crate::ui::v2::modal;
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
    // B2 绘制门控：上一轮迭代是否处理过消息。Tick 不是消息——纯 tick 醒来
    // 且没有动画/活动在跑时，本轮跳过绘制（见 `App::needs_draw`）。
    let mut processed_messages = true;
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
        // 首轮必画（冷启动画面），此后按门控判定。
        if processed_messages || app.needs_draw() {
            draw_and_publish(
                terminal,
                &mut frames,
                app,
                theme,
                &route,
                fullscreen_view,
                probe,
            )?;
            app.last_drawn_frame = crate::app::anim::frame_now();
        }
        if route == ScreenRoute::Agent {
            fullscreen_view.clamp_scroll(app);
        }

        let Some(msg) = app_rx.recv().await else {
            break;
        };
        handle_message(app, msg, &mut frames, fullscreen_view);
        processed_messages = true;
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
    let load_older = PointerTarget::Agent(AgentTarget::LoadOlder);
    fullscreen_view.pointer.load_older_hover = visual.hovered.as_ref() == Some(&load_older);
    fullscreen_view.pointer.load_older_pressed = visual.pressed.as_ref() == Some(&load_older);

    let sidebar_index = |target: &PointerTarget| match target {
        PointerTarget::Agent(AgentTarget::SidebarRow(index)) => Some(*index),
        _ => None,
    };
    fullscreen_view.pointer.sidebar_hover = visual.hovered.as_ref().and_then(sidebar_index);
    fullscreen_view.pointer.sidebar_pressed = visual.pressed.as_ref().and_then(sidebar_index);

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
            PointerTarget::Agent(AgentTarget::LoadOlder) => {
                app.load_older();
                frames.invalidate();
            }
            PointerTarget::Agent(AgentTarget::SidebarRow(index)) => {
                app.sidebar_open(index);
                fullscreen_view.pointer_state.clear();
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
            PointerTarget::Agent(AgentTarget::Subagents) => {
                app.cycle_subagent();
                frames.invalidate();
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
                // 卡片悬浮在对话上，背景不可点也不该跟着滚：滚轮直接滚动
                // 卡片内容（Grok 的 blocking card 同款语义）。
                ScreenRoute::Modal(modal_route) => {
                    const WHEEL_LINES: usize = 3;
                    if up {
                        modal_wheel_scroll_up(app, *modal_route, WHEEL_LINES);
                    } else {
                        app.modal_wheel_scroll(*modal_route, WHEEL_LINES);
                    }
                }
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
        view.transcripts.len_for(&session.session_id),
        usize::from(view.body_height),
        session.scroll.follow,
        session.scroll.offset,
    )
}

fn set_scroll_offset(app: &mut App, view: &FullscreenView, offset: usize) {
    let max = view.max_offset(app);
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
        | route::WorkspaceRoute::History { detail: false, .. }
        | route::WorkspaceRoute::Subagents { .. } => {
            app.workspace_move_selection(if up { -1 } else { 1 });
        }
        // 设置卡片：滚轮直接滚视口（内容在卡片里，会话视口没有意义）。
        route::WorkspaceRoute::Settings => app.settings_scroll(up, 3),
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

/// 滚轮向上滚卡片：与 `App::modal_wheel_scroll` 的向下方向对称（面板的
/// scroll 字段是 usize/u16 饱和类型，向上滚就是 saturating_sub）。
fn modal_wheel_scroll_up(app: &mut App, route: ModalRoute, lines: usize) {
    let session_id = match app.active_session_id() {
        Some(id) => id,
        None => return,
    };
    let session = match app.sessions.get_mut(&session_id) {
        Some(session) => session,
        None => return,
    };
    match route {
        ModalRoute::Permission => {
            if let Some(panel) = session.pending_permissions.first_mut() {
                panel.scroll = panel.scroll.saturating_sub(lines);
            }
        }
        ModalRoute::Ask => {
            if let Some(panel) = session.pending_ask.as_mut() {
                panel.scroll = panel.scroll.saturating_sub(lines as u16);
            }
        }
        ModalRoute::Plan => {
            if let Some(panel) = session.pending_plan.as_mut() {
                panel.scroll = panel.scroll.saturating_sub(lines);
            }
        }
        ModalRoute::Confirm
        | ModalRoute::AttachPath
        | ModalRoute::CwdInput
        | ModalRoute::Thinking => {}
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
    ///
    /// 正文先剥离全部转义序列再落盘（审计 F2）：`less -R` 直通 SGR、`sh`
    /// 缺失时的 `cat` 回退原样输出，模型 thinking 里的 OSC/CSI 序列会被
    /// 真实终端解释。临时文件走独占创建（CWE-377），不覆盖既有文件/符号链接。
    fn run_pager(&mut self, text: &str) -> Result<()> {
        let body = crate::app::pager::sanitize_body(text);
        let path = match crate::app::pager::write_temp_file(&std::env::temp_dir(), &body) {
            Ok(path) => path,
            // 与旧实现一致：写不出来就跳过 pager，不中断主循环。
            Err(_) => return Ok(()),
        };

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
        // 阻塞弹窗与设置卡片不再整屏清空：agent 视图继续做背景（Codex/Grok 的
        // 「对话始终可见」），上面压一层 DIM 遮罩，卡片居中悬浮。
        // 背景层不登记任何命中区（被卡片覆盖的锚点过不了 strict probe），
        // 所以背景绘制期间 builder 挂起，画完再解除、由卡片自己登记。
        ScreenRoute::Modal(modal_route) => {
            draw_fullscreen_agent(frame, app, theme, fullscreen_view, hit_map.suspended());
            hit_map.set_suspended(false);
            dim_screen(frame, area, theme);
            modal::draw(frame, app, area, theme, *modal_route, hit_map);
        }
        ScreenRoute::Workspace(route::WorkspaceRoute::Settings) => {
            draw_fullscreen_agent(frame, app, theme, fullscreen_view, hit_map.suspended());
            hit_map.set_suspended(false);
            dim_screen(frame, area, theme);
            workspace::draw_settings_card(frame, app, area, theme, hit_map);
        }
        ScreenRoute::Workspace(workspace_route) => {
            clear_screen(frame, theme);
            workspace::draw(frame, app, workspace_route, theme, hit_map);
        }
    }
}

/// 阻塞卡片下的整屏压暗层：前景统一压到 muted 并加 DIM，`Reset` 背景保持
/// 原样（`terminal` 主题没有底色，塞 `Color::Reset` 会把颜色信息整个抹掉）。
/// `Frame` 的光标状态不在这里碰——背景绘制已跳过 `set_cursor_position`，
/// 前景卡片自己决定光标。
fn dim_screen(frame: &mut Frame, area: Rect, theme: &Theme) {
    for x in area.x..area.right() {
        for y in area.y..area.bottom() {
            let cell = &mut frame.buffer_mut()[(x, y)];
            if cell.fg != Color::Reset {
                cell.fg = theme.text.muted;
            }
            cell.modifier.insert(Modifier::DIM);
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
    /// 子代理预览条的命中矩形（**相对本块左上角**，高度恒为 1）；`None` = 这一帧
    /// 没画。渲染与命中登记共用这一份几何——各算一次就迟早错位。
    subagent_strip: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentLayout {
    live_rows: usize,
    slash_rows: usize,
    stream_rows: usize,
    thinking_rows: usize,
    composer_rows: usize,
    /// 状态行（已并入快捷键提示）。
    status_rows: usize,
    /// 子代理预览条（0 = 没有运行中的子代理）。
    subagent_rows: usize,
    /// sticky 待办面板占的行数（0 = 不显示）。
    todo_rows: usize,
}

impl AgentLayout {
    fn height(self) -> usize {
        self.live_rows
            .saturating_add(self.slash_rows)
            .saturating_add(self.stream_rows)
            .saturating_add(self.thinking_rows)
            .saturating_add(self.composer_rows)
            .saturating_add(self.status_rows)
            .saturating_add(self.subagent_rows)
            .saturating_add(self.todo_rows)
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
    // 常驻可选项：**放得下才画**（额度在末尾统一分配）。底部两行合并成一行之后，
    // 这些项和快捷键提示抢同一行的空间，所以不再按终端宽度拍几个固定门槛——
    // 门槛看不出 `cwd` 比 `model` 长多少，合并行又没有第二行可以溢出。
    let mut optional: Vec<Span<'static>> = Vec::new();
    if let Some(session) = app.active_session() {
        spans.push(Span::styled(
            format!(" · {}", session.activity_label()),
            Style::new().fg(theme.text.secondary),
        ));
        if toast.is_none() {
            if let Some(model) = session.display_model() {
                optional.push(Span::styled(
                    format!(" · {model}"),
                    Style::new().fg(theme.text.dim),
                ));
            }
            optional.push(Span::styled(
                format!(
                    " · {}",
                    match session.mode {
                        ConversationMode::Plan => "plan",
                        ConversationMode::Code => "code",
                    }
                ),
                Style::new().fg(theme.text.dim),
            ));
            for span in usage_segments(session, theme) {
                optional.push(span);
            }
            // cwd 排在使用量之后：三项指标比"我在哪个目录"更需要看见（目录在
            // workspace 面板里另有出处）。
            if let Some(cwd) = app.effective_cwd(None) {
                optional.push(Span::styled(
                    format!(" · {}", crate::app::truncate_str(&cwd, 28)),
                    Style::new().fg(theme.text.dim),
                ));
            }
            if !session.composer.attachments.is_empty() {
                optional.push(Span::styled(
                    format!(" · ✎{}", session.composer.attachments.len()),
                    Style::new().fg(theme.semantic.warning),
                ));
            }
        }
    }
    // 组装：必需项（连接相位 + 活动状态）与行尾时钟是底线，快捷键提示先占额度
    // （最多吃半行），剩下的才轮到 model / mode / cwd / usage 这些常驻项——放不下
    // 就整块丢弃，不截半个词。连接诊断 / toast 出现时提示整块让路：它们比提示
    // 重要，而且各自都有按剩余宽度算的截断额度。
    let clock = Span::styled(
        format!(" {}", chrono::Local::now().format("%H:%M")),
        Style::new().fg(theme.text.dim),
    );
    let used =
        |spans: &[Span<'static>]| -> usize { spans.iter().map(|span| span.content.width()).sum() };
    let urgent = toast.is_some() || app.conn_error.is_some();
    let room = (width as usize).saturating_sub(used(&spans) + clock.content.width());
    let hint = (!urgent).then(|| shortcuts_hint(app, room)).flatten();
    let mut budget = room.saturating_sub(hint.map_or(0, |hint| hint.width() + 1));
    for span in optional {
        let span_width = span.content.width();
        if span_width <= budget {
            budget -= span_width;
            spans.push(span);
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
        let rest = (width as usize).saturating_sub(used(&spans) + clock.content.width());
        let budget = rest.saturating_sub(8).max(28);
        spans.push(Span::styled(
            format!(" · {}", crate::app::truncate_str(error, budget)),
            Style::new().fg(theme.semantic.warning),
        ));
    }
    if let Some(toast) = toast {
        let color = match toast.level {
            NoticeLevel::Info => theme.text.secondary,
            NoticeLevel::Warn => theme.semantic.warning,
            NoticeLevel::Error => theme.accent.error,
        };
        // 宽度感知的截断（理由同诊断）：至少给 24 列，避免窄屏时把提示压成一个词。
        let rest = (width as usize).saturating_sub(used(&spans) + clock.content.width());
        let budget = rest.saturating_sub(8).max(24);
        spans.push(Span::styled(
            format!(" · {}", crate::app::truncate_str(&toast.text, budget)),
            Style::new().fg(color),
        ));
    }
    // 提示右对齐到时钟**之前**：时钟保持行尾锚点。
    if let Some(hint) = hint {
        let rest = (width as usize).saturating_sub(used(&spans) + clock.content.width());
        let gap = rest.saturating_sub(hint.width());
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap), Style::new()));
            spans.push(Span::styled(hint, Style::new().fg(theme.text.dim)));
        }
    }
    spans.push(clock);
    Line::from(spans)
}

/// 用量三件套（v1 回归）：`↑12k ↓3k (6%) · 41 tok/s · cache 87%`。
///
/// 每一项都**只在数据存在时**出现——三个来源的可得性各不相同：
/// - **上下文占比**要 `context_limit`（daemon 没给就不猜比例）；
/// - **输出速率**要两端权威墙钟（`TurnStarted` / `TurnFinished` 的信封时间）；
/// - **缓存命中率**要 `cache_usage_reported == Some(true)`：真 0% 与「没上报」
///   必须可区分（协议里专门留了这个字段），不能把不上报画成 0%。
fn usage_segments(session: &SessionState, theme: &Theme) -> Vec<Span<'static>> {
    let dim = Style::new().fg(theme.text.dim);
    let Some(usage) = session.usage.as_ref() else {
        return Vec::new();
    };
    let mut spans = vec![Span::styled(
        format!(
            " · ↑{}k ↓{}k",
            usage.prompt_tokens / 1000,
            usage.completion_tokens / 1000
        ),
        dim,
    )];
    // 上下文占用：`prompt_tokens` 是本次请求的输入（≈ 当前上下文），除以 daemon
    // 给的窗口上限。v1 就是 `↑xk ↓yk (pct%)`，v2 移植时把手动百分比弄丢了。
    if let Some(limit) = session.context_limit.filter(|limit| *limit > 0) {
        let pct = (u64::from(usage.prompt_tokens) * 100 / u64::from(limit)).min(999);
        spans.push(Span::styled(format!(" ({pct}%)"), dim));
    }
    if let Some(rate) = token_rate(session) {
        spans.push(Span::styled(format!(" · {rate:.0} tok/s"), dim));
    }
    // 缓存命中率取**会话累计**（`usage_totals`）：单次请求的命中率抖动过大。
    if let Some(totals) = session.usage_totals.as_ref()
        && totals.cache_usage_reported == Some(true)
    {
        let hit = u64::from(totals.prompt_cache_hit_tokens);
        let miss = u64::from(totals.prompt_cache_miss_tokens);
        // `checked_div` 顺手把 `hit + miss == 0`（没有可比的请求）挡掉。
        if let Some(pct) = (hit * 100).checked_div(hit + miss) {
            spans.push(Span::styled(format!(" · cache {pct}%"), dim));
        }
    }
    spans
}

/// 输出速率（tok/s）= 本次请求的 `completion_tokens` / 回合墙钟时长。
///
/// 只在**回合已结束**时给数：流式途中 `usage` 还是上一轮请求的结果，拿它配本轮
/// 已用时长会得到一个虚高且随时间衰减的假数。两端都取信封 `ts_ms`（同源），不用
/// 本地时钟兜底——本仓对「猜出来的时间」一贯是这个纪律。
///
/// 多轮回合（中间夹工具调用）分母含工具执行时间，所以这是**端到端**速率：只会
/// 比模型瞬时生成速度低，不会虚高。
fn token_rate(session: &SessionState) -> Option<f64> {
    if session.streaming.is_some() {
        return None;
    }
    let (turn_id, finished_at_ms) = session.last_turn_finished.as_ref()?;
    let usage = session.usage.as_ref()?;
    if usage.completion_tokens == 0 {
        return None;
    }
    let started_at_ms = session.timeline.turn_started_at_ms(turn_id)?;
    let elapsed_ms = finished_at_ms.saturating_sub(started_at_ms);
    // 亚秒回合除出来全是噪声。
    if elapsed_ms < 500 {
        return None;
    }
    Some(f64::from(usage.completion_tokens) * 1000.0 / elapsed_ms as f64)
}

/// 底部快捷键提示的两个版本（完整 / 紧凑）。
///
/// 合并进状态行之后，给它的宽度是**算出来的剩余列**而不是固定的终端宽度阈值，
/// 所以这里只产出候选文本，选哪一个由 [`shortcuts_hint`] 按剩余空间决定。
fn shortcuts_variants(app: &App) -> (&'static str, &'static str) {
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
        "Enter 应答 · Esc 取消 · F1 帮助"
    } else if slash_open {
        "↑↓ 选择 · Tab/Enter 补全 · Esc 关闭 · F1 帮助"
    } else if streaming {
        "Esc 中止 · Ctrl+Y 撤销 · Ctrl+E 压缩 · F1 帮助"
    } else if has_attachment {
        "Enter 发送 · Ctrl+A 附件 · Ctrl+Y 撤销 · F1 帮助"
    } else if app.active_session().is_some() {
        "Enter 发送 · Alt+T 展开思考 · Alt+E 展开工具 · Ctrl+P 模式 · F1 帮助"
    } else {
        "Ctrl+N 新建 · Ctrl+L 会话 · F1 帮助 · Ctrl+Q 退出"
    };
    let compact = if waiting {
        "Enter 应答 · Esc 取消"
    } else if slash_open {
        "↑↓ 选择 · Enter 补全"
    } else if streaming {
        "Esc 中止 · Ctrl+E 压缩"
    } else if has_attachment {
        "Enter 发送 · Ctrl+A 附件"
    } else if app.active_session().is_some() {
        "Enter 发送 · Alt+Enter 换行"
    } else {
        "Ctrl+N 新建 · Ctrl+L 会话"
    };
    (text, compact)
}

/// 这一行还能给的列数 → 画哪一版快捷键提示（都放不下就不画）。
///
/// 优先**紧凑版**：完整版约 60 列，在 80~120 列的终端上会把 model / mode / cwd /
/// usage 全挤掉——那些是状态，提示只是提示。只有剩余空间明显富余（完整版 +
/// 一屏常驻项）时才升级到完整版。提示是不可丢弃的吗？不是：宁可让它消失，也不
/// 让它把 `ready` / 模型 / 上下文挤没。
fn shortcuts_hint(app: &App, room: usize) -> Option<&'static str> {
    /// 升级到完整版需要额外富余的列数（留给常驻项）。
    const FULL_HINT_SLACK: usize = 30;
    let (full, compact) = shortcuts_variants(app);
    if room >= full.width() + FULL_HINT_SLACK {
        Some(full)
    } else if room > compact.width() {
        Some(compact)
    } else {
        None
    }
}

/// 子代理预览条的左缩进（与转录区正文同一档）。
const SUBAGENT_STRIP_INDENT: usize = 2;
/// 方框总宽（含左右竖边）。
///
/// 「大概 15 列」：`子代理 N ›` 加两侧内边距正好收在这个宽度里，再宽就是空白。
const SUBAGENT_STRIP_WIDTH: u16 = 14;

/// 子代理预览条：一个固定宽度的单行方框，**只画运行中的子代理**。
///
/// 子代理是会话内部的协作方，不是顶层会话——`session.list` 不区分父子（daemon
/// 侧 `list_sessions` 原样列出所有会话），所以它此前会作为一条独立会话混进侧栏
/// 对话列表。过滤在前端做（见 `sidebar_rows`），而它的存在感收敛到这一格：
/// 跑着的时候在，done 之后自然消失（数据源就是 roster 的 `Running` 状态，不需要
/// 额外的清理时机）。点它 = `Ctrl+↑`（进入 / 循环子代理视图）。
///
/// 单行方框只画左右两条竖边：上下边在单行里没有位置，画了反而像坏掉的框（与
/// `render_brand` 的多行框不同，那是真的占了三行）。
fn subagent_strip_line(app: &App, width: u16, theme: &Theme) -> Option<Line<'static>> {
    let running = app.running_child_agent_ids();
    if running.is_empty() {
        return None;
    }
    let total = usize::from(SUBAGENT_STRIP_WIDTH);
    if usize::from(width) < SUBAGENT_STRIP_INDENT + total {
        return None;
    }
    let label = format!("子代理 {} ›", running.len());
    let inner = total.saturating_sub(2);
    let label = crate::app::truncate_str(&label, inner.saturating_sub(1).max(1));
    let pad = inner.saturating_sub(1 + label.width());
    Some(Line::from(vec![
        Span::styled(" ".repeat(SUBAGENT_STRIP_INDENT), Style::new()),
        Span::styled("│".to_string(), Style::new().fg(theme.chrome.border)),
        Span::styled(format!(" {label}"), Style::new().fg(theme.text.secondary)),
        Span::styled(" ".repeat(pad), Style::new()),
        Span::styled("│".to_string(), Style::new().fg(theme.chrome.border)),
    ]))
}

/// 子代理预览条的命中宽度（与 [`subagent_strip_line`] 画的框同宽）。
fn subagent_strip_width() -> u16 {
    SUBAGENT_STRIP_WIDTH
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
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
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

    /// 卡片化整帧验收：授权弹窗悬浮在 agent 视图上，背景 + 暗化 + 卡片同帧
    /// 渲染且 HitMap 过 strict probe。回归点：
    /// 1. 背景 transcript 内容仍然可见（不再整屏清空）；
    /// 2. 卡片命中只来自弹窗自身（背景锚点被 Clear 覆盖后 probe 不报错）。
    #[test]
    fn permission_modal_renders_over_agent_background() {
        let (mut app, _rx) = App::new_for_test();
        let session_id = "session-1".to_string();
        let mut session = SessionState::new(session_id.clone());
        session.timeline = model_with_sealed_answer();
        session.pending_permissions.push(PermissionPanel {
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            action_summary: Some("cargo build".into()),
            reason: "构建".into(),
            paths: vec![],
            category: PermissionCategory::Exec,
            level: 2,
            risk: PermissionRisk::High,
            consequence: "会执行本地命令".into(),
            trust_folder: false,
            scroll: 0,
        });
        app.tabs.push(session_id.clone());
        app.sessions.insert(session_id, session);
        let route = route::resolve(&app);
        assert!(
            matches!(route, ScreenRoute::Modal(_)),
            "挂起 permission 必须解析成 Modal 路由"
        );

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = FullscreenView::default();
        let mut hit_map = HitMapBuilder::new(
            FrameId::new(1),
            route.clone(),
            ratatui::layout::Size::new(100, 30),
            0,
        );
        terminal
            .draw(|frame| draw(frame, &app, &test_theme(), &route, &mut view, &mut hit_map))
            .expect("draw modal over agent");
        let map = hit_map.finish();
        assert!(map.validate().is_ok(), "{:?}", map.validate().err());
        map.probe(terminal.backend().buffer(), map.frame_id, &map.route)
            .expect("背景+卡片整帧必须过 strict probe");

        // 背景 transcript（"hello"）与卡片（"工具权限"）同帧可见。
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let flat: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(flat.contains("工具权限"), "卡片必须可见：{flat}");
        assert!(
            flat.contains("hello"),
            "背景 transcript 必须仍然可见：{flat}"
        );
    }

    /// 滚轮滚卡片：Modal 路由下滚轮滚的是面板内容，不是背景 transcript。
    #[test]
    fn wheel_over_modal_scrolls_panel_not_transcript() {
        let (mut app, _rx) = App::new_for_test();
        let session_id = "session-1".to_string();
        let mut session = SessionState::new(session_id.clone());
        session.pending_ask = Some(crate::app::session::AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            qaqh_client::AskMode::Single,
            vec![qaqh_client::DomainAskQuestion {
                id: "q1".into(),
                question: "选哪个？".into(),
                options: vec!["A".into(), "B".into()],
                allow_custom: false,
            }],
        ));
        app.tabs.push(session_id.clone());
        app.sessions.insert(session_id.clone(), session);

        app.modal_wheel_scroll(ModalRoute::Ask, 2);
        let ask_scroll = app.sessions[&session_id]
            .pending_ask
            .as_ref()
            .expect("ask")
            .scroll;
        assert_eq!(ask_scroll, 2, "滚轮向下必须增大面板 scroll");
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
        let session_id = "session-1".to_string();
        let mut session = SessionState::new(session_id.clone());
        session.timeline = model;
        app.tabs.push(session_id.clone());
        app.sessions.insert(session_id, session);
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
                        exit_code: None,
                        completed_at_ms: None,
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

    fn model_with_todo_write() -> TimelineModel {
        let mut model = model_with_sealed_answer();
        model.apply(&entry(
            3,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "todo-1".to_string(),
                    block_order: 1,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Sealed,
                    text: String::new(),
                    tool: Some(qaqh_client::TimelineTool {
                        exit_code: None,
                        completed_at_ms: None,
                        tool_call_id: "call-todo".to_string(),
                        name: "todo_write".to_string(),
                        state: qaqh_client::TimelineToolState::Succeeded,
                        summary: None,
                        args_json: Some(
                            r#"{"items":[
                                {"title":"已完成甲","status":"completed"},
                                {"title":"进行中丙","status":"in_progress"},
                                {"title":"待办丁","status":"pending"}
                            ]}"#
                            .to_string(),
                        ),
                        output: None,
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

    /// sticky 面板贴在输入带上沿，进行中的那项在第一行。
    ///
    /// 面板吃的是**转录区**的行：色带位置不变（19..=21），面板挤在它上面。
    #[test]
    fn todo_panel_sits_above_the_composer_band() {
        let mut app = app_with_model(model_with_todo_write());
        app.show_workspace = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let band = test_theme().chrome.composer_bg;
        assert_eq!(
            composer_band_rows(&terminal, 80, 24, band),
            vec![20, 21, 22],
            "面板不该顶掉输入带"
        );

        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..80u16)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<String>()
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect()
        };
        // 面板 4 行（标题 + 3 项）落在转录区末尾，紧贴色带。
        assert!(row(16).contains("待办·1/3完成"), "{}", row(16));
        assert!(row(17).contains("进行中丙"), "{}", row(17));
        assert!(row(18).contains("待办丁"), "{}", row(18));
        assert!(row(19).contains("已完成甲"), "{}", row(19));
    }

    fn model_with_consecutive_reads() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-reads",
            TimelineEvent::TurnOpened {
                user_text: "读三个文件".to_string(),
            },
        ));
        for (index, path) in ["src/a.rs", "src/b.rs", "src/c.rs"].iter().enumerate() {
            model.apply(&entry(
                (index + 2) as u64,
                "turn-reads",
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_id: format!("read-{}", index + 1),
                        block_order: index as u32,
                        kind: TimelineBlockKind::Tool,
                        state: TimelineBlockState::Sealed,
                        text: String::new(),
                        tool: Some(qaqh_client::TimelineTool {
                            exit_code: None,
                            completed_at_ms: None,
                            tool_call_id: format!("call-{}", index + 1),
                            name: "read".to_string(),
                            state: qaqh_client::TimelineToolState::Succeeded,
                            summary: None,
                            args_json: None,
                            output: Some(format!("L1: {path}")),
                            diff: None,
                            progress: String::new(),
                            progress_truncated: false,
                            progress_stream: None,
                            progress_bytes_total: 0,
                            display: Some(qaqh_client::TimelineToolDisplay {
                                summary: None,
                                diff: None,
                                lines_added: 0,
                                lines_removed: 0,
                                header: Some(qaqh_client::TimelineToolHeader::Path {
                                    path: path.to_string(),
                                    op: qaqh_client::TimelinePathOp::Read,
                                }),
                                body: None,
                                metrics: None,
                                outcome: None,
                            }),
                            failure: None,
                            permission: None,
                        }),
                    },
                },
            ));
        }
        model
    }

    /// 全屏侧：连续 read 合成**一张**卡，命中区域也只有一块（指向首成员）。
    #[test]
    fn fullscreen_collapses_consecutive_lookups_into_one_card() {
        let app = app_with_model(model_with_consecutive_reads());
        let theme = test_theme();
        let mut cache = FullscreenTranscriptCache::default();
        cache.sync(&app.sessions["session-1"], 79, &theme);

        let text: Vec<String> = cache
            .rendered_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("Read 3 files"), "{joined}");
        assert!(
            !joined.contains("Read src/a.rs"),
            "合并后不再逐张画头：{joined}"
        );
        assert!(joined.contains("src/a.rs"), "{joined}");

        let tool_spans = cache.tool_spans();
        assert_eq!(tool_spans.len(), 1, "三张卡只留一个命中区域");
        assert_eq!(tool_spans[0].0, "read-1", "命中区域指向首成员");
        assert_eq!(tool_spans[0].2 - tool_spans[0].1, 4, "1 行头 + 3 行清单");
    }

    /// 展开首成员即拆组：`expanded_tools` 里放 read-1，卡片回到逐张画。
    #[test]
    fn fullscreen_group_splits_once_a_member_is_expanded() {
        let mut app = app_with_model(model_with_consecutive_reads());
        let session = app.sessions.get_mut("session-1").expect("session");
        assert!(session.toggle_tool_expanded("read-1"));
        let theme = test_theme();
        let mut cache = FullscreenTranscriptCache::default();
        cache.sync(&app.sessions["session-1"], 79, &theme);

        let joined: String = cache
            .rendered_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Read src/a.rs"), "{joined}");
        assert!(
            joined.contains("Read 2 files"),
            "其余连续项照常合并：{joined}"
        );
    }

    /// 回归（长会话 spinner 卡死）：窗口里留下一个**终态条目丢失**的旧 running
    /// 回合时，「正在工作」必须仍然为假——否则输入框上方会常驻一行思考动画、
    /// 状态栏常驻 `answering · r0`。
    #[test]
    fn ghost_running_turn_does_not_keep_the_session_working() {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "第一轮".into(),
            },
        ));
        model.apply(&entry(
            2,
            "t2",
            TimelineEvent::TurnOpened {
                user_text: "第二轮".into(),
            },
        ));
        // t1 的 TurnSealed 丢失（旧实现里它会一直留在窗口当幽灵）；t2 正常封口。
        model.apply(&entry(
            3,
            "t2",
            TimelineEvent::TurnSealed {
                state: qaqh_client::TimelineTurnState::Completed,
                failure: None,
            },
        ));

        let mut session = SessionState::new("s".into());
        session.timeline = model;
        assert!(
            session.timeline.turns[0].is_streaming(),
            "夹具前提：t1 仍是幽灵"
        );
        assert!(
            !session_is_working(&session),
            "幽灵不得让 spinner 常驻：{}",
            session.activity_label()
        );
    }

    /// root（会话）+ 一个指定状态的子代理：预览条的数据源是 **roster status**。
    fn subagent_team_snapshot(status: &str) -> qaqh_client::ClientV2TeamSnapshot {
        serde_json::from_value(serde_json::json!({
            "root_session_id": "session-1",
            "agents": [
                {
                    "agent_id": "session-1",
                    "agent_path": "/root",
                    "role": "root",
                    "status": "running",
                    "residency": "loaded"
                },
                {
                    "agent_id": "child",
                    "agent_path": "/root/child",
                    "nickname": "child",
                    "status": status,
                    "residency": "loaded",
                    "parent_agent_path": "/root"
                }
            ],
            "unread_messages": [],
            "revision": 1,
            "last_fact_seq": 1
        }))
        .expect("team snapshot")
    }

    fn subagent_running_app() -> App {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.teams
            .entry("session-1".into())
            .or_default()
            .replace_from_snapshot(subagent_team_snapshot("running"));
        app
    }

    fn drawn_row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        // 读**整行**：全屏布局左侧还有会话栏，写死 0..80 会把右侧右对齐的
        // 内容（快捷键提示）切掉。
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect()
    }

    /// 子代理预览条：**跑着的时候**占一行，done 之后连那一行一起消失。
    ///
    /// 行为契约来自数据源本身（roster 的 `Running`），没有额外的清理时机——
    /// 「下一个对话开始时置空」这种时序约束因此不需要额外实现。
    #[test]
    fn subagent_strip_appears_only_while_a_subagent_runs() {
        let mut app = subagent_running_app();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();
        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let band = test_theme().chrome.composer_bg;
        assert_eq!(
            composer_band_rows(&terminal, 80, 24, band),
            vec![19, 20, 21],
            "预览条占走底部一行，输入带上移"
        );
        // 宽字符的续格在 buffer 里是空格（ratatui 的 `Cell::default()`），比对前先
        // 去掉空白——与 `fullscreen_agent_draw_survives_resize` 同一套路。
        let last_row = |terminal: &Terminal<TestBackend>| -> String {
            drawn_row(terminal, 23)
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect()
        };
        assert!(
            last_row(&terminal).contains("子代理1"),
            "预览条贴在最后一行：{:?}",
            drawn_row(&terminal, 23)
        );

        // 子代理 done → 预览条消失，输入带落回最下面。
        app.teams
            .get_mut("session-1")
            .expect("team")
            .replace_from_snapshot(subagent_team_snapshot("completed"));
        let mut view = FullscreenView::default();
        draw_agent_frame(&mut terminal, &app, &route, &mut view);
        assert_eq!(
            composer_band_rows(&terminal, 80, 24, band),
            vec![20, 21, 22]
        );
        assert!(
            !last_row(&terminal).contains("子代理"),
            "done 之后预览条必须消失：{:?}",
            drawn_row(&terminal, 23)
        );
    }

    /// 底部两行（状态 + 快捷键）合并成**一行**：省下的那一行留给子代理预览条。
    ///
    /// 没有子代理在跑时，这一行就是整个底部行——比原来少占一行。
    #[test]
    fn status_row_carries_status_and_shortcuts_on_one_line() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.conn_phase = ConnPhase::Ready;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();
        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let compact: String = drawn_row(&terminal, 23)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(compact.contains("ready"), "状态仍在：{compact:?}");
        assert!(
            compact.contains("Enter发送"),
            "快捷键提示同排右对齐：{compact:?}"
        );
    }

    /// 预览条是**按钮**：点它 = `Ctrl+↑`，进入子代理视图。
    #[tokio::test]
    async fn subagent_strip_click_enters_the_child_view() {
        let mut app = subagent_running_app();
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let rect = frames
            .current()
            .expect("published frame")
            .regions
            .iter()
            .find_map(|region| match &region.target {
                PointerTarget::Agent(AgentTarget::Subagents) => Some(region.rect),
                _ => None,
            })
            .expect("预览条必须登记命中矩形");
        assert_eq!(rect.height, 1, "预览条是单行方框");

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, rect.x + 1, rect.y)),
                &mut frames,
                &mut view,
            );
        }
        assert_eq!(
            app.inspect.as_deref(),
            Some("child"),
            "点击预览条要进入子代理视图"
        );
    }

    /// 用量三件套（v1 回归）：上下文占比 / 输出速率 / 缓存命中率。
    #[test]
    fn usage_segments_render_context_rate_and_cache_hit() {
        let theme = test_theme();
        let mut session = SessionState::new("s".into());
        // 无 usage → 一项都不画。
        assert!(
            usage_segments(&session, &theme).is_empty(),
            "无 usage 就不画"
        );

        session.usage = Some(usage_info(12_000, 3_000));
        session.usage_totals = Some(usage_info(100, 10));
        let text = spans_text(&usage_segments(&session, &theme));
        assert!(text.contains("↑12k ↓3k"), "{text}");
        assert!(!text.contains('%'), "没有 context_limit 就不画占比：{text}");
        assert!(!text.contains("cache"), "没上报缓存就不画命中率：{text}");

        // context_limit + 缓存上报 → 占比与命中率都出来。
        session.context_limit = Some(200_000);
        session.usage_totals = Some(qaqh_client::UsageInfo {
            prompt_cache_hit_tokens: 870,
            prompt_cache_miss_tokens: 130,
            cache_usage_reported: Some(true),
            ..usage_info(0, 0)
        });
        let text = spans_text(&usage_segments(&session, &theme));
        assert!(text.contains("(6%)"), "12000/200000 = 6%：{text}");
        assert!(text.contains("cache 87%"), "{text}");
        assert!(!text.contains("tok/s"), "回合没结束就没有速率：{text}");
    }

    /// 输出速率要**两端权威墙钟**：缺任一端都不给数（不拿本地时钟兜底）。
    #[test]
    fn token_rate_needs_both_ends_of_the_turn() {
        let mut session = SessionState::new("s".into());
        session.usage = Some(usage_info(100, 2_000));
        session.last_turn_finished = Some(("t1".into(), 12_000));
        assert!(token_rate(&session).is_none(), "缺 started_at_ms → 不给数");

        session.timeline.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "q".into(),
            },
        ));
        session.timeline.record_turn_time("t1", Some(2_000));
        // 10 秒里产出 2000 token = 200 tok/s。
        assert_eq!(token_rate(&session).map(|rate| rate.round()), Some(200.0));

        // 流式途中不给数（`usage` 可能还是上一轮的）。
        session.streaming = Some(crate::app::session::StreamingState {
            turn_id: "t2".into(),
            phase: crate::app::session::StreamPhase::Answering,
            round_num: 0,
            tool_name: None,
            armed_at: std::time::Instant::now(),
        });
        assert!(token_rate(&session).is_none());
    }

    fn usage_info(prompt_tokens: u32, completion_tokens: u32) -> qaqh_client::UsageInfo {
        qaqh_client::UsageInfo {
            prompt_tokens,
            completion_tokens,
            ..qaqh_client::UsageInfo::default()
        }
    }

    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
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
            .get_mut("session-1")
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
                ..Default::default()
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

    /// 收集整行都铺着 composer 输入带底色的行号（跳过左侧会话栏占位列）。
    fn composer_band_rows(
        terminal: &Terminal<TestBackend>,
        width: u16,
        height: u16,
        band: ratatui::style::Color,
    ) -> Vec<usize> {
        let rail = usize::from(crate::ui::v2::sidebar::rail_width(width));
        let buffer = terminal.backend().buffer();
        (0..usize::from(height))
            .filter(|y| {
                (rail..usize::from(width)).all(|x| {
                    buffer
                        .cell((x as u16, *y as u16))
                        .is_some_and(|cell| cell.bg == band)
                })
            })
            .collect()
    }

    #[test]
    fn composer_band_paints_three_contiguous_rows_above_status() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let band = test_theme().chrome.composer_bg;
        let rows = composer_band_rows(&terminal, 80, 24, band);
        assert_eq!(rows, vec![20, 21, 22], "composer band rows");
    }

    #[test]
    fn composer_band_centers_single_line_input() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        app.sessions
            .get_mut("session-1")
            .expect("session")
            .composer
            .insert_str("hello");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        draw_agent_frame(&mut terminal, &app, &route, &mut view);

        let band = test_theme().chrome.composer_bg;
        let rows = composer_band_rows(&terminal, 80, 24, band);
        assert_eq!(rows, vec![20, 21, 22], "composer band rows");
        // 单行输入垂直居中：`❯ hello` 落在色带中间一行。
        let buffer = terminal.backend().buffer();
        let middle: String = (0..80usize)
            .filter_map(|x| buffer.cell((x as u16, 21)).map(|cell| cell.symbol()))
            .collect();
        assert!(middle.contains("❯ hello"), "middle row was {middle:?}");
    }

    #[test]
    fn fullscreen_agent_draw_survives_resize() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        for (width, height) in [(80, 24), (40, 20), (20, 8), (10, 6), (120, 40)] {
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
                action: crate::app::ConfirmAction::UndoTurn { session_id, turn_id }
            }) if session_id == "session-1" && turn_id == "turn-1"
        ));
    }

    #[test]
    fn fullscreen_scroll_clamps_to_rendered_content() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcripts
            .touch("session-1")
            .sync(&app.sessions["session-1"], 79, &theme);
        view.body_height = 10;

        view.scroll_up(&mut app, usize::MAX / 2);
        let session = app.active_session().expect("session");
        assert!(!session.scroll.follow);
        assert_eq!(session.scroll.offset, view.max_offset(&app));

        view.scroll_down(&mut app, usize::MAX / 2);
        assert!(app.active_session().expect("session").scroll.follow);
    }

    #[tokio::test]
    async fn fullscreen_page_up_requests_older_at_top() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcripts
            .touch("session-1")
            .sync(&app.sessions["session-1"], 79, &theme);
        view.body_height = 10;

        let max_offset = view.max_offset(&app);
        let session = app.sessions.get_mut("session-1").expect("session");
        session.timeline.has_more = true;
        session.timeline.turns[0].turn_index = Some(1);
        session.scroll.offset = max_offset;

        view.page_up(&mut app);

        assert!(app.sessions["session-1"].loading_older);
    }

    #[test]
    fn fullscreen_block_cache_reuses_unchanged_blocks() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        let theme = test_theme();
        let mut cache = FullscreenTranscriptCache::default();

        cache.sync(&app.sessions["session-1"], 79, &theme);
        assert_eq!(cache.render_misses, 6);
        let before = cache.render_misses;

        cache.sync(&app.sessions["session-1"], 79, &theme);
        assert_eq!(cache.render_misses, before, "same version must be a no-op");

        let session = app.sessions.get_mut("session-1").expect("session");
        session.timeline.version = session.timeline.version.saturating_add(1);
        let turn = session.timeline.turns.last_mut().expect("turn");
        let block = turn
            .rounds
            .iter_mut()
            .flat_map(|round| &mut round.blocks)
            .find(|block| block.kind == TimelineBlockKind::Text)
            .expect("text block");
        block.text.push_str(" updated");
        // 缓存键的内容身份是 (block_id, rev)——rev 是「可见内容可能变化即自增」
        // 的权威计数（`Block::touch`），生产路径（TextDelta 等）必然调用；这里
        // 模拟同一条纪律。旧键曾哈希整个 kind 内容，绕过 rev 也能 miss，但那
        // 要求 view 类型 derive Hash 且每次 O(内容)。
        block.touch();

        cache.sync(&app.sessions["session-1"], 79, &theme);
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
            app.sessions["session-1"].expanded_tools.contains("tool-1"),
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
            app.sessions["session-1"]
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
            app.sessions["session-1"]
                .expanded_thinking
                .contains("thinking-1"),
            "click must toggle the historical thinking block"
        );
    }

    #[tokio::test]
    async fn agent_rail_lists_activated_sessions_and_opens_on_click() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.session_list_cache = vec![
            SessionListEntry {
                meta: SessionMeta {
                    session_id: "session-1".into(),
                    ..SessionMeta::default()
                },
                running: true,
                workspace_id: None,
            },
            SessionListEntry {
                meta: SessionMeta {
                    session_id: "session-2".into(),
                    ..SessionMeta::default()
                },
                running: true,
                workspace_id: None,
            },
        ];
        app.activity_cache.insert(
            "session-2".into(),
            qaqh_client::DomainActivityState::Working,
        );
        app.show_workspace = false;

        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 100, 24);
        let rail_rect = |index: usize| {
            frames
                .current()
                .expect("published frame")
                .regions
                .iter()
                .find_map(|region| match &region.target {
                    PointerTarget::Agent(AgentTarget::SidebarRow(index_)) if *index_ == index => {
                        Some(region.rect)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("rail row {index} must be registered"))
        };
        let (rect0, rect1) = (rail_rect(0), rail_rect(1));
        assert_eq!(rect0.x, 0, "rail occupies the left edge");
        assert!(rect1.y > rect0.y, "rail rows stack vertically");
        assert_eq!(rect1.x, 0, "rail rows span the rail column");

        // 悬停：只改视觉状态，不触发动作。
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(MouseEventKind::Moved, rect1.x + 1, rect1.y)),
            &mut frames,
            &mut view,
        );
        assert_eq!(view.pointer.sidebar_hover, Some(1));

        // 点击未打开的 session-2 → 开新 tab 并聚焦（同 workspace 开会话语义）。
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Down(MouseButton::Left),
                rect1.x + 1,
                rect1.y,
            )),
            &mut frames,
            &mut view,
        );
        handle_message(
            &mut app,
            AppMsg::Mouse(left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                rect1.x + 1,
                rect1.y,
            )),
            &mut frames,
            &mut view,
        );
        assert!(
            app.tabs.contains(&"session-2".to_string()),
            "click must open the session tab"
        );
        assert_eq!(app.tabs[app.active], "session-2");
    }

    #[test]
    fn agent_rail_hidden_below_min_width() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.session_list_cache = vec![SessionListEntry {
            meta: SessionMeta {
                session_id: "session-1".into(),
                ..SessionMeta::default()
            },
            running: true,
            workspace_id: None,
        }];
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let (map, _backend) = draw_agent_to_map(&app, &mut view, 80, 24);
        assert!(
            !map.regions.iter().any(|region| matches!(
                region.target,
                PointerTarget::Agent(AgentTarget::SidebarRow(_))
            )),
            "narrow terminal must not register rail rows"
        );
    }

    #[test]
    fn fullscreen_switch_back_reuses_cached_blocks_without_rerender() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        let mut session2 = SessionState::new("session-2".into());
        session2.timeline = model_with_many_sealed_turns(2);
        app.tabs.push("session-2".into());
        app.sessions.insert("session-2".into(), session2);
        let theme = test_theme();
        let mut view = FullscreenView::default();

        let synced_misses = |view: &mut FullscreenView, id: &str| {
            let cache = view.transcripts.touch(id);
            cache.sync(&app.sessions[id], 79, &theme);
            cache.render_misses
        };

        // 渲染 session-1（active）→ 切到 session-2 渲染 → 切回 session-1：
        // per-session 缓存必须原样命中，块级零重渲染。
        let misses_1 = synced_misses(&mut view, "session-1");
        synced_misses(&mut view, "session-2");
        let misses_1_back = synced_misses(&mut view, "session-1");
        assert_eq!(
            misses_1_back, misses_1,
            "switching back must hit the resident cache, not re-render blocks"
        );
    }

    #[test]
    fn transcript_cache_evicts_least_recently_used_beyond_capacity() {
        let mut app = app_with_model(model_with_many_sealed_turns(1));
        for index in 2..=5 {
            let session_id = format!("session-{index}");
            let mut session = SessionState::new(session_id.clone());
            session.timeline = model_with_many_sealed_turns(1);
            app.tabs.push(session_id.clone());
            app.sessions.insert(session_id, session);
        }
        let theme = test_theme();
        let mut view = FullscreenView::default();

        let synced_misses = |view: &mut FullscreenView, id: &str| {
            let cache = view.transcripts.touch(id);
            cache.sync(&app.sessions[id], 79, &theme);
            cache.render_misses
        };

        for index in 1..=4 {
            synced_misses(&mut view, format!("session-{index}").as_str());
        }
        // 复摸 session-1：LRU 顺位刷新，容量内零重渲染。
        let misses_1_first = synced_misses(&mut view, "session-1");
        // 第 5 个会话挤掉的是 session-2（最久未用），不是 session-1。
        synced_misses(&mut view, "session-5");
        assert_eq!(
            view.transcripts.len_for("session-2"),
            0,
            "least recently used cache must be evicted"
        );
        assert!(
            view.transcripts.len_for("session-1") > 0,
            "recently touched cache must survive the eviction"
        );
        // 逐出后复摸 session-2：全新缓存，从零重新计 miss。
        let misses_2_fresh = synced_misses(&mut view, "session-2");
        assert_eq!(
            misses_2_fresh, misses_1_first,
            "fresh cache re-renders everything"
        );
    }

    #[tokio::test]
    async fn switching_tabs_suspends_background_tabs() {
        let mut app = app_with_model(model_with_sealed_answer());
        let mut entry2 = SessionListEntry {
            meta: SessionMeta {
                session_id: "session-2".into(),
                ..SessionMeta::default()
            },
            running: true,
            workspace_id: None,
        };
        entry2.meta.title = Some("第二个会话".into());
        app.session_list_cache = vec![
            SessionListEntry {
                meta: SessionMeta {
                    session_id: "session-1".into(),
                    ..SessionMeta::default()
                },
                running: true,
                workspace_id: None,
            },
            entry2,
        ];

        // 开第二个 tab：它成为 active，只有它挂流。
        app.open_session_tab("session-2");
        fn ids(app: &App) -> Vec<&str> {
            let mut ids: Vec<&str> = app.tracked_session_ids.iter().map(String::as_str).collect();
            ids.sort_unstable();
            ids
        }
        assert_eq!(ids(&app), ["session-2"]);
        assert!(app.sessions["session-1"].suspended);
        assert!(!app.sessions["session-2"].suspended);

        // 侧栏点击切回 session-1（open_session_tab 已打开分支）：流跟随焦点。
        app.sidebar_open(0);
        assert_eq!(ids(&app), ["session-1"]);
        assert!(!app.sessions["session-1"].suspended);
        assert!(app.sessions["session-2"].suspended);
    }

    #[tokio::test]
    async fn alt_tab_key_swaps_tracked_stream() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.tabs.push("session-2".into());
        app.sessions
            .insert("session-2".into(), SessionState::new("session-2".into()));
        app.active = 1;
        let mut view = FullscreenView::default();
        let mut frames = FramePublisher::default();

        // Alt+Left 切回 session-1：tracked 集合与挂起标记必须跟着焦点走。
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        handle_message(&mut app, AppMsg::Key(key), &mut frames, &mut view);

        let ids: Vec<&str> = app.tracked_session_ids.iter().map(String::as_str).collect();
        assert!(
            ids.contains(&"session-1") && !ids.contains(&"session-2"),
            "tracked set must follow focus, got {ids:?}"
        );
        assert!(!app.sessions["session-1"].suspended);
        assert!(app.sessions["session-2"].suspended);
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
            app.sessions["session-1"].expanded_tools.contains("tool-1"),
            "click must toggle the tool card through the same App path"
        );
    }

    /// 用户报告的 bug 复现：点击展开「闪了一下，但没真正展开」。
    ///
    /// 完整走 run_loop 的绘制路径：点击前一帧 → Down/Up → 重绘一帧，
    /// 断言**画面上**工具卡正文真的变多（不是只看 App 集合翻转）。
    #[test]
    fn tool_card_click_actually_expands_on_the_repainted_frame() {
        let mut app = app_with_model(model_with_tool_card());
        app.show_workspace = false;
        let mut view = FullscreenView::default();
        let width = 80;
        let height = 24;

        let measure_body = |app: &App, view: &mut FullscreenView| -> (usize, String) {
            let mut frames = publish_frame(app, view, width, height);
            let len = view.transcripts.len_for("session-1");
            let text = view
                .transcripts
                .lines_for_test("session-1")
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = &mut frames;
            (len, text)
        };

        let (rows_before, text_before) = measure_body(&app, &mut view);
        // 折叠态显示的是**尾部 3 行**预览 + `… +5 行（点击展开）` 提示。
        assert!(
            text_before.contains("+5 行"),
            "折叠态必须显示折叠提示：{text_before}"
        );
        assert!(
            !text_before.contains("one\n"),
            "折叠态不该露出正文头部（one…）：{text_before}"
        );

        let mut frames = publish_frame(&app, &mut view, width, height);
        let rect = frames
            .current()
            .expect("published frame")
            .regions
            .iter()
            .find_map(|region| match &region.target {
                PointerTarget::Agent(AgentTarget::Tool { block_id, .. })
                    if block_id == "tool-1" =>
                {
                    Some(region.rect)
                }
                _ => None,
            })
            .expect("tool card must be registered");
        let (column, row) = (rect.x + 1, rect.y);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            handle_message(
                &mut app,
                AppMsg::Mouse(left_mouse(kind, column, row)),
                &mut frames,
                &mut view,
            );
        }

        // 关键一步：run_loop 在点击消息后的下一次迭代会重绘（B2 门控下
        // processed_messages=true 必绘）。这里用与 run_loop 相同的路径重画一帧。
        let (rows_after, text_after) = measure_body(&app, &mut view);

        assert!(
            app.sessions["session-1"].expanded_tools.contains("tool-1"),
            "App 状态应翻转"
        );
        assert!(
            rows_after > rows_before,
            "展开后画面行数必须真的变多（曾因块缓存键缺 expanded 位而停在 \
             折叠渲染，表现为「点击后闪一下但没展开」）：\
             before={rows_before} after={rows_after}"
        );
        assert!(
            text_after.contains("one"),
            "展开后正文头部必须出现在画面上：{text_after}"
        );
        assert!(
            !text_after.contains("+5 行"),
            "展开后折叠提示必须消失：{text_after}"
        );
    }

    #[test]
    fn agent_draw_registers_back_to_latest_only_when_scrolled() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("session-1")
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
        let session = app.sessions.get_mut("session-1").expect("session");
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
            .get_mut("session-1")
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
            app.sessions["session-1"].scroll.offset,
            metrics
                .offset_for_track_row(track_row)
                .min(view.max_offset(&app))
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
            app.sessions["session-1"].scroll.offset,
            metrics
                .offset_for_drag_row(drag_row, 0)
                .min(view.max_offset(&app))
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
    #[tokio::test]
    async fn load_older_button_is_clickable_and_starts_pagination() {
        let mut app = app_with_model(model_with_many_sealed_turns(3));
        app.show_workspace = false;
        {
            let session = app.sessions.get_mut("session-1").expect("session");
            session.timeline.has_more = true;
            session.timeline.turns[0].turn_index = Some(0);
        }
        let mut view = FullscreenView::default();
        let mut frames = publish_frame(&app, &mut view, 80, 24);
        let target = PointerTarget::Agent(AgentTarget::LoadOlder);
        assert_target_reachable(frames.current().unwrap(), &target);
        let rect = frames
            .current()
            .unwrap()
            .regions
            .iter()
            .find(|region| region.target == target)
            .expect("load older region")
            .rect;
        let (column, row) = (rect.x + 1, rect.y);
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
            app.sessions["session-1"].loading_older,
            "clicking the top entry must start the same pagination path as PgUp"
        );
    }

    #[test]
    fn agent_menu_wins_over_back_to_latest_when_they_overlap() {
        let mut app = app_with_model(model_with_many_sealed_turns(30));
        app.show_workspace = false;
        app.sessions
            .get_mut("session-1")
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
            scroll: 0,
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
            .get_mut("session-1")
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
                    session_id: format!("session-{index}"),
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
            .get_mut("session-1")
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
                ..Default::default()
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
            .get_mut("session-1")
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
