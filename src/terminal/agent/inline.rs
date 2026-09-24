//! V2 inline/scrollback agent shell.
//!
//! Owns the commit ledger projection, terminal scrollback commits and the
//! inline viewport renderer. Fullscreen rendering lives in the sibling module.

use super::*;

pub(super) const COMMIT_CHUNK_BLOCKS: usize = 32;
pub(super) const MAX_VIEWPORT_HEIGHT: u16 = 16;
const LIVE_VIEWPORT_ROWS: usize = 4;
const NARROW_LIVE_VIEWPORT_ROWS: usize = 2;
pub(super) const NARROW_VIEWPORT_WIDTH: u16 = 40;
pub(super) const VIEWPORT_HEIGHT_PERCENT: u16 = 60;
#[derive(Debug, Default)]
pub(super) struct AgentState {
    pub(super) transcript: V2TranscriptRuntime,
    pub(super) seed: Option<String>,
    pub(super) pending_commits: VecDeque<PendingCommit>,
    /// 已稳定、但必须等更早的 pending block 提交完后才能写 scrollback 的流式行。
    pub(super) pending_stream_lines: VecDeque<Line<'static>>,
    streaming: StreamingCommitState,
    pub(super) streamed_blocks: HashSet<(String, String)>,
    pub(super) replay_active: bool,
    pub(super) replay_cursor: usize,
    pub(super) replay_version: Option<u64>,
    pub(super) rebaseline_epoch: Option<u64>,
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
pub(super) struct AgentSync {
    pub(super) pending: Vec<PendingCommit>,
    pub(super) reset_scrollback: bool,
}

impl AgentState {
    /// 把当前活动会话的 timeline 同步为待提交块。
    ///
    /// 首次进入一个 seed 时先以权威快照重建 projector 的“已见”状态，再全量
    /// 重放；ledger 会拒绝已经写进 scrollback 的块。之后只做增量投影。
    pub(super) fn sync(&mut self, app: &App) -> AgentSync {
        let Some(seed) = app.active_seed() else {
            let reset_scrollback = self.seed.is_some();
            self.seed = None;
            self.transcript.clear();
            self.pending_commits.clear();
            self.reset_streaming();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            self.rebaseline_epoch = None;
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
            self.rebaseline_epoch = None;
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
            self.rebaseline_epoch = Some(session.timeline.rebaseline_epoch);
            return AgentSync {
                pending: self.replay_chunk(
                    &seed,
                    &session.timeline.turns,
                    session.timeline.version,
                ),
                reset_scrollback: true,
            };
        }

        if self.rebaseline_epoch != Some(session.timeline.rebaseline_epoch) {
            self.pending_commits.clear();
            self.reset_streaming();
            self.transcript.begin_replay(&seed);
            self.replay_active = true;
            self.replay_cursor = 0;
            self.replay_version = Some(session.timeline.version);
            self.rebaseline_epoch = Some(session.timeline.rebaseline_epoch);
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
    pub(super) fn force_replay(&mut self, app: &App) {
        self.pending_commits.clear();
        self.reset_streaming();
        let Some(seed) = app.active_seed() else {
            self.seed = None;
            self.transcript.clear();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            self.rebaseline_epoch = None;
            return;
        };
        let Some(session) = app.sessions.get(&seed) else {
            self.seed = None;
            self.transcript.clear();
            self.replay_active = false;
            self.replay_cursor = 0;
            self.replay_version = None;
            self.rebaseline_epoch = None;
            return;
        };
        self.seed = Some(seed.clone());
        self.transcript.begin_replay(&seed);
        self.replay_active = true;
        self.replay_cursor = 0;
        self.replay_version = Some(session.timeline.version);
        self.rebaseline_epoch = Some(session.timeline.rebaseline_epoch);
    }

    fn was_streamed(&self, block: &TranscriptBlock) -> bool {
        self.streamed_blocks
            .contains(&(block.turn_id.clone(), block.id.to_string()))
    }

    /// 把当前 open assistant 的**完整行**变成可提交行；最后一行留在 live tail。
    pub(super) fn sync_streaming(
        &mut self,
        app: &App,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
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

    pub(super) fn take_commit_chunk(&mut self, max: usize) -> Vec<PendingCommit> {
        let count = max.min(self.pending_commits.len());
        self.pending_commits.drain(..count).collect()
    }
}

pub(super) async fn commit_pending(
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
pub(super) fn clear_wide_trailing_cells(buffer: &mut ratatui::buffer::Buffer) {
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

pub(super) fn draw_agent(frame: &mut Frame, app: &App, theme: &Theme) {
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

pub(super) fn initial_inline_height(app: &App, theme: &Theme) -> u16 {
    let (width, height) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    inline_viewport_height(app, width, height, theme)
}

/// 计算 inline viewport 的目标高度。
///
/// 高度由实际布局需求推导，并受终端高度的 60% 与绝对上限约束。终端过矮时，
/// 先压缩 live/slash，再隐藏 shortcuts，最后才压缩 composer 与 status。
pub(super) fn inline_viewport_height(
    app: &App,
    width: u16,
    terminal_height: u16,
    theme: &Theme,
) -> u16 {
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
pub(super) fn render_brand(app: &App, width: u16, height: u16, theme: &Theme) -> AgentRender {
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

pub(super) fn agent_layout(app: &App, width: u16, available: u16, theme: &Theme) -> AgentLayout {
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

pub(super) fn render_agent(app: &App, width: u16, height: u16, theme: &Theme) -> AgentRender {
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
