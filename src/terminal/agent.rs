//! V2 Agent View：真实 Runtime/App 状态驱动的 inline 外壳（M4.1）。
//!
//! 运行：`qaqh-tui --v2-agent`
//!
//! 与 `--v2-inline` 原型的区别：
//! - 复用生产 `Runtime` / `App`，因此会连接 daemon 并处理真实 timeline 事件；
//! - 已封口 transcript 经 V2 projector + commit ledger 写入终端 scrollback；
//! - inline viewport 只绘制 live transcript、composer、status 与 shortcuts；
//! - 不启用鼠标捕获，保留终端原生选择/复制；v1 默认全屏路径不受影响。

use std::collections::VecDeque;
use std::io::stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind, poll, read,
};
use ratatui::crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{event::KeyCode, execute};
use ratatui::layout::Position;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::timeline_model::Turn;
use crate::app::{App, AppMsg, ConnPhase, Overlay};
use crate::runtime::{Runtime, RuntimeMsg};
use crate::terminal::transcript::PendingCommit;
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::route::{self, ScreenRoute};
use crate::ui::v2::runtime::V2TranscriptRuntime;
use crate::ui::v2::transcript::{BlockState, render_transcript};
use crate::ui::v2::workspace;
use qaqh_client::ConversationMode;

const TICK_INTERVAL: Duration = Duration::from_millis(200);
const COMMIT_CHUNK_BLOCKS: usize = 32;
const MAX_SLASH_ROWS: usize = 4;
const MAX_VIEWPORT_HEIGHT: u16 = 16;
const LIVE_VIEWPORT_ROWS: usize = 4;
const NARROW_LIVE_VIEWPORT_ROWS: usize = 2;
const NARROW_VIEWPORT_WIDTH: u16 = 40;
const VIEWPORT_HEIGHT_PERCENT: u16 = 60;

/// 启动真实 V2 Agent View。
pub async fn run(no_spawn: bool) -> Result<()> {
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
    let theme = Theme::current();
    let initial_height = initial_inline_height(&app, theme);
    let mut terminal = TerminalHost::init(initial_height);
    if let Err(error) = execute!(stdout(), EnableBracketedPaste) {
        ratatui::restore();
        runtime.shutdown().await;
        return Err(error).context("启用括号粘贴");
    }

    let mut input = InputPump::new(app_tx.clone());
    spawn_tick(app_tx.clone());

    let mut agent = AgentState::default();

    let result = run_loop(
        &mut terminal,
        &mut input,
        &mut app_rx,
        &mut app,
        &mut agent,
        theme,
    )
    .await;

    input.suspend().await;
    let _ = execute!(stdout(), DisableBracketedPaste);
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
    theme: &'static Theme,
) -> Result<()> {
    loop {
        if app.quit {
            break;
        }
        let route = route::resolve(app);
        let size = terminal.terminal.size()?;
        let desired_height = inline_viewport_height(app, size.width, size.height, theme);
        terminal.set_inline_height(desired_height);
        reconcile_screen(terminal, input, &route, app, agent, theme).await?;
        if route == ScreenRoute::Agent && terminal.needs_inline_rebuild() {
            input.suspend().await;
            let result = terminal.ensure_inline_height();
            input.resume();
            result?;
        }
        terminal
            .terminal
            .draw(|frame| draw(frame, app, theme, &route))?;

        let Some(msg) = app_rx.recv().await else {
            break;
        };
        handle_message(app, msg);
        while let Ok(msg) = app_rx.try_recv() {
            handle_message(app, msg);
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

fn handle_message(app: &mut App, msg: AppMsg) {
    if let AppMsg::Key(key) = &msg
        && key.code == KeyCode::Esc
        && route::resolve(app) == ScreenRoute::Workspace(route::WorkspaceRoute::Todo)
    {
        app.show_workspace = false;
        return;
    }
    app.handle(msg);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenMode {
    Inline,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenTransition {
    Stay,
    EnterAlternate,
    LeaveAlternate,
}

fn screen_transition(mode: ScreenMode, route: &ScreenRoute) -> ScreenTransition {
    match (mode, route) {
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
    inline_height: u16,
    desired_inline_height: u16,
}

impl TerminalHost {
    fn init(inline_height: u16) -> Self {
        let inline_height = inline_height.max(1);
        Self {
            terminal: ratatui::init_with_options(TerminalOptions {
                viewport: Viewport::Inline(inline_height),
            }),
            mode: ScreenMode::Inline,
            inline_height,
            desired_inline_height: inline_height,
        }
    }

    fn set_inline_height(&mut self, height: u16) {
        self.desired_inline_height = height.max(1);
    }

    fn needs_inline_rebuild(&self) -> bool {
        self.mode == ScreenMode::Inline && self.inline_height != self.desired_inline_height
    }

    /// 在当前 inline viewport 高度与布局需求不一致时重建 viewport。
    ///
    /// 只清理旧 viewport 区域，并把它锚定在原来的顶部；不会触碰已提交到
    /// scrollback 的内容，也不会重放 transcript。
    fn ensure_inline_height(&mut self) -> Result<()> {
        if !self.needs_inline_rebuild() {
            return Ok(());
        }
        self.rebuild_inline(self.desired_inline_height)
    }

    fn rebuild_inline(&mut self, height: u16) -> Result<()> {
        let height = height.max(1);
        let old_area = self.terminal.get_frame().area();
        self.terminal.clear()?;
        self.terminal
            .set_cursor_position(Position::new(0, old_area.y))?;
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        self.inline_height = height;
        self.desired_inline_height = height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    fn enter_alternate(&mut self) -> Result<()> {
        execute!(stdout(), EnterAlternateScreen)?;
        self.terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        self.mode = ScreenMode::Alternate;
        Ok(())
    }

    fn leave_alternate(&mut self) -> Result<()> {
        let inline_height = self.desired_inline_height.max(1);
        execute!(stdout(), LeaveAlternateScreen)?;
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(inline_height),
            },
        )?;
        self.inline_height = inline_height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    /// 清空屏幕与终端 scrollback，并把 inline viewport 重新锚定到顶部。
    ///
    /// 只用于会话切换：旧会话的历史必须从终端历史里移除，否则新会话只能被
    /// 追加到旧历史后面，无法满足“清屏 + 按 ledger 顺序重放”。调用后由
    /// `commit_pending` 写入新 seed 的完整已封口快照。
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
        self.inline_height = inline_height;
        self.mode = ScreenMode::Inline;
        Ok(())
    }

    /// 挂起 TUI → 外部分页器 → 按原屏幕模式恢复。
    fn run_pager(&mut self, text: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("qaqh-pager-{}.md", std::process::id()));
        if std::fs::write(&path, text).is_err() {
            return Ok(());
        }

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

        let inline_height = self.desired_inline_height.max(1);
        self.terminal = ratatui::init_with_options(TerminalOptions {
            viewport: Viewport::Inline(inline_height),
        });
        self.inline_height = inline_height;
        self.mode = ScreenMode::Inline;
        execute!(stdout(), EnableBracketedPaste)?;
        if was_alternate {
            self.enter_alternate()?;
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
    replay_active: bool,
    replay_cursor: usize,
    replay_version: Option<u64>,
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
    let sync = agent.sync(app);
    if sync.reset_scrollback {
        input.suspend().await;
        let result = host.purge_scrollback_for_replay();
        input.resume();
        result?;
    }
    agent.pending_commits.extend(sync.pending);
    let chunk = agent.take_commit_chunk(COMMIT_CHUNK_BLOCKS);
    if chunk.is_empty() {
        return Ok(());
    }

    let width = host.terminal.get_frame().area().width;
    let blocks: Vec<_> = chunk.into_iter().map(|item| item.block).collect();
    let mut lines = render_transcript(&blocks, width, theme);
    if lines.is_empty() {
        lines.push(Line::default());
    }
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    host.terminal.insert_before(height, |buffer| {
        Paragraph::new(lines).render(buffer.area, buffer);
    })?;
    Ok(())
}

fn draw(frame: &mut Frame, app: &App, theme: &Theme, route: &ScreenRoute) {
    match route {
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

struct AgentRender {
    lines: Vec<Line<'static>>,
    cursor: Option<Position>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentLayout {
    live_rows: usize,
    slash_rows: usize,
    composer_rows: usize,
    status_rows: usize,
    shortcuts_rows: usize,
}

impl AgentLayout {
    fn height(self) -> usize {
        self.live_rows
            .saturating_add(self.slash_rows)
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
        return max_height.clamp(1, 3);
    }

    u16::try_from(agent_layout(app, width, max_height, theme).height())
        .unwrap_or(u16::MAX)
        .min(max_height)
        .max(1)
}

fn agent_layout(app: &App, width: u16, available: u16, theme: &Theme) -> AgentLayout {
    let available = usize::from(available.max(1));
    let Some(session) = app.active_session() else {
        return AgentLayout {
            live_rows: 0,
            slash_rows: 0,
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
    let preferred_slash = slash_menu_rows(app);
    let preferred_live = if narrow {
        NARROW_LIVE_VIEWPORT_ROWS
    } else {
        LIVE_VIEWPORT_ROWS
    };
    let preferred_status = usize::from(theme.spacing.status_height.max(1));
    let preferred_shortcuts = if narrow {
        0
    } else {
        usize::from(theme.spacing.shortcuts_height.max(1))
    };

    let mut layout = AgentLayout {
        live_rows: preferred_live,
        slash_rows: preferred_slash,
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
        } else if layout.shortcuts_rows > 0 {
            layout.shortcuts_rows = 0;
        } else if layout.composer_rows > 1 {
            layout.composer_rows -= 1;
        } else if layout.status_rows > 0 {
            layout.status_rows = 0;
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
    let height = usize::from(height.max(1));
    let Some(session) = app.active_session() else {
        let mut lines = vec![
            Line::from(Span::styled(
                " QAQH Agent View",
                Style::new().fg(theme.accent.assistant),
            )),
            Line::default(),
            Line::from(Span::styled(
                " Ctrl+N 新建会话 · Ctrl+L 会话列表 · F1 帮助 · Ctrl+Q 退出",
                Style::new().fg(theme.text.dim),
            )),
        ];
        lines.truncate(height);
        return AgentRender {
            lines,
            cursor: None,
        };
    };

    let layout = agent_layout(app, width, u16::try_from(height).unwrap_or(u16::MAX), theme);

    let blocks: Vec<_> = adapter::from_turns(&session.timeline.turns)
        .into_iter()
        .filter(|block| block.state == BlockState::Live)
        .collect();
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
    if let Some(session) = app.active_session() {
        spans.push(Span::styled(
            format!(" · {}", session.activity_label()),
            Style::new().fg(theme.text.secondary),
        ));
        if width >= 40 {
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
        if width >= 70
            && let Some(cwd) = app.effective_cwd(None)
        {
            spans.push(Span::styled(
                format!(" · {}", crate::app::truncate_str(&cwd, 28)),
                Style::new().fg(theme.text.dim),
            ));
        }
        if width >= 50 {
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
            spans.push(Span::styled(
                format!(" · {}", crate::app::truncate_str(error, 28)),
                Style::new().fg(theme.semantic.warning),
            ));
        }
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
    use crate::app::session::{AskPanel, SessionState};
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
        terminal
            .draw(|frame| draw(frame, &app, &theme, &route))
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

        for (width, height) in [(80, 24), (40, 20), (20, 8), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            terminal
                .draw(|frame| draw(frame, &app, &theme, &route))
                .expect("draw after resize");
        }
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
    }
}
