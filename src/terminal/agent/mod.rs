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
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, poll, read,
};
use ratatui::crossterm::{event::KeyCode, execute};
use ratatui::layout::{Position, Rect};
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
use crate::ui::v2::route::{self, ScreenRoute};
use crate::ui::v2::transcript::{BlockKind, BlockState, TranscriptBlock};
use crate::ui::v2::workspace;
use qaqh_client::{ConversationMode, NoticeLevel, TimelineBlockKind};

mod fullscreen;
use fullscreen::{
    FullscreenView, draw_fullscreen_agent, handle_fullscreen_agent_mouse,
    handle_fullscreen_menu_key,
};

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
    let mut terminal = TerminalHost::init();
    if let Err(error) = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste) {
        ratatui::restore();
        runtime.shutdown().await;
        return Err(error).context("启用终端输入");
    }

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
                                // Fullscreen shell captures mouse input globally.
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
        }

        if app.force_redraw {
            terminal.terminal.clear()?;
            app.force_redraw = false;
        }
        terminal
            .terminal
            .draw(|frame| draw(frame, app, theme, &route, fullscreen_view))?;
        if route == ScreenRoute::Agent {
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
    // 鼠标由渲染层接管：命中测试需要当前 shell/弹窗几何。
    if let AppMsg::Mouse(mouse) = msg {
        match route {
            ScreenRoute::Agent => {
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_fullscreen_agent_mouse(app, fullscreen_view, area, mouse);
            }
            ScreenRoute::Workspace(workspace_route) => {
                fullscreen_view.pointer.clear_pointer();
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_workspace_mouse(app, workspace_route, area, mouse);
            }
            ScreenRoute::Modal(modal) => {
                fullscreen_view.pointer.clear_pointer();
                let size = terminal.terminal.size()?;
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_modal_mouse(app, *modal, area, mouse);
            }
        }
        return Ok(());
    }
    if *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && matches!(&msg, AppMsg::Paste(_))
    {
        return Ok(());
    }
    if *route == ScreenRoute::Agent
        && fullscreen_view.menu.is_some()
        && let AppMsg::Key(key) = &msg
    {
        handle_fullscreen_menu_key(app, fullscreen_view, key);
        return Ok(());
    }
    if let AppMsg::Key(key) = &msg
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
        app.workspace_hover = None;
        app.workspace_pressed = None;
        app.show_workspace = false;
        return Ok(());
    }
    if matches!(msg, AppMsg::Key(_)) {
        app.workspace_hover = None;
        app.workspace_pressed = None;
    }
    app.handle(msg);
    Ok(())
}

fn handle_workspace_mouse(
    app: &mut App,
    route: &route::WorkspaceRoute,
    area: ratatui::layout::Rect,
    mouse: ratatui::crossterm::event::MouseEvent,
) {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};

    let hit =
        crate::ui::v2::workspace::workspace_hit_test(app, route, area, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Moved => app.workspace_hover = hit,
        MouseEventKind::Down(MouseButton::Left) => {
            app.workspace_hover = hit;
            app.workspace_pressed = hit;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let pressed = app.workspace_pressed.take();
            app.workspace_hover = hit;
            if let (Some(pressed), Some(released)) = (pressed, hit)
                && pressed == released
            {
                dispatch_workspace_hit(app, pressed);
                app.workspace_hover = None;
                app.workspace_pressed = None;
            }
        }
        MouseEventKind::ScrollUp => {
            handle_workspace_scroll(app, route, true);
            app.workspace_hover = crate::ui::v2::workspace::workspace_hit_test(
                app,
                route,
                area,
                mouse.column,
                mouse.row,
            );
            app.workspace_pressed = None;
        }
        MouseEventKind::ScrollDown => {
            handle_workspace_scroll(app, route, false);
            app.workspace_hover = crate::ui::v2::workspace::workspace_hit_test(
                app,
                route,
                area,
                mouse.column,
                mouse.row,
            );
            app.workspace_pressed = None;
        }
        _ => {}
    }
}

fn handle_workspace_scroll(app: &mut App, route: &route::WorkspaceRoute, up: bool) {
    match route {
        route::WorkspaceRoute::Sessions { .. }
        | route::WorkspaceRoute::Settings
        | route::WorkspaceRoute::History { detail: false, .. } => {
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
        WorkspaceHit::SettingsRow(index) => app.mouse_settings_row(index),
        WorkspaceHit::Back => app.workspace_back(),
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

struct TerminalHost {
    terminal: DefaultTerminal,
    last_terminal_size: (u16, u16),
}

impl TerminalHost {
    /// alternate-screen fullscreen shell.
    ///
    /// `ratatui::init()` enables raw mode, enters the alternate screen and installs
    /// the panic restore hook. Mouse capture is enabled separately after init so
    /// initialization failures still restore the terminal.
    fn init() -> Self {
        let terminal = ratatui::init();
        let terminal_size = ratatui::crossterm::terminal::size().unwrap_or((0, 0));
        Self {
            terminal,
            last_terminal_size: terminal_size,
        }
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
        execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        let _ = self.terminal.clear();
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

fn draw(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    route: &ScreenRoute,
    fullscreen_view: &mut FullscreenView,
) {
    match route {
        ScreenRoute::Agent => draw_fullscreen_agent(frame, app, theme, fullscreen_view),
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
    use super::*;
    use crate::app::Overlay;
    use crate::app::session::SessionState;
    use crate::app::timeline_model::TimelineModel;
    use crate::theme::{ColorSupport, ThemeKind};
    use crate::ui::v2::fullscreen::{FullscreenState, MessageMenu, MessageRole};
    use qaqh_client::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
            .draw(|frame| draw(frame, &app, &theme, &route, &mut view))
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
                .draw(|frame| draw(frame, &app, &theme, &route, &mut view))
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
            .draw(|frame| draw(frame, &app, &theme, &route, &mut view))
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
}
