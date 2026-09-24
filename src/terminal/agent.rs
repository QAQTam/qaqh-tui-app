//! V2 Agent View：真实 Runtime/App 状态驱动的终端外壳（M4.1）。
//!
//! 运行：`qaqh-tui`（alpha1 起默认 inline；`--v2-fullscreen` 可切全屏）
//!
//! 与 `--v2-inline` 原型的区别：
//! - 复用生产 `Runtime` / `App`，因此会连接 daemon 并处理真实 timeline 事件；
//! - inline 模式把已封口 transcript 经 V2 projector + commit ledger 写入终端
//!   scrollback；fullscreen 模式改由 App 自己持有 transcript 视口与滚动状态；
//! - inline viewport 只绘制 live transcript、composer、status 与 shortcuts；
//! - inline 不启用鼠标捕获，保留终端原生选择/复制；fullscreen 捕获滚轮并支持
//!   浮层按钮，原生复制需要终端级绕过（通常是 Shift+选择）。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, poll, read,
};
use ratatui::crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{event::KeyCode, execute};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::session::SessionState;
use crate::app::timeline_model::{Turn, strip_ansi_escapes};
use crate::app::{App, AppMsg, ConnPhase, ModalHit, Overlay, StartupIntent};
use crate::runtime::{Runtime, RuntimeMsg};
use crate::terminal::transcript::PendingCommit;
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::fullscreen::{self, FullscreenState, MessageAction, MessageMenu};
use crate::ui::v2::route::{self, ScreenRoute};
use crate::ui::v2::runtime::V2TranscriptRuntime;
use crate::ui::v2::transcript::{BlockKind, BlockState, TranscriptBlock, render_transcript};
use crate::ui::v2::workspace;
use qaqh_client::{ConversationMode, NoticeLevel, TimelineBlockKind, TimelineBlockState};

const TICK_INTERVAL: Duration = Duration::from_millis(200);
const COMMIT_CHUNK_BLOCKS: usize = 32;
const MAX_SLASH_ROWS: usize = 4;
const MAX_VIEWPORT_HEIGHT: u16 = 16;
const LIVE_VIEWPORT_ROWS: usize = 4;
const NARROW_LIVE_VIEWPORT_ROWS: usize = 2;
const NARROW_VIEWPORT_WIDTH: u16 = 40;
const VIEWPORT_HEIGHT_PERCENT: u16 = 60;

/// 启动真实 V2 Agent View。
///
/// `fullscreen=false` 保留 alpha1 的 inline + scrollback 外壳；`true` 使用
/// alternate-screen 全屏 shell，由 App 自己持有 transcript 视口与滚动状态。
pub async fn run(no_spawn: bool, resume: bool, fullscreen: bool) -> Result<()> {
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
    // V2 的 F4/Workspace 是 alternate-screen 工作区，不再复用 v1 常驻 sidebar。
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
    let mut terminal = if fullscreen {
        TerminalHost::init_fullscreen()
    } else {
        TerminalHost::init(initial_inline_height(&app, theme))
    };
    let input_setup = if fullscreen {
        execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)
    } else {
        execute!(stdout(), EnableBracketedPaste)
    };
    if let Err(error) = input_setup {
        ratatui::restore();
        runtime.shutdown().await;
        return Err(error).context("启用终端输入");
    }

    let mut input = InputPump::new(app_tx.clone());
    spawn_tick(app_tx.clone());

    let mut agent = AgentState::default();
    let mut fullscreen_view = FullscreenView::default();

    let result = run_loop(
        &mut terminal,
        &mut input,
        &mut app_rx,
        &mut app,
        &mut agent,
        &mut fullscreen_view,
        theme,
    )
    .await;

    input.suspend().await;
    // ⚠ 退出清理必须**同时**关掉鼠标捕获：捕获是在 `enter_alternate` 里开的，
    // 而用户完全可能在全屏面（弹窗 / Workspace）里直接退出——那条路径不经过
    // `leave_alternate`，只靠它收尾会把终端留在鼠标上报模式，用户的原生选择/
    // 复制就此失效（实测：`?1000h` 有、`?1000l` 没有）。重复关是幂等的。
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
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
                                // 鼠标只在弹窗（alternate screen）期间被捕获，
                                // 见 `TerminalHost::enter_alternate`。
                                Event::Mouse(mouse) => AppMsg::Mouse(mouse),
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
    /// `Terminal::with_options(Viewport::Inline)` 重建 viewport 时要读取 cursor
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
    agent: &mut AgentState,
    fullscreen_view: &mut FullscreenView,
    theme: &'static Theme,
) -> Result<()> {
    loop {
        if app.quit {
            break;
        }
        let route = route::resolve(app);
        if terminal.mode == ScreenMode::Fullscreen && route != ScreenRoute::Agent {
            fullscreen_view.close_menu();
        }
        let size = terminal.terminal.size()?;
        let previous_size = terminal.last_terminal_size;
        let terminal_resized = terminal.note_terminal_size(size.width, size.height);

        if terminal.mode == ScreenMode::Fullscreen {
            // 全屏 shell 自己渲染整张 transcript；resize 只交给 ratatui
            // autoresize，不再重建 inline viewport，也不 purge scrollback。
            if terminal_resized {
                fullscreen_view.close_menu();
                terminal.terminal.autoresize()?;
            }
        } else {
            let desired_height = inline_viewport_height(app, size.width, size.height, theme);
            terminal.set_inline_height(desired_height);

            if terminal_resized
                && size.height < previous_size.1
                && route == ScreenRoute::Agent
                && terminal.mode == ScreenMode::Inline
            {
                // 缩小会把旧 viewport 的可见行留在新 origin 上方。清掉 scrollback 后
                // 用 timeline 重放，避免旧 logo / 工具卡 / 状态栏残留成重影。
                input.suspend().await;
                let result = terminal.purge_scrollback_for_replay();
                input.resume();
                result?;
                agent.force_replay(app);
            } else if terminal_resized {
                // 其他尺寸变化交给 ratatui autoresize；不要手工重建 inline viewport。
                terminal.terminal.autoresize()?;
            }

            reconcile_screen(terminal, input, &route, app, agent, theme).await?;
            if route == ScreenRoute::Agent && terminal.needs_inline_rebuild(size.height) {
                input.suspend().await;
                let result = terminal.ensure_inline_height(size.height);
                input.resume();
                result?;
            }
        }

        if app.force_redraw {
            terminal.terminal.clear()?;
            app.force_redraw = false;
        }
        let screen_mode = terminal.mode;
        terminal
            .terminal
            .draw(|frame| draw(frame, app, theme, &route, screen_mode, fullscreen_view))?;
        if screen_mode == ScreenMode::Fullscreen && route == ScreenRoute::Agent {
            fullscreen_view.clamp_scroll(app);
        }

        let Some(msg) = app_rx.recv().await else {
            break;
        };
        handle_message(app, msg, terminal, &route, fullscreen_view)?;
        while let Ok(msg) = app_rx.try_recv() {
            handle_message(app, msg, terminal, &route, fullscreen_view)?;
            if app.quit {
                break;
            }
        }
        if let Some(text) = app.pending_pager.take() {
            input.suspend().await;
            let result = terminal.run_pager(&text);
            input.resume();
            result?;
        }
    }
    Ok(())
}

fn handle_message(
    app: &mut App,
    msg: AppMsg,
    terminal: &TerminalHost,
    route: &ScreenRoute,
    fullscreen_view: &mut FullscreenView,
) -> Result<()> {
    // 鼠标在 v2 里由**渲染层**接管：命中测试需要弹窗/全屏 shell 几何（只有这里
    // 知道当前屏幕区域），而 v1 那套 `App::handle_mouse`（第 0 行 = tab bar）
    // 在 v2 是错的——alt screen 的第 0 行不是 tab bar，点一下历史区就切标签页。
    if let AppMsg::Mouse(mouse) = msg {
        match (terminal.mode, route) {
            (ScreenMode::Fullscreen, ScreenRoute::Agent) => {
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_fullscreen_agent_mouse(app, fullscreen_view, area, mouse);
            }
            (_, ScreenRoute::Modal(modal)) => {
                fullscreen_view.pointer.clear_pointer();
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_modal_mouse(app, *modal, area, mouse);
            }
            _ => fullscreen_view.pointer.clear_pointer(),
        }
        // inline 主界面不捕获鼠标；全屏 Workspace 本轮不接鼠标。
        return Ok(());
    }
    if terminal.mode == ScreenMode::Fullscreen
        && *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && matches!(&msg, AppMsg::Paste(_))
    {
        return Ok(());
    }
    if terminal.mode == ScreenMode::Fullscreen
        && *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && let AppMsg::Key(key) = &msg
    {
        handle_fullscreen_menu_key(app, fullscreen_view, key);
        return Ok(());
    }
    if let AppMsg::Key(key) = &msg
        && terminal.mode == ScreenMode::Fullscreen
        && *route == ScreenRoute::Agent
    {
        match key.code {
            KeyCode::PageUp => {
                fullscreen_view.page_up(app);
                return Ok(());
            }
            KeyCode::PageDown => {
                fullscreen_view.scroll_down(app, 20);
                return Ok(());
            }
            _ => {}
        }
    }
    if let AppMsg::Key(key) = &msg
        && key.code == KeyCode::Esc
        && route::resolve(app) == ScreenRoute::Workspace(route::WorkspaceRoute::Todo)
    {
        app.show_workspace = false;
        return Ok(());
    }
    app.handle(msg);
    Ok(())
}

fn handle_fullscreen_agent_mouse(
    app: &mut App,
    view: &mut FullscreenView,
    area: ratatui::layout::Rect,
    mouse: ratatui::crossterm::event::MouseEvent,
) {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};

    if view.menu.is_some() {
        let hit = view.menu.as_ref().and_then(|menu| {
            fullscreen::message_menu_hit_test(area, menu, mouse.column, mouse.row)
        });
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(menu) = view.menu.as_mut() {
                    menu.hover = hit;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if hit.is_none() {
                    view.close_menu();
                    return;
                }
                if let Some(menu) = view.menu.as_mut() {
                    menu.hover = hit;
                    menu.pressed = hit;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = view.menu.as_mut().and_then(|menu| menu.pressed.take());
                if let Some(menu) = view.menu.as_mut() {
                    menu.hover = hit;
                }
                if pressed.is_some()
                    && pressed == hit
                    && let Some(action) = view.menu.as_ref().and_then(MessageMenu::activate)
                {
                    activate_message_action(app, view, action);
                }
            }
            _ => {}
        }
        return;
    }

    let show_back_to_latest =
        view.can_scroll() && !app.active_session().is_some_and(|s| s.scroll.follow);
    let hit_back_to_latest = || {
        show_back_to_latest
            .then(|| fullscreen::hit_test(area, mouse.column, mouse.row))
            .flatten()
    };

    match mouse.kind {
        MouseEventKind::ScrollUp => view.scroll_up(app, 3),
        MouseEventKind::ScrollDown => view.scroll_down(app, 3),
        MouseEventKind::Moved => {
            view.pointer.back_to_latest_hover = hit_back_to_latest().is_some();
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let target = hit_back_to_latest();
            view.pointer.back_to_latest_hover = target.is_some();
            view.pointer.back_to_latest_pressed = target.is_some();
            if target.is_none()
                && let Some(hit) = view.assistant_at(mouse.column, mouse.row)
            {
                view.open_menu(hit, mouse.column, mouse.row);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let released = hit_back_to_latest();
            view.pointer.back_to_latest_hover = released.is_some();
            if view.pointer.back_to_latest_pressed && released.is_some() {
                app.scroll_bottom();
            }
            view.pointer.back_to_latest_pressed = false;
        }
        _ => {}
    }
}

fn handle_fullscreen_menu_key(
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

fn activate_message_action(app: &mut App, view: &mut FullscreenView, action: MessageAction) {
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
        MessageAction::Retry | MessageAction::Fork => {}
    }
}

fn assistant_markdown(app: &App, turn_id: &str, block_id: &str) -> Option<String> {
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
fn handle_modal_mouse(
    app: &mut App,
    modal: route::ModalRoute,
    area: ratatui::layout::Rect,
    mouse: ratatui::crossterm::event::MouseEvent,
) {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    let hit = |app: &App| crate::ui::v2::modal::hit_test(app, modal, area, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Moved => {
            app.modal_hover = hit(app);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let target = hit(app);
            app.modal_hover = target;
            app.modal_pressed = target;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let released = hit(app);
            app.modal_hover = released;
            let pressed = app.modal_pressed.take();
            if let (Some(pressed), Some(released)) = (pressed, released)
                && pressed == released
            {
                dispatch_modal_hit(app, pressed);
            }
        }
        // 其它按钮（右键/中键）与滚轮：弹窗里暂不接。
        _ => {}
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenMode {
    Inline,
    Alternate,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenTransition {
    Stay,
    EnterAlternate,
    LeaveAlternate,
}

fn screen_transition(mode: ScreenMode, route: &ScreenRoute) -> ScreenTransition {
    match (mode, route) {
        (ScreenMode::Fullscreen, _) => ScreenTransition::Stay,
        (ScreenMode::Inline, ScreenRoute::Agent)
        | (ScreenMode::Alternate, ScreenRoute::Workspace(_))
        | (ScreenMode::Alternate, ScreenRoute::Modal(_)) => ScreenTransition::Stay,
        (ScreenMode::Inline, ScreenRoute::Workspace(_) | ScreenRoute::Modal(_)) => {
            ScreenTransition::EnterAlternate
        }
        (ScreenMode::Alternate, ScreenRoute::Agent) => ScreenTransition::LeaveAlternate,
    }
}

struct TerminalHost {
    terminal: DefaultTerminal,
    mode: ScreenMode,
    requested_inline_height: u16,
    desired_inline_height: u16,
    last_terminal_size: (u16, u16),
}

impl TerminalHost {
    fn init(inline_height: u16) -> Self {
        let inline_height = inline_height.max(1);
        let terminal = ratatui::init_with_options(TerminalOptions {
            viewport: Viewport::Inline(inline_height),
        });
        let terminal_size = ratatui::crossterm::terminal::size().unwrap_or((0, 0));
        Self {
            terminal,
            mode: ScreenMode::Inline,
            requested_inline_height: inline_height,
            desired_inline_height: inline_height,
            last_terminal_size: terminal_size,
        }
    }

    /// alternate-screen 全屏 shell。
    ///
    /// `ratatui::init()` 会启用 raw mode、进入 alternate screen 并安装 panic
    /// restore hook；鼠标捕获由 `run` 在初始化成功后单独打开，确保错误路径也能
    /// 统一清理。
    fn init_fullscreen() -> Self {
        let terminal = ratatui::init();
        let terminal_size = ratatui::crossterm::terminal::size().unwrap_or((0, 0));
        Self {
            terminal,
            mode: ScreenMode::Fullscreen,
            requested_inline_height: 0,
            desired_inline_height: 0,
            last_terminal_size: terminal_size,
        }
    }

    fn note_terminal_size(&mut self, width: u16, height: u16) -> bool {
        let next = (width, height);
        let changed = self.last_terminal_size != next;
        self.last_terminal_size = next;
        changed
    }

    fn set_inline_height(&mut self, height: u16) {
        self.desired_inline_height = height.max(1);
    }

    fn needs_inline_rebuild(&self, terminal_height: u16) -> bool {
        if self.mode != ScreenMode::Inline
            || self.requested_inline_height == self.desired_inline_height
        {
            return false;
        }
        // 终端高度把目标 viewport 夹住了：这是 ratatui 的 autoresize 职责，
        // 不要在这里重建一个更矮的 inline viewport，否则放大再缩小会留下旧行。
        let terminal_height = terminal_height.max(1);
        if self.desired_inline_height >= terminal_height
            && self.requested_inline_height > self.desired_inline_height
        {
            return false;
        }
        true
    }

    /// 在当前 inline viewport 高度与布局需求不一致时重建 viewport。
    ///
    /// 只清理旧 viewport 区域：增高锚定顶部，缩高锚定底边；不会重放 transcript。
    fn ensure_inline_height(&mut self, terminal_height: u16) -> Result<()> {
        if !self.needs_inline_rebuild(terminal_height) {
            return Ok(());
        }
        self.rebuild_inline(self.desired_inline_height)
    }

    fn rebuild_inline(&mut self, height: u16) -> Result<()> {
        let height = height.max(1);
        let old_area = self.terminal.get_frame().area();
        // 缩高时保持 viewport 底边不动，否则旧底部行会留在新 viewport 下面，
        // 表现为状态栏/输入框重影；增高仍锚定顶部，避免侵入上方 scrollback。
        let anchor_y = if height < old_area.height {
            old_area
                .y
                .saturating_add(old_area.height)
                .saturating_sub(height)
        } else {
            old_area.y
        };
        self.terminal.clear()?;
        self.terminal
            .set_cursor_position(Position::new(0, anchor_y))?;
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        self.requested_inline_height = height;
        self.desired_inline_height = height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    /// 进 alternate screen（弹窗 / 工作区）。
    ///
    /// **鼠标捕获只在这里开**：alt screen 里没有 scrollback，终端原生滚轮/选择
    /// 本来也用不上，所以"吃掉原生鼠标"在这里代价最小；回到 inline 必须立刻
    /// 关掉（见 `leave_alternate`），否则主界面的原生选择/复制就废了。
    fn enter_alternate(&mut self) -> Result<()> {
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        self.terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        self.mode = ScreenMode::Alternate;
        Ok(())
    }

    fn leave_alternate(&mut self) -> Result<()> {
        let inline_height = self.desired_inline_height.max(1);
        execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(inline_height),
            },
        )?;
        self.requested_inline_height = inline_height;
        self.desired_inline_height = inline_height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    /// 清空屏幕与终端 scrollback，并把 inline viewport 重新锚定到顶部。
    ///
    /// 只用于会话切换与终端缩屏重建：旧会话/旧 viewport 的历史必须从终端历史里
    /// 移除，否则新内容只能被追加到旧历史后面，无法满足“清屏 + 按 ledger 顺序重放”。
    /// 调用后由 `commit_pending` 写入新 seed 的完整已封口快照。
    fn purge_scrollback_for_replay(&mut self) -> Result<()> {
        let inline_height = self.desired_inline_height.max(1);
        execute!(
            stdout(),
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            MoveTo(0, 0)
        )?;
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(inline_height),
            },
        )?;
        self.requested_inline_height = inline_height;
        self.desired_inline_height = inline_height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    /// 挂起 TUI → 外部分页器 → 按原屏幕模式恢复。
    fn run_pager(&mut self, text: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("qaqh-pager-{}.md", std::process::id()));
        if std::fs::write(&path, text).is_err() {
            return Ok(());
        }

        let was_fullscreen = self.mode == ScreenMode::Fullscreen;
        let was_alternate = self.mode == ScreenMode::Alternate;
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

        if was_fullscreen {
            self.terminal = ratatui::init();
            self.requested_inline_height = 0;
            self.desired_inline_height = 0;
            self.mode = ScreenMode::Fullscreen;
            execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        } else {
            let inline_height = self.desired_inline_height.max(1);
            self.terminal = ratatui::init_with_options(TerminalOptions {
                viewport: Viewport::Inline(inline_height),
            });
            self.requested_inline_height = inline_height;
            self.desired_inline_height = inline_height;
            self.mode = ScreenMode::Inline;
            execute!(stdout(), EnableBracketedPaste)?;
            if was_alternate {
                self.enter_alternate()?;
            }
        }
        let _ = self.terminal.clear();
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

async fn reconcile_screen(
    terminal: &mut TerminalHost,
    input: &mut InputPump,
    route: &ScreenRoute,
    app: &App,
    agent: &mut AgentState,
    theme: &Theme,
) -> Result<()> {
    if terminal.mode == ScreenMode::Fullscreen {
        return Ok(());
    }
    match screen_transition(terminal.mode, route) {
        ScreenTransition::Stay => {
            if *route == ScreenRoute::Agent {
                commit_pending(terminal, input, app, agent, theme).await?;
            }
        }
        ScreenTransition::EnterAlternate => {
            commit_pending(terminal, input, app, agent, theme).await?;
            input.suspend().await;
            let result = terminal.enter_alternate();
            input.resume();
            result?;
        }
        ScreenTransition::LeaveAlternate => {
            input.suspend().await;
            let result = terminal.leave_alternate();
            input.resume();
            result?;
            commit_pending(terminal, input, app, agent, theme).await?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct AgentState {
    transcript: V2TranscriptRuntime,
    seed: Option<String>,
    pending_commits: VecDeque<PendingCommit>,
    /// 已稳定、但必须等更早的 pending block 提交完后才能写 scrollback 的流式行。
    pending_stream_lines: VecDeque<Line<'static>>,
    streaming: StreamingCommitState,
    streamed_blocks: HashSet<(String, String)>,
    replay_active: bool,
    replay_cursor: usize,
    replay_version: Option<u64>,
}

#[derive(Debug, Default)]
struct StreamingCommitState {
    turn_id: Option<String>,
    block_id: Option<String>,
    text: String,
    processed_lines: usize,
    emitted_lines: usize,
    in_code: bool,
    code_lang: Option<String>,
}

impl StreamingCommitState {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
struct AgentSync {
    pending: Vec<PendingCommit>,
    reset_scrollback: bool,
}

impl AgentState {
    /// 把当前活动会话的 timeline 同步为待提交块。
    ///
    /// 首次进入一个 seed 时先以权威快照重建 projector 的“已见”状态，再全量
    /// 重放；ledger 会拒绝已经写进 scrollback 的块。之后只做增量投影。
    fn sync(&mut self, app: &App) -> AgentSync {
        let Some(seed) = app.active_seed() else {
            let reset_scrollback = self.seed.is_some();
            self.seed = None;
            self.transcript.clear();
            self.pending_commits.clear();
            self.reset_streaming();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            return AgentSync {
                pending: Vec::new(),
                reset_scrollback,
            };
        };
        let Some(session) = app.sessions.get(&seed) else {
            let reset_scrollback = self.seed.is_some();
            self.seed = None;
            self.transcript.clear();
            self.pending_commits.clear();
            self.reset_streaming();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            return AgentSync {
                pending: Vec::new(),
                reset_scrollback,
            };
        };

        if self.seed.as_deref() != Some(seed.as_str()) {
            self.seed = Some(seed.clone());
            self.pending_commits.clear();
            self.reset_streaming();
            self.transcript.begin_replay(&seed);
            self.replay_active = true;
            self.replay_cursor = 0;
            self.replay_version = Some(session.timeline.version);
            return AgentSync {
                pending: self.replay_chunk(
                    &seed,
                    &session.timeline.turns,
                    session.timeline.version,
                ),
                reset_scrollback: true,
            };
        }

        if self.replay_active {
            if self.replay_version != Some(session.timeline.version) {
                self.transcript.begin_replay(&seed);
                self.reset_streaming();
                self.replay_cursor = 0;
                self.replay_version = Some(session.timeline.version);
            }
            return AgentSync {
                pending: self.replay_chunk(
                    &seed,
                    &session.timeline.turns,
                    session.timeline.version,
                ),
                reset_scrollback: false,
            };
        }

        AgentSync {
            pending: self.transcript.sync_timeline_versioned(
                &seed,
                &session.timeline.turns,
                session.timeline.version,
            ),
            reset_scrollback: false,
        }
    }

    fn reset_streaming(&mut self) {
        self.pending_stream_lines.clear();
        self.streaming.reset();
        self.streamed_blocks.clear();
    }

    /// 终端缩小时，旧 inline viewport 的可见行会变成屏幕残留；清 scrollback 后
    /// 用权威 timeline 重放，等价于一次会话切换的干净重建。
    fn force_replay(&mut self, app: &App) {
        self.pending_commits.clear();
        self.reset_streaming();
        let Some(seed) = app.active_seed() else {
            self.seed = None;
            self.transcript.clear();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            return;
        };
        let Some(session) = app.sessions.get(&seed) else {
            self.seed = None;
            self.transcript.clear();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            return;
        };
        self.seed = Some(seed.clone());
        self.transcript.begin_replay(&seed);
        self.replay_active = true;
        self.replay_cursor = 0;
        self.replay_version = Some(session.timeline.version);
    }

    fn was_streamed(&self, block: &TranscriptBlock) -> bool {
        self.streamed_blocks
            .contains(&(block.turn_id.clone(), block.id.to_string()))
    }

    /// 把当前 open assistant 的**完整行**变成可提交行；最后一行留在 live tail。
    fn sync_streaming(&mut self, app: &App, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let current = app.active_session().and_then(open_assistant_block);
        let mut out = Vec::new();
        match current {
            Some((turn_id, block_id, text)) => {
                let same_block = self.streaming.turn_id.as_deref() == Some(turn_id)
                    && self.streaming.block_id.as_deref() == Some(block_id);
                if !same_block {
                    out.extend(self.finish_streaming(width, theme));
                    self.streaming.turn_id = Some(turn_id.to_string());
                    self.streaming.block_id = Some(block_id.to_string());
                }
                self.streaming.text.clear();
                self.streaming.text.push_str(text);
                let stable = stable_line_count(text, false);
                out.extend(self.render_streaming_lines(stable, width, theme));
            }
            None => out.extend(self.finish_streaming(width, theme)),
        }
        out
    }

    /// block 已封口/切换：把最后一行也提交，并标记该 block 不再走 sealed 整体渲染。
    fn finish_streaming(&mut self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.streaming.block_id.is_none() {
            return Vec::new();
        }
        let total = stable_line_count(&self.streaming.text, true);
        let out = self.render_streaming_lines(total, width, theme);
        if self.streaming.emitted_lines > 0
            && let (Some(turn_id), Some(block_id)) = (
                self.streaming.turn_id.take(),
                self.streaming.block_id.take(),
            )
        {
            self.streamed_blocks.insert((turn_id, block_id));
        }
        self.streaming.reset();
        out
    }

    fn render_streaming_lines(
        &mut self,
        target: usize,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let text = self.streaming.text.clone();
        let source: Vec<&str> = text.split('\n').collect();
        let mut out = Vec::new();
        while self.streaming.processed_lines < target {
            let Some(line) = source.get(self.streaming.processed_lines).copied() else {
                break;
            };
            self.streaming.processed_lines += 1;

            if let Some(lang) = fence_language(line) {
                if self.streaming.in_code {
                    self.streaming.in_code = false;
                    self.streaming.code_lang = None;
                } else {
                    self.streaming.in_code = true;
                    self.streaming.code_lang = lang;
                }
                continue;
            }

            let first_line = self.streaming.emitted_lines == 0;
            let rendered = crate::ui::v2::transcript::render_stream_line(
                line,
                width,
                theme,
                first_line,
                self.streaming.in_code,
                self.streaming.code_lang.as_deref(),
            );
            self.streaming.emitted_lines =
                self.streaming.emitted_lines.saturating_add(rendered.len());
            out.extend(rendered);
        }
        out
    }

    fn replay_chunk(&mut self, seed: &str, turns: &[Turn], version: u64) -> Vec<PendingCommit> {
        const REPLAY_TURNS_PER_FRAME: usize = 8;

        let end = self
            .replay_cursor
            .saturating_add(REPLAY_TURNS_PER_FRAME)
            .min(turns.len());
        let pending = self
            .transcript
            .replay_slice(seed, &turns[self.replay_cursor..end]);
        self.replay_cursor = end;
        if self.replay_cursor >= turns.len() {
            self.replay_active = false;
            self.transcript.finish_replay(version);
        }
        pending
    }

    fn take_commit_chunk(&mut self, max: usize) -> Vec<PendingCommit> {
        let count = max.min(self.pending_commits.len());
        self.pending_commits.drain(..count).collect()
    }
}

async fn commit_pending(
    host: &mut TerminalHost,
    input: &mut InputPump,
    app: &App,
    agent: &mut AgentState,
    theme: &Theme,
) -> Result<()> {
    let width = host.terminal.get_frame().area().width;
    let sync = agent.sync(app);
    if sync.reset_scrollback {
        input.suspend().await;
        let result = host.purge_scrollback_for_replay();
        input.resume();
        result?;
    }

    let stream_lines = agent.sync_streaming(app, usize::from(width), theme);
    agent.pending_stream_lines.extend(stream_lines);

    let mut pending = sync.pending;
    pending.retain(|item| !agent.was_streamed(&item.block));
    agent.pending_commits.extend(pending);

    let chunk = agent.take_commit_chunk(COMMIT_CHUNK_BLOCKS);
    if !chunk.is_empty() {
        let blocks: Vec<_> = chunk.into_iter().map(|item| item.block).collect();
        let mut lines = render_transcript(&blocks, width, theme);
        if lines.is_empty() {
            lines.push(Line::default());
        }
        commit_lines(host, lines)?;
    }

    // 流式行必须排在所有更早的 sealed block 后面；有积压时先留在队列，
    // 下一帧 pending 清空后再写，避免工具/回答顺序倒置。
    if agent.pending_commits.is_empty() && !agent.pending_stream_lines.is_empty() {
        let lines = agent.pending_stream_lines.drain(..).collect();
        commit_lines(host, lines)?;
    }
    Ok(())
}

fn commit_lines(host: &mut TerminalHost, lines: Vec<Line<'static>>) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    host.terminal.insert_before(height, |buffer| {
        Paragraph::new(lines).render(buffer.area, buffer);
        clear_wide_trailing_cells(buffer);
    })?;
    Ok(())
}

/// `Terminal::insert_before` 直接逐 cell 调 backend.draw，不会像普通 diff draw 一样
/// 跳过宽字符的尾格。Buffer 中宽字符尾格通常是 `" "`，Kitty 等终端收到
/// `你 + MoveTo(尾格) + 空格` 后会把整个宽字形擦成空白。
///
/// 这里把尾格 symbol 清成空串：backend 仍会移动光标，但不会打印覆盖空格，
/// 宽字符因此能保留下来。普通 diff draw 路径不受影响。
fn clear_wide_trailing_cells(buffer: &mut ratatui::buffer::Buffer) {
    let width = usize::from(buffer.area.width);
    let height = usize::from(buffer.area.height);
    if width == 0 || height == 0 {
        return;
    }

    for row in 0..height {
        let row_start = row * width;
        let mut col = 0;
        while col < width {
            let cell = &buffer.content[row_start + col];
            let cell_width = cell.symbol().width();
            if cell_width > 1 {
                for trailing in 1..cell_width {
                    let trailing_col = col + trailing;
                    if trailing_col < width {
                        buffer.content[row_start + trailing_col].set_symbol("");
                    }
                }
                col += cell_width;
            } else {
                col += 1;
            }
        }
    }
}

fn draw(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    route: &ScreenRoute,
    screen_mode: ScreenMode,
    fullscreen_view: &mut FullscreenView,
) {
    match route {
        ScreenRoute::Agent if screen_mode == ScreenMode::Fullscreen => {
            draw_fullscreen_agent(frame, app, theme, fullscreen_view);
        }
        ScreenRoute::Agent => draw_agent(frame, app, theme),
        ScreenRoute::Modal(modal) => {
            clear_screen(frame, theme);
            crate::ui::v2::modal::draw(frame, app, frame.area(), theme, *modal);
        }
        ScreenRoute::Workspace(workspace_route) => {
            clear_screen(frame, theme);
            workspace::draw(frame, app, workspace_route, theme);
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

fn draw_agent(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();
    let rendered = render_agent(app, area.width, area.height, theme);
    frame.render_widget(Paragraph::new(rendered.lines), area);
    if let Some(cursor) = rendered.cursor {
        frame.set_cursor_position((
            area.x.saturating_add(cursor.x),
            area.y.saturating_add(cursor.y),
        ));
    }
}

fn draw_fullscreen_agent(frame: &mut Frame, app: &App, theme: &Theme, view: &mut FullscreenView) {
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
    if let Some(session) = app.active_session() {
        fullscreen::draw_scrollbar(
            frame,
            body,
            view.transcript.lines.len(),
            usize::from(view.body_height),
            session.scroll.follow,
            session.scroll.offset,
            theme,
        );
    }
    if view.can_scroll()
        && app
            .active_session()
            .is_some_and(|session| !session.scroll.follow)
    {
        fullscreen::draw_back_to_latest(frame, body, view.pointer, theme);
    }
    if let Some(menu) = view.menu.as_ref() {
        fullscreen::draw_message_menu(frame, area, menu, theme);
    }
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

fn initial_inline_height(app: &App, theme: &Theme) -> u16 {
    let (width, height) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    inline_viewport_height(app, width, height, theme)
}

/// 计算 inline viewport 的目标高度。
///
/// 高度由实际布局需求推导，并受终端高度的 60% 与绝对上限约束。终端过矮时，
/// 先压缩 live/slash，再隐藏 shortcuts，最后才压缩 composer 与 status。
fn inline_viewport_height(app: &App, width: u16, terminal_height: u16, theme: &Theme) -> u16 {
    let terminal_height = terminal_height.max(1);
    let ratio_height = terminal_height
        .saturating_mul(VIEWPORT_HEIGHT_PERCENT)
        .checked_div(100)
        .unwrap_or(0)
        .clamp(1, MAX_VIEWPORT_HEIGHT);
    let max_height = ratio_height.min(terminal_height).max(1);

    if app.active_session().is_none() {
        // 普通启动要容纳品牌标识、输入框和提示；`resume` 首帧就是全屏
        // Workspace，inline 高度只需要一个安全占位。
        let desired = if app.startup_intent == StartupIntent::New {
            14
        } else {
            3
        };
        return desired.min(max_height).max(1);
    }

    u16::try_from(agent_layout(app, width, max_height, theme).height())
        .unwrap_or(u16::MAX)
        .min(max_height)
        .max(1)
}

/// 普通启动的品牌首屏：品牌标识 + 输入框 + 一行状态提示。
///
/// 这里不预造 session，也不写 scrollback；`Enter` 由 app 层转成
/// `SessionCreate`，首条消息在 seed 确认后补发。
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
    if width < 40 {
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

fn open_assistant_block(session: &SessionState) -> Option<(&str, &str, &str)> {
    let turn_id = session.timeline.running_turn_id()?;
    let turn = session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)?;
    for round in turn.rounds.iter().rev() {
        for block in round.blocks.iter().rev() {
            if block.kind == TimelineBlockKind::Text && block.state == TimelineBlockState::Open {
                return Some((
                    turn.turn_id.as_str(),
                    block.block_id.as_str(),
                    block.text.as_str(),
                ));
            }
        }
    }
    None
}

/// 稳定行数：未封口时最后一行始终留在 live tail；封口时才允许提交最后一行。
fn stable_line_count(text: &str, sealed: bool) -> usize {
    if text.is_empty() {
        return 0;
    }
    let lines = text.split('\n').count();
    if text.ends_with('\n') {
        lines.saturating_sub(1)
    } else if sealed {
        lines
    } else {
        lines.saturating_sub(1)
    }
}

/// `Some(lang)` = 围栏行；外层 `None` = 普通行。`lang=None` 表示无语言标记。
fn fence_language(line: &str) -> Option<Option<String>> {
    let trimmed = line.trim_start();
    let marker = if trimmed.starts_with("```") {
        "```"
    } else if trimmed.starts_with("~~~") {
        "~~~"
    } else {
        return None;
    };
    let rest = trimmed[marker.len()..].trim();
    Some((!rest.is_empty()).then(|| rest.to_string()))
}

fn session_is_working(session: &SessionState) -> bool {
    session.streaming.is_some() || session.timeline.running_turn_id().is_some()
}

/// live transcript 是否需要占位。
///
/// 当前只有**运行中的工具**需要多行可变正文：进度、输出、状态会持续变化。
/// reasoning 走 thinking 行；assistant open block 的稳定行走流式提交，未完成
/// 尾行走单独的 stream 行，不再把整段 assistant 塞进 live transcript。
fn has_live_transcript(session: &SessionState) -> bool {
    let Some(turn_id) = session.timeline.running_turn_id() else {
        return false;
    };
    let Some(turn) = session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)
    else {
        return false;
    };
    turn.rounds
        .iter()
        .flat_map(|round| &round.blocks)
        .any(|block| {
            block.kind == TimelineBlockKind::Tool && block.state == TimelineBlockState::Open
        })
}

fn agent_layout(app: &App, width: u16, available: u16, theme: &Theme) -> AgentLayout {
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
    let working = session_is_working(session);
    let min_composer = if narrow {
        1
    } else {
        usize::from(theme.spacing.composer_min_height.max(1))
    };
    let max_composer = usize::from(theme.spacing.composer_max_height.max(1)).max(min_composer);
    let preferred_composer = composer_visual_rows(session, width, theme)
        .clamp(min_composer.min(available), max_composer.min(available));
    let preferred_slash = slash_menu_rows(app);
    let preferred_live = if !has_live_transcript(session) {
        0
    } else if narrow {
        NARROW_LIVE_VIEWPORT_ROWS
    } else {
        LIVE_VIEWPORT_ROWS
    };
    let preferred_stream = usize::from(open_assistant_block(session).is_some());
    let preferred_thinking = usize::from(working);
    let preferred_status = usize::from(theme.spacing.status_height.max(1));
    let preferred_shortcuts = if narrow {
        0
    } else {
        usize::from(theme.spacing.shortcuts_height.max(1))
    };

    let mut layout = AgentLayout {
        live_rows: preferred_live,
        slash_rows: preferred_slash,
        stream_rows: preferred_stream,
        thinking_rows: preferred_thinking,
        composer_rows: preferred_composer,
        status_rows: if available >= 2 { preferred_status } else { 0 },
        shortcuts_rows: preferred_shortcuts,
    };

    // 高度不足时按“正文优先、chrome 降级”的顺序收缩。composer 至少保留
    // 一行；只有终端高度连 composer + status 都放不下时才牺牲 status。
    while layout.height() > available {
        if layout.live_rows > 0 {
            layout.live_rows -= 1;
        } else if layout.slash_rows > 0 {
            layout.slash_rows -= 1;
        } else if layout.stream_rows > 0 {
            layout.stream_rows = 0;
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

fn render_agent(app: &App, width: u16, height: u16, theme: &Theme) -> AgentRender {
    let Some(session) = app.active_session() else {
        return render_brand(app, width, height, theme);
    };
    let height = usize::from(height.max(1));

    let layout = agent_layout(app, width, u16::try_from(height).unwrap_or(u16::MAX), theme);

    let mut blocks: Vec<_> = adapter::from_turns(&session.timeline.turns)
        .into_iter()
        .filter(|block| block.state == BlockState::Live)
        .collect();
    // live transcript 只保留运行中的工具；assistant open block 的稳定行由
    // `sync_streaming` 逐行提交，未完成尾行单独走 stream_rows。
    blocks.retain(|block| matches!(block.kind, BlockKind::Tool(_)));
    let live = render_transcript(&blocks, width, theme);
    let live_start = live.len().saturating_sub(layout.live_rows);
    let mut lines: Vec<Line<'static>> = live[live_start..].to_vec();
    while lines.len() < layout.live_rows {
        lines.push(Line::default());
    }

    if let Some(overlay) = app.overlays.last()
        && let Some(last) = lines.last_mut()
    {
        *last = overlay_hint(overlay, theme);
    }

    lines.extend(slash_menu_lines(app, width, theme, layout.slash_rows));
    if layout.stream_rows > 0 {
        lines.push(stream_tail_line(session, width, theme));
    }
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
struct FullscreenView {
    pointer: FullscreenState,
    transcript: FullscreenTranscriptCache,
    body_area: Rect,
    visible_start: usize,
    body_height: u16,
    menu: Option<MessageMenu>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssistantHit {
    turn_id: String,
    block_id: String,
}

impl FullscreenView {
    fn assistant_at(&self, column: u16, row: u16) -> Option<AssistantHit> {
        if column < self.body_area.x
            || column >= self.body_area.x.saturating_add(self.body_area.width)
            || row < self.body_area.y
            || row >= self.body_area.y.saturating_add(self.body_area.height)
        {
            return None;
        }
        let line = self
            .visible_start
            .saturating_add(usize::from(row.saturating_sub(self.body_area.y)));
        self.transcript
            .spans
            .iter()
            .find(|span| span.assistant && line >= span.start && line < span.end)
            .map(|span| AssistantHit {
                turn_id: span.turn_id.clone(),
                block_id: span.block_id.clone(),
            })
    }

    fn open_menu(&mut self, hit: AssistantHit, column: u16, row: u16) {
        self.menu = Some(MessageMenu::new(
            hit.turn_id,
            hit.block_id,
            Position::new(column, row),
        ));
    }

    fn close_menu(&mut self) {
        self.menu = None;
    }
}

impl FullscreenView {
    fn can_scroll(&self) -> bool {
        self.transcript.lines.len() > usize::from(self.body_height)
    }

    fn max_offset(&self) -> usize {
        self.transcript
            .lines
            .len()
            .saturating_sub(usize::from(self.body_height))
    }

    fn scroll_up(&mut self, app: &mut App, lines: usize) {
        if !self.can_scroll() {
            app.scroll_bottom();
            return;
        }
        app.scroll_up(lines);
        self.clamp_scroll(app);
    }

    fn scroll_down(&mut self, app: &mut App, lines: usize) {
        app.scroll_down(lines);
        self.clamp_scroll(app);
    }

    fn page_up(&mut self, app: &mut App) {
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

    fn clamp_scroll(&mut self, app: &mut App) {
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
struct FullscreenTranscriptCache {
    key: Option<FullscreenTranscriptKey>,
    blocks: HashMap<FullscreenBlockKey, Vec<Line<'static>>>,
    spans: Vec<FullscreenBlockSpan>,
    lines: Vec<Line<'static>>,
    #[cfg(test)]
    render_misses: usize,
}

#[derive(Debug, Clone)]
struct FullscreenBlockSpan {
    turn_id: String,
    block_id: String,
    assistant: bool,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FullscreenTranscriptKey {
    seed: String,
    version: u64,
    width: u16,
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
    fn clear(&mut self) {
        self.key = None;
        self.blocks.clear();
        self.spans.clear();
        self.lines.clear();
    }

    fn sync(&mut self, app: &App, width: u16, theme: &Theme) {
        let Some(session) = app.active_session() else {
            self.clear();
            return;
        };
        let key = FullscreenTranscriptKey {
            seed: session.seed.clone(),
            version: session.timeline.version,
            width,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }

        // Live reasoning 仍在 composer 上方单独显示，避免“单行思考链”在历史区
        // 重复；其余 live block（尤其流式 assistant）必须进入全屏历史，否则全屏
        // 模式下只能看到最后一行。
        let blocks: Vec<_> = adapter::from_turns(&session.timeline.turns)
            .into_iter()
            .filter(|block| {
                block.state.is_visible()
                    && !(block.state == BlockState::Live
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
            spans.push(FullscreenBlockSpan {
                turn_id: block.turn_id.clone(),
                block_id: block.id.to_string(),
                assistant: matches!(block.kind, BlockKind::Assistant { .. }),
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

/// 当前 open assistant 的未完成尾行。稳定行已经写进 scrollback，这里只画
/// 最后一行；它每帧可变，因此绝不能再走 `insert_before`。
fn stream_tail_line(session: &SessionState, width: u16, theme: &Theme) -> Line<'static> {
    let Some((_, _, text)) = open_assistant_block(session) else {
        return Line::default();
    };
    let prefix = format!("{} ", theme.glyph.assistant);
    let body_width = usize::from(width)
        .saturating_sub(prefix.width())
        .saturating_sub(1)
        .max(1);
    let tail = strip_ansi_escapes(text.rsplit('\n').next().unwrap_or_default());
    let shown = tail_cols(&tail, body_width);
    let mut spans = vec![Span::styled(
        prefix,
        Style::new().fg(theme.accent.assistant),
    )];
    if let Some(line) = crate::ui::v2::markdown::render(&shown, body_width, theme).last() {
        spans.extend(line.spans.clone());
    }
    spans.push(Span::styled(
        theme.glyph.cursor.to_string(),
        Style::new().fg(theme.accent.assistant),
    ));
    Line::from(spans)
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

fn overlay_hint(overlay: &Overlay, theme: &Theme) -> Line<'static> {
    let text = match overlay {
        Overlay::SessionList { .. } => " 会话列表 · M5 Workspace",
        Overlay::Settings(_) => " 设置 · M5 Workspace",
        Overlay::Help => " 帮助 · M5 Workspace",
        Overlay::History { .. } => " 历史回合 · M5 Workspace",
        Overlay::AttachPath { .. } => " 附件路径 · M5 Modal",
        Overlay::Confirm { .. } => " 确认操作 · M5 Modal",
        Overlay::CwdInput { .. } => " 新会话目录 · M5 Modal",
        Overlay::Thinking { .. } => " 思考回放 · M5 Modal",
    };
    Line::from(Span::styled(text, Style::new().fg(theme.semantic.warning)))
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
    // v2 此前**完全没有 toast 面**：v1 在状态栏中间渲染 `app.toasts`，v2 的
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
        " Enter 发送 · Alt+Enter 换行 · Ctrl+P 模式 · Ctrl+L 会话 · F1 帮助"
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
    use super::*;
    use crate::app::session::{AskPanel, SessionState, StreamPhase, StreamingState};
    use crate::app::timeline_model::TimelineModel;
    use crate::theme::{ColorSupport, ThemeKind};
    use qaqh_client::{
        AskMode, DomainAskQuestion as AskQuestion, TimelineBlock, TimelineBlockKind,
        TimelineBlockState, TimelineEntry, TimelineEvent, TimelineTool, TimelineToolState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    fn test_theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
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

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
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

    fn model_with_live_activity() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "inspect".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "thinking-1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Open,
                    text: "checking the workspace".to_string(),
                    tool: None,
                },
            },
        ));
        model.apply(&entry(
            3,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "tool-1".to_string(),
                    block_order: 1,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: Some(TimelineTool {
                        tool_call_id: "call-1".to_string(),
                        name: "read".to_string(),
                        state: TimelineToolState::Running,
                        summary: Some("src/main.rs".to_string()),
                        args_json: None,
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

    fn model_with_live_answer_after_thinking() -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "question".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "thinking-1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Open,
                    text: "internal reasoning".to_string(),
                    tool: None,
                },
            },
        ));
        model.apply(&entry(
            3,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "answer-1".to_string(),
                    block_order: 1,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: "assistant answer".to_string(),
                    tool: None,
                },
            },
        ));
        model
    }

    fn model_with_live_reasoning(text: &str) -> TimelineModel {
        let mut model = TimelineModel::default();
        model.apply(&entry(
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "question".to_string(),
            },
        ));
        model.apply(&entry(
            2,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "thinking-1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Open,
                    text: text.to_string(),
                    tool: None,
                },
            },
        ));
        model
    }

    #[test]
    fn first_snapshot_replays_once_then_syncs_incrementally() {
        let app = app_with_model(model_with_sealed_answer());
        let mut state = AgentState::default();

        let first = state.sync(&app);
        assert!(first.reset_scrollback);
        assert_eq!(first.pending.len(), 2);

        let second = state.sync(&app);
        assert!(!second.reset_scrollback);
        assert!(second.pending.is_empty());
    }

    #[test]
    fn forced_replay_resets_and_replays_after_scrollback_purge() {
        let app = app_with_model(model_with_sealed_answer());
        let mut state = AgentState::default();
        let first = state.sync(&app);
        assert_eq!(first.pending.len(), 2);

        state.force_replay(&app);
        assert!(state.replay_active);
        assert!(state.pending_commits.is_empty());
        assert!(state.pending_stream_lines.is_empty());

        let replayed = state.sync(&app);
        assert_eq!(replayed.pending.len(), 2, "缩屏重建后必须从 timeline 重放");
    }

    #[test]
    fn streaming_commits_complete_lines_and_flushes_tail_on_seal() {
        let mut model = model_with_live_answer_after_thinking();
        model.apply(&entry(
            4,
            "turn-1",
            TimelineEvent::TextDelta {
                block_id: "answer-1".to_string(),
                fragment_seq: 1,
                delta: "\nsecond".to_string(),
            },
        ));
        let app = app_with_model(model.clone());
        let mut state = AgentState::default();

        let first = state.sync_streaming(&app, 60, &test_theme());
        let first_text = text_of(&first);
        assert!(first_text.contains("assistant answer"), "{first_text}");
        assert!(!first_text.contains("second"), "{first_text}");
        assert!(state.streamed_blocks.is_empty());

        model.apply(&entry(
            5,
            "turn-1",
            TimelineEvent::BlockSealed {
                block_id: "answer-1".to_string(),
            },
        ));
        let sealed_app = app_with_model(model);
        let second = state.sync_streaming(&sealed_app, 60, &test_theme());
        let second_text = text_of(&second);
        assert!(second_text.contains("second"), "{second_text}");
        assert!(
            state
                .streamed_blocks
                .contains(&("turn-1".to_string(), "answer-1".to_string())),
            "已流式提交的 block 必须在 sealed 整体渲染时被抑制"
        );
    }

    #[test]
    fn session_switch_resets_scrollback_and_replays_each_seed() {
        let mut app = app_with_model(model_with_sealed_answer());
        let mut second = TimelineModel::default();
        second.apply(&entry(
            1,
            "turn-2",
            TimelineEvent::TurnOpened {
                user_text: "second session".to_string(),
            },
        ));
        second.apply(&entry(
            2,
            "turn-2",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b2".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Sealed,
                    text: "second answer".to_string(),
                    tool: None,
                },
            },
        ));
        app.tabs.push("seed-2".to_string());
        let mut session = SessionState::new("seed-2".to_string());
        session.timeline = second;
        app.sessions.insert("seed-2".to_string(), session);

        let mut state = AgentState::default();
        let first = state.sync(&app);
        assert!(first.reset_scrollback);
        assert_eq!(first.pending.len(), 2);

        app.active = 1;
        let switched = state.sync(&app);
        assert!(switched.reset_scrollback);
        assert_eq!(switched.pending.len(), 2);
        assert!(
            switched
                .pending
                .iter()
                .all(|pending| pending.seed == "seed-2")
        );

        app.active = 0;
        let switched_back = state.sync(&app);
        assert!(switched_back.reset_scrollback);
        assert_eq!(
            switched_back.pending.len(),
            2,
            "切回旧会话时必须先清 scrollback，因此允许重新 emit"
        );
    }

    #[test]
    fn replay_commits_are_drained_in_bounded_chunks() {
        fn pending(index: usize) -> PendingCommit {
            let mut block = crate::ui::v2::transcript::TranscriptBlock::new(
                format!("b{index}"),
                crate::ui::v2::transcript::BlockKind::Assistant {
                    text: format!("block {index}"),
                },
            )
            .with_turn_id("turn-1");
            assert!(block.seal());
            PendingCommit {
                seed: "seed".into(),
                block,
            }
        }

        let mut state = AgentState::default();
        state.pending_commits.extend((0..70).map(pending));

        assert_eq!(
            state.take_commit_chunk(COMMIT_CHUNK_BLOCKS).len(),
            COMMIT_CHUNK_BLOCKS
        );
        assert_eq!(state.pending_commits.len(), 70 - COMMIT_CHUNK_BLOCKS);
        assert_eq!(
            state.take_commit_chunk(COMMIT_CHUNK_BLOCKS).len(),
            COMMIT_CHUNK_BLOCKS
        );
        assert_eq!(state.pending_commits.len(), 70 - 2 * COMMIT_CHUNK_BLOCKS);
    }

    #[test]
    fn first_replay_is_chunked_across_frames() {
        let app = app_with_model(model_with_many_sealed_turns(20));
        let mut state = AgentState::default();

        let first = state.sync(&app);
        assert!(first.reset_scrollback);
        assert_eq!(first.pending.len(), 16);
        assert!(state.replay_active);

        let second = state.sync(&app);
        assert_eq!(second.pending.len(), 16);
        assert!(state.replay_active);

        let third = state.sync(&app);
        assert_eq!(third.pending.len(), 8);
        assert!(!state.replay_active);

        assert!(state.sync(&app).pending.is_empty());
    }

    #[test]
    fn agent_render_keeps_composer_visible_on_narrow_cjk_input() {
        let mut app = app_with_model(TimelineModel::default());
        let input = "这是一个很长的中文输入，用来验证窄屏横向窗口";
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = input.chars().collect();
        session.composer.cursor = session.composer.input.len();

        let rendered = render_agent(&app, 20, 8, &test_theme());
        assert!(rendered.lines.len() <= 8);
        let cursor = rendered.cursor.expect("composer cursor");
        assert!(cursor.x < 20, "cursor={cursor:?}");
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("横向窗口"), "{text}");
    }

    #[test]
    fn agent_render_sanitizes_control_characters() {
        let mut app = app_with_model(TimelineModel::default());
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = vec!['a', '\u{1b}', 'b'];
        session.composer.cursor = session.composer.input.len();

        let rendered = render_agent(&app, 40, 8, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains('\u{1b}'), "raw escape reached the terminal");
        assert!(text.contains('�'), "{text}");
    }

    #[test]
    fn composer_rows_split_newlines_and_track_cursor() {
        let input: Vec<char> = "ab\ncd".chars().collect();
        let (rows, cursor_row, cursor_col) = build_composer_rows(&input, 3, 20);
        assert_eq!(rows, vec![vec!['a', 'b'], vec!['c', 'd']]);
        assert_eq!(cursor_row, 1);
        assert_eq!(cursor_col, 0);
    }

    #[test]
    fn composer_rows_wrap_wide_text_and_keep_cursor_visible() {
        let input: Vec<char> = "这是很长的中文输入".chars().collect();
        let (rows, cursor_row, cursor_col) = build_composer_rows(&input, input.len(), 6);
        assert!(rows.len() > 1);
        assert_eq!(cursor_row, rows.len() - 1);
        assert!(cursor_col <= 6);

        let mut app = app_with_model(TimelineModel::default());
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = input;
        session.composer.cursor = session.composer.input.len();
        let rendered = render_agent(&app, 20, 8, &test_theme());
        let cursor = rendered.cursor.expect("composer cursor");
        assert!(cursor.y < 8);
    }

    #[test]
    fn brand_page_renders_draft_input_box_and_cursor() {
        let (mut app, _rx) = App::new_for_test();
        app.draft_composer.insert_str("hello 世界");
        let rendered = render_agent(&app, 80, 14, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();

        assert!(text.contains("Q A Q"), "{text}");
        assert!(text.contains("hello 世界"), "{text}");
        assert!(text.contains('╭'), "{text}");
        assert!(
            rendered.cursor.is_some(),
            "draft composer must expose cursor"
        );
        assert!(rendered.lines.len() <= 14);
    }

    #[test]
    fn brand_page_gets_more_than_old_three_row_empty_state() {
        let (app, _rx) = App::new_for_test();
        let height = inline_viewport_height(&app, 80, 40, &test_theme());
        assert!(
            height > 3,
            "brand page needs room for logo + input: {height}"
        );
        assert!(height <= MAX_VIEWPORT_HEIGHT);
    }

    #[test]
    fn dynamic_viewport_grows_for_composer_and_slash_menu() {
        let mut app = app_with_model(TimelineModel::default());
        let theme = test_theme();
        let base = inline_viewport_height(&app, 80, 40, &theme);

        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = "line\n".repeat(8).chars().collect();
        session.composer.cursor = session.composer.input.len();
        let grown = inline_viewport_height(&app, 80, 40, &theme);
        assert!(grown > base, "base={base}, grown={grown}");
        assert!(grown <= MAX_VIEWPORT_HEIGHT, "grown={grown}");

        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = vec!['/'];
        session.composer.cursor = 1;
        let slash = inline_viewport_height(&app, 80, 40, &theme);
        assert!(slash > base, "base={base}, slash={slash}");
        assert!(slash <= MAX_VIEWPORT_HEIGHT, "slash={slash}");
    }

    #[test]
    fn narrow_viewport_hides_shortcuts_and_preserves_composer_tail() {
        let mut app = app_with_model(TimelineModel::default());
        let input = "这是一个很长的中文输入，用来验证窄屏横向窗口";
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = input.chars().collect();
        session.composer.cursor = session.composer.input.len();
        let theme = test_theme();

        let height = inline_viewport_height(&app, 20, 8, &theme);
        assert!(height <= 4, "height={height}");
        let layout = agent_layout(&app, 20, height, &theme);
        assert_eq!(layout.shortcuts_rows, 0);
        assert!(layout.composer_rows >= 1);
        assert_eq!(layout.status_rows, 1);

        let rendered = render_agent(&app, 20, height, &theme);
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("横向窗口"), "{text}");
        assert!(rendered.lines.len() <= usize::from(height));
    }

    #[test]
    fn viewport_height_respects_screen_ratio_and_cap() {
        let app = app_with_model(TimelineModel::default());
        let theme = test_theme();
        for terminal_height in [1, 2, 4, 8, 24, 40, 80] {
            let height = inline_viewport_height(&app, 80, terminal_height, &theme);
            let expected_cap = terminal_height
                .saturating_mul(VIEWPORT_HEIGHT_PERCENT)
                .checked_div(100)
                .unwrap_or(0)
                .clamp(1, MAX_VIEWPORT_HEIGHT)
                .min(terminal_height)
                .max(1);
            assert!(
                height <= expected_cap,
                "screen={terminal_height}, height={height}"
            );
            assert!(height >= 1);
        }
    }

    #[test]
    fn slash_menu_tracks_selection_and_stays_in_viewport() {
        let mut app = app_with_model(TimelineModel::default());
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = vec!['/'];
        session.composer.cursor = 1;
        app.slash_selected = 2;

        let rendered = render_agent(&app, 60, 10, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("/new"), "{text}");
        assert!(text.contains("▸ /sessions"), "{text}");
        assert!(text.contains("/settings"), "{text}");
        assert!(rendered.lines.len() <= 10);
    }

    #[test]
    fn live_thinking_and_tool_cards_render_in_agent_viewport() {
        let app = app_with_model(model_with_live_activity());
        let rendered = render_agent(&app, 60, 10, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("checking the workspace"), "{text}");
        assert!(text.contains("read"), "{text}");
        assert!(text.contains("src/main.rs"), "{text}");
    }

    #[test]
    fn thinking_row_uses_latest_line_and_tail_window() {
        let app = app_with_model(model_with_live_reasoning(
            "first line\nsecond line is intentionally long",
        ));
        let rendered = render_agent(&app, 24, 8, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("long"), "{text}");
        assert!(!text.contains("first line"), "{text}");
    }

    #[test]
    fn thinking_row_keeps_spinner_without_reasoning() {
        let mut app = app_with_model(TimelineModel::default());
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.streaming = Some(StreamingState {
            turn_id: "turn-1".to_string(),
            phase: StreamPhase::ToolCalling,
            round_num: 0,
            tool_name: Some("read".to_string()),
            armed_at: std::time::Instant::now(),
        });

        let rendered = render_agent(&app, 40, 8, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("tool…"), "{text}");
        let thinking = rendered
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .find(|line| line.contains("tool…"))
            .expect("thinking row");
        let first = thinking.chars().next().expect("spinner");
        assert!(
            matches!(first, '·' | '✢' | '✳' | '✶' | '✻' | '✽'),
            "thinking row must keep the working spinner: {thinking:?}"
        );
    }

    #[test]
    fn thinking_tail_keeps_latest_characters_and_wide_chars_intact() {
        assert_eq!(tail_cols("abcdef", 3), "def");
        assert_eq!(tail_cols("你好世界", 4), "世界");
    }

    #[test]
    fn live_answer_tail_and_thinking_use_dedicated_rows() {
        let app = app_with_model(model_with_live_answer_after_thinking());
        let rendered = render_agent(&app, 60, 10, &test_theme());
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.contains("assistant answer"),
            "open assistant 的未完成尾行应在 live viewport 显示：{text}"
        );
        assert!(text.contains('▌'), "流式尾行应带光标：{text}");
        assert!(!text.contains("Thinking…"), "{text}");

        let thinking = rendered
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .find(|line| line.contains("internal reasoning"))
            .expect("thinking row");
        let first = thinking.chars().next().expect("spinner");
        assert!(
            matches!(first, '·' | '✢' | '✳' | '✶' | '✻' | '✽'),
            "thinking row must lead with a working spinner: {thinking:?}"
        );
    }

    #[tokio::test]
    async fn agent_delegates_enter_to_existing_composer_send_path() {
        let mut app = app_with_model(TimelineModel::default());
        let session = app.sessions.get_mut("seed-1").expect("session");
        session.composer.input = "hello".chars().collect();
        session.composer.cursor = session.composer.input.len();

        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        let session = app.sessions.get("seed-1").expect("session");
        assert!(session.composer.is_empty());
    }

    #[test]
    fn agent_draw_routes_pending_ask_to_v2_modal() {
        let mut app = app_with_model(TimelineModel::default());
        app.sessions.get_mut("seed-1").expect("session").pending_ask = Some(AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            AskMode::Single,
            vec![AskQuestion {
                id: "q1".into(),
                question: "选择完整方案".into(),
                options: vec!["方案一".into(), "方案二".into()],
                allow_custom: true,
            }],
        ));
        let theme = test_theme();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(10),
            },
        )
        .expect("inline terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();
        terminal
            .draw(|frame| draw(frame, &app, &theme, &route, ScreenMode::Inline, &mut view))
            .expect("draw ask modal");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("问题1/1"), "{text}");
        assert!(text.contains("选择完整方案"), "{text}");
    }

    #[test]
    fn inline_agent_draw_survives_resize() {
        let app = app_with_model(TimelineModel::default());
        let theme = test_theme();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(10),
            },
        )
        .expect("inline terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        for (width, height) in [(80, 24), (40, 20), (20, 8), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            terminal
                .draw(|frame| draw(frame, &app, &theme, &route, ScreenMode::Inline, &mut view))
                .expect("draw after resize");
        }
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
        let theme = test_theme();
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

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &app,
                    &theme,
                    &route,
                    ScreenMode::Fullscreen,
                    &mut view,
                )
            })
            .expect("draw fullscreen agent");

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
        let theme = test_theme();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView::default();

        for (width, height) in [(80, 24), (40, 20), (20, 8), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &app,
                        &theme,
                        &route,
                        ScreenMode::Fullscreen,
                        &mut view,
                    )
                })
                .expect("draw fullscreen after resize");
        }
    }

    #[test]
    fn fullscreen_context_menu_renders_copy_and_disabled_actions() {
        let mut app = app_with_model(model_with_sealed_answer());
        app.show_workspace = false;
        let theme = test_theme();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        let route = route::resolve(&app);
        let mut view = FullscreenView {
            menu: Some(MessageMenu::new(
                "turn-1".into(),
                "b1".into(),
                Position::new(20, 5),
            )),
            ..Default::default()
        };

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &app,
                    &theme,
                    &route,
                    ScreenMode::Fullscreen,
                    &mut view,
                )
            })
            .expect("draw context menu");

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
    fn fullscreen_assistant_hit_uses_semantic_block_spans() {
        let app = app_with_model(model_with_many_sealed_turns(3));
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcript.sync(&app, 79, &theme);
        view.body_area = Rect::new(0, 0, 80, 10);
        view.visible_start = 0;

        assert_eq!(view.assistant_at(2, 0), None, "row 0 is the user block");
        assert_eq!(
            view.assistant_at(2, 2),
            Some(AssistantHit {
                turn_id: "turn-0".into(),
                block_id: "block-0".into(),
            })
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

    #[test]
    fn screen_transition_enters_and_leaves_alternate_once() {
        let workspace = ScreenRoute::Workspace(route::WorkspaceRoute::Help);
        let modal = ScreenRoute::Modal(route::ModalRoute::Ask);
        assert_eq!(
            screen_transition(ScreenMode::Inline, &ScreenRoute::Agent),
            ScreenTransition::Stay
        );
        assert_eq!(
            screen_transition(ScreenMode::Inline, &workspace),
            ScreenTransition::EnterAlternate
        );
        assert_eq!(
            screen_transition(ScreenMode::Alternate, &workspace),
            ScreenTransition::Stay
        );
        assert_eq!(
            screen_transition(ScreenMode::Alternate, &modal),
            ScreenTransition::Stay,
            "Workspace → Modal 不应重复进出 alternate"
        );
        assert_eq!(
            screen_transition(ScreenMode::Alternate, &ScreenRoute::Agent),
            ScreenTransition::LeaveAlternate
        );
        assert_eq!(
            screen_transition(ScreenMode::Fullscreen, &ScreenRoute::Agent),
            ScreenTransition::Stay
        );
        assert_eq!(
            screen_transition(ScreenMode::Fullscreen, &workspace),
            ScreenTransition::Stay,
            "全屏 shell 在 Agent/Workspace 之间不应退出 alternate"
        );
    }

    #[test]
    fn insert_before_clears_wide_trailing_cells_without_touching_glyph() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 8, 1));
        buffer.set_string(0, 0, "你a", Style::default());
        assert_eq!(buffer.content[0].symbol(), "你");
        assert_eq!(buffer.content[1].symbol(), " ");
        assert_eq!(buffer.content[2].symbol(), "a");

        clear_wide_trailing_cells(&mut buffer);

        assert_eq!(buffer.content[0].symbol(), "你");
        assert_eq!(buffer.content[1].symbol(), "");
        assert_eq!(buffer.content[2].symbol(), "a");
    }
}
