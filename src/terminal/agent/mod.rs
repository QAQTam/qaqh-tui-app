//! V2 Agent shell：共享事件循环、终端生命周期与输入分发。
//!
//! 当前生产默认是 fullscreen（[`fullscreen`]）；inline/scrollback 作为冻结的
//! 兼容分支保留在 [`inline`]，`--v1` 仍走独立旧 UI 路径。
//!
//! 两个 v2 shell 共用 Runtime/App 状态：
//! - inline 把已封口 transcript 经 projector + commit ledger 写入 scrollback；
//! - fullscreen 由 App 自己持有 transcript 视口、滚动和鼠标命中状态。

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
use crate::ui::v2::route::{self, ScreenRoute};
use crate::ui::v2::runtime::V2TranscriptRuntime;
use crate::ui::v2::transcript::{BlockKind, BlockState, TranscriptBlock, render_transcript};
use crate::ui::v2::workspace;
use qaqh_client::{ConversationMode, NoticeLevel, TimelineBlockKind, TimelineBlockState};

mod fullscreen;
mod inline;
use fullscreen::{
    FullscreenView, draw_fullscreen_agent, handle_fullscreen_agent_mouse,
    handle_fullscreen_menu_key,
};
use inline::{
    AgentState, commit_pending, draw_agent, initial_inline_height, inline_viewport_height,
};

const TICK_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SLASH_ROWS: usize = 4;

/// 启动真实 V2 Agent shell。
///
/// 当前 CLI 只从 fullscreen 入口调用；`fullscreen=false` 保留给冻结的 inline
/// 兼容分支与回归测试，不再作为默认启动路径。
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
            (_, ScreenRoute::Workspace(route::WorkspaceRoute::Settings)) => {
                fullscreen_view.pointer.clear_pointer();
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_settings_mouse(app, area, mouse);
            }
            (_, ScreenRoute::Modal(modal)) => {
                fullscreen_view.pointer.clear_pointer();
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_modal_mouse(app, *modal, area, mouse);
            }
            _ => {
                fullscreen_view.pointer.clear_pointer();
                app.settings_hover = None;
                app.settings_pressed = None;
            }
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

fn handle_settings_mouse(
    app: &mut App,
    area: ratatui::layout::Rect,
    mouse: ratatui::crossterm::event::MouseEvent,
) {
    use crate::app::settings::SettingsHit;
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};

    let hit = crate::ui::v2::workspace::settings_hit_test(app, area, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Moved => app.settings_hover = hit,
        MouseEventKind::Down(MouseButton::Left) => {
            app.settings_hover = hit;
            app.settings_pressed = hit;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let pressed = app.settings_pressed.take();
            app.settings_hover = hit;
            if pressed.is_some()
                && pressed == hit
                && let Some(SettingsHit::Row(index)) = hit
            {
                app.mouse_settings_row(index);
            }
        }
        _ => {}
    }
}

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
    use super::fullscreen::{FullscreenTranscriptCache, MessageHit, assistant_markdown};
    use super::inline::{
        COMMIT_CHUNK_BLOCKS, MAX_VIEWPORT_HEIGHT, VIEWPORT_HEIGHT_PERCENT, agent_layout,
        clear_wide_trailing_cells, render_agent,
    };
    use super::*;
    use crate::app::session::{AskPanel, SessionState, StreamPhase, StreamingState};
    use crate::app::timeline_model::TimelineModel;
    use crate::theme::{ColorSupport, ThemeKind};
    use crate::ui::v2::fullscreen::{FullscreenState, MessageMenu, MessageRole};
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
    fn rebaseline_epoch_forces_scrollback_replay() {
        let mut app = app_with_model(model_with_sealed_answer());
        let mut state = AgentState::default();
        let first = state.sync(&app);
        assert_eq!(first.pending.len(), 2);

        let session = app.sessions.get_mut("seed-1").expect("session");
        session.timeline.rebaseline_epoch = session.timeline.rebaseline_epoch.saturating_add(1);
        session.timeline.version = session.timeline.version.saturating_add(1);

        let replayed = state.sync(&app);
        assert!(replayed.reset_scrollback);
        assert_eq!(replayed.pending.len(), 2);
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
                MessageRole::Assistant,
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
    fn fullscreen_assistant_hit_uses_semantic_block_spans() {
        let app = app_with_model(model_with_many_sealed_turns(3));
        let theme = test_theme();
        let mut view = FullscreenView::default();
        view.transcript.sync(&app, 79, &theme);
        view.body_area = Rect::new(0, 0, 80, 10);
        view.visible_start = 0;

        assert_eq!(
            view.message_at(2, 0),
            Some(MessageHit {
                turn_id: "turn-0".into(),
                block_id: "turn-0:user".into(),
                role: MessageRole::User,
            })
        );
        assert_eq!(
            view.message_at(2, 2),
            Some(MessageHit {
                turn_id: "turn-0".into(),
                block_id: "block-0".into(),
                role: MessageRole::Assistant,
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
