//! V2 Agent View：真实 Runtime/App 状态驱动的 inline 外壳（M4.1）。
//!
//! 运行：`qaqh-tui --v2-agent`
//!
//! 与 `--v2-inline` 原型的区别：
//! - 复用生产 `Runtime` / `App`，因此会连接 daemon 并处理真实 timeline 事件；
//! - 已封口 transcript 经 V2 projector + commit ledger 写入终端 scrollback；
//! - inline viewport 只绘制 live transcript、composer、status 与 shortcuts；
//! - 不启用鼠标捕获，保留终端原生选择/复制；v1 默认全屏路径不受影响。

use std::io::stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Position;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, AppMsg, ConnPhase, Overlay};
use crate::runtime::{Runtime, RuntimeMsg};
use crate::terminal::transcript::PendingCommit;
use crate::theme::Theme;
use crate::ui::v2::adapter;
use crate::ui::v2::runtime::V2TranscriptRuntime;
use crate::ui::v2::transcript::{BlockState, render_transcript};
use qaqh_client::ConversationMode;

const VIEWPORT_HEIGHT: u16 = 10;
const TICK_INTERVAL: Duration = Duration::from_millis(200);
const COMMIT_CHUNK_BLOCKS: usize = 32;
const MAX_COMPOSER_ROWS: usize = 4;
const MAX_SLASH_ROWS: usize = 4;

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

    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Inline(VIEWPORT_HEIGHT),
    });
    if let Err(error) = execute!(stdout(), EnableBracketedPaste) {
        ratatui::restore();
        runtime.shutdown().await;
        return Err(error).context("启用括号粘贴");
    }

    spawn_input(app_tx.clone());
    spawn_tick(app_tx.clone());

    let mut app = App::new(runtime.clone(), app_tx);
    app.fetch_session_list();
    let mut agent = AgentState::default();
    let theme = Theme::current();

    let result = run_loop(&mut terminal, &mut app_rx, &mut app, &mut agent, theme).await;

    let _ = execute!(stdout(), DisableBracketedPaste);
    runtime.shutdown().await;
    ratatui::restore();
    result
}

fn spawn_input(tx: mpsc::UnboundedSender<AppMsg>) {
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        while let Some(event) = reader.next().await {
            let msg = match event {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => AppMsg::Key(key),
                Ok(Event::Paste(text)) => AppMsg::Paste(text),
                Ok(Event::Resize(_, _)) => AppMsg::Resize,
                Ok(_) => continue,
                Err(_) => break,
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
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
    terminal: &mut DefaultTerminal,
    app_rx: &mut mpsc::UnboundedReceiver<AppMsg>,
    app: &mut App,
    agent: &mut AgentState,
    theme: &'static Theme,
) -> Result<()> {
    loop {
        if app.quit {
            break;
        }
        terminal.draw(|frame| draw(frame, app, theme))?;

        let Some(msg) = app_rx.recv().await else {
            break;
        };
        app.handle(msg);
        while let Ok(msg) = app_rx.try_recv() {
            app.handle(msg);
            if app.quit {
                break;
            }
        }
        commit_pending(terminal, app, agent, theme)?;
    }
    Ok(())
}

#[derive(Debug, Default)]
struct AgentState {
    transcript: V2TranscriptRuntime,
    seed: Option<String>,
}

impl AgentState {
    /// 把当前活动会话的 timeline 同步为待提交块。
    ///
    /// 首次进入一个 seed 时先以权威快照重建 projector 的“已见”状态，再全量
    /// 重放；ledger 会拒绝已经写进 scrollback 的块。之后只做增量投影。
    fn sync(&mut self, app: &App) -> Vec<PendingCommit> {
        let Some(seed) = app.active_seed() else {
            self.seed = None;
            return Vec::new();
        };
        let Some(session) = app.sessions.get(&seed) else {
            self.seed = None;
            return Vec::new();
        };

        if self.seed.as_deref() != Some(seed.as_str()) {
            self.seed = Some(seed.clone());
            self.transcript.reset_from_turns(&session.timeline.turns);
            return self.transcript.replay_all(&seed, &session.timeline.turns);
        }

        let mut pending = Vec::new();
        for turn in &session.timeline.turns {
            pending.extend(self.transcript.sync_turn(&seed, turn));
        }
        pending
    }
}

fn commit_pending(
    terminal: &mut DefaultTerminal,
    app: &App,
    agent: &mut AgentState,
    theme: &Theme,
) -> Result<()> {
    let pending = agent.sync(app);
    if pending.is_empty() {
        return Ok(());
    }

    let width = terminal.get_frame().area().width;
    for chunk in pending.chunks(COMMIT_CHUNK_BLOCKS) {
        let blocks: Vec<_> = chunk.iter().map(|item| item.block.clone()).collect();
        let mut lines = render_transcript(&blocks, width, theme);
        if lines.is_empty() {
            lines.push(Line::default());
        }
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        terminal.insert_before(height, |buffer| {
            Paragraph::new(lines).render(buffer.area, buffer);
        })?;
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
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

    // 底部固定 status + shortcuts；其余空间优先给 composer，再给 slash 菜单，
    // 最后才给 live transcript。窄屏不会把输入区挤出 viewport。
    let footer = 2usize;
    let body_capacity = height.saturating_sub(footer);
    let slash_capacity = slash_menu_capacity(app, body_capacity);
    let composer_capacity =
        composer_capacity(session, body_capacity.saturating_sub(slash_capacity));
    let live_capacity = body_capacity
        .saturating_sub(slash_capacity)
        .saturating_sub(composer_capacity);

    let blocks: Vec<_> = adapter::from_turns(&session.timeline.turns)
        .into_iter()
        .filter(|block| block.state == BlockState::Live)
        .collect();
    let live = render_transcript(&blocks, width, theme);
    let live_start = live.len().saturating_sub(live_capacity);
    let mut lines: Vec<Line<'static>> = live[live_start..].to_vec();
    while lines.len() < live_capacity {
        lines.push(Line::default());
    }

    if let Some(overlay) = app.overlays.last()
        && let Some(last) = lines.last_mut()
    {
        *last = overlay_hint(overlay, theme);
    }

    lines.extend(slash_menu_lines(app, width, theme, slash_capacity));
    let composer_start = lines.len();
    let composer = composer_lines(
        &session.composer.input,
        session.composer.cursor,
        width,
        theme,
        composer_capacity,
    );
    lines.extend(composer.lines);
    lines.push(status_line(app, width, theme));
    lines.push(shortcuts_line(app, width, theme));
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

fn composer_capacity(session: &crate::app::session::SessionState, available: usize) -> usize {
    let input_rows = session
        .composer
        .input
        .iter()
        .filter(|ch| **ch == '\n')
        .count()
        .saturating_add(1);
    input_rows.clamp(1, MAX_COMPOSER_ROWS).min(available.max(1))
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

fn slash_menu_capacity(app: &App, available: usize) -> usize {
    if !app.overlays.is_empty() || app.inspecting() {
        return 0;
    }
    app.slash_candidates()
        .len()
        .min(MAX_SLASH_ROWS)
        .min(available.saturating_sub(1))
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
    use crate::app::session::SessionState;
    use crate::app::timeline_model::TimelineModel;
    use crate::theme::{ColorSupport, ThemeKind};
    use qaqh_client::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
        TimelineTool, TimelineToolState,
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

        assert_eq!(state.sync(&app).len(), 2);
        assert!(state.sync(&app).is_empty());
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
        assert!(text.contains("▸ /clear"), "{text}");
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
    fn inline_agent_draw_survives_resize() {
        let app = app_with_model(TimelineModel::default());
        let theme = test_theme();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .expect("inline terminal");

        for (width, height) in [(80, 24), (40, 20), (20, 8), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            terminal
                .draw(|frame| draw(frame, &app, &theme))
                .expect("draw after resize");
        }
    }
}
