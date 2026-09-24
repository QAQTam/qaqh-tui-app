//! V2 Transcript block 模型与主题化渲染。
//!
//! M3 的第一层刻意与 V1 `render_transcript` 解耦：
//! - V1 的 `RenderLine/SpanStyle` 继续服务旧全屏路径；
//! - V2 直接输出 ratatui `Line`，颜色只从 [`Theme`] token 取；
//! - block 状态先冻结，后续由 commit ledger 接管 `Sealed -> Committed`。
//!
//! 该模块暂不读取 wire 类型；M3 后续 adapter 负责把 `Turn/Block/ToolCard`
//! 投影到这里的 view model，避免在渲染层混入后端契约。

#![allow(dead_code)] // M3 逐层接线；先冻结模型和渲染口径。

use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::wrap_text;
use crate::app::timeline_model::strip_ansi_escapes;
use crate::theme::Theme;

const MIN_WIDTH: usize = 20;
const TOOL_BODY_EDGE: usize = 3;
const TOOL_BODY_RUNNING_TAIL: usize = 6;

/// 稳定块身份。后续映射到 `CommitId.block_id`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(String);

impl BlockId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for BlockId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for BlockId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// 块在终端提交协议中的生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Live,
    Sealed,
    Committed,
    Discarded,
}

impl BlockState {
    pub const fn is_mutable(self) -> bool {
        matches!(self, Self::Live)
    }

    pub const fn is_visible(self) -> bool {
        !matches!(self, Self::Discarded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Prepared,
    Running,
    Success,
    Failed,
    Cancelled,
    Backgrounded,
}

impl ToolState {
    const fn is_running(self) -> bool {
        matches!(self, Self::Prepared | Self::Running | Self::Backgrounded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemLevel {
    Info,
    Warning,
    Error,
}

/// ToolBlock 的 V2 view model。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    pub name: String,
    pub summary: Option<String>,
    pub state: ToolState,
    pub output: Option<String>,
    pub diff: Option<String>,
    pub progress: Option<String>,
    pub failure: Option<String>,
    pub duration: Option<Duration>,
    pub bytes: Option<u64>,
}

impl ToolBlock {
    pub fn new(name: impl Into<String>, state: ToolState) -> Self {
        Self {
            name: name.into(),
            summary: None,
            state,
            output: None,
            diff: None,
            progress: None,
            failure: None,
            duration: None,
            bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
        duration: Option<Duration>,
    },
    Tool(ToolBlock),
    System {
        text: String,
        level: SystemLevel,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBlock {
    pub id: BlockId,
    pub turn_id: String,
    pub revision: u64,
    pub state: BlockState,
    pub kind: BlockKind,
}

impl TranscriptBlock {
    pub fn new(id: impl Into<BlockId>, kind: BlockKind) -> Self {
        Self {
            id: id.into(),
            turn_id: String::new(),
            revision: 1,
            state: BlockState::Live,
            kind,
        }
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = turn_id.into();
        self
    }

    pub fn with_state(mut self, state: BlockState) -> Self {
        self.state = state;
        self
    }

    pub fn seal(&mut self) -> bool {
        if self.state != BlockState::Live {
            return false;
        }
        self.state = BlockState::Sealed;
        true
    }

    pub fn commit(&mut self) -> bool {
        if self.state != BlockState::Sealed {
            return false;
        }
        self.state = BlockState::Committed;
        true
    }

    pub fn discard(&mut self) -> bool {
        if self.state == BlockState::Discarded {
            return false;
        }
        self.state = BlockState::Discarded;
        true
    }

    /// 替换纯文本块内容；工具块不走此入口。
    ///
    /// 封口后拒绝修改，防止 `Sealed` 内容在重放时发生静默变化。
    pub fn replace_text(&mut self, value: impl Into<String>) -> bool {
        if !self.state.is_mutable() {
            return false;
        }
        let value = value.into();
        match &mut self.kind {
            BlockKind::User { text }
            | BlockKind::Assistant { text }
            | BlockKind::Thinking { text, .. }
            | BlockKind::System { text, .. } => {
                if *text == value {
                    return false;
                }
                *text = value;
                self.revision = self.revision.saturating_add(1);
                true
            }
            BlockKind::Tool(_) => false,
        }
    }

    /// 生成不含 ANSI 的稳定内容指纹，供 commit ledger 冲突检测使用。
    pub fn content_fingerprint(&self) -> String {
        let mut out = String::new();
        push_field(&mut out, self.id.as_str());
        push_field(&mut out, &self.turn_id);
        push_field(&mut out, &self.revision.to_string());
        push_field(&mut out, &format!("{:?}", self.state));
        match &self.kind {
            BlockKind::User { text } => {
                push_field(&mut out, "user");
                push_field(&mut out, text);
            }
            BlockKind::Assistant { text } => {
                push_field(&mut out, "assistant");
                push_field(&mut out, text);
            }
            BlockKind::Thinking { text, duration } => {
                push_field(&mut out, "thinking");
                push_field(&mut out, text);
                push_field(&mut out, &format!("{duration:?}"));
            }
            BlockKind::Tool(tool) => {
                push_field(&mut out, "tool");
                push_field(&mut out, &tool.name);
                push_field(&mut out, tool.summary.as_deref().unwrap_or(""));
                push_field(&mut out, &format!("{:?}", tool.state));
                push_field(&mut out, tool.output.as_deref().unwrap_or(""));
                push_field(&mut out, tool.diff.as_deref().unwrap_or(""));
                push_field(&mut out, tool.progress.as_deref().unwrap_or(""));
                push_field(&mut out, tool.failure.as_deref().unwrap_or(""));
                push_field(&mut out, &format!("{:?}", tool.duration));
                push_field(&mut out, &format!("{:?}", tool.bytes));
            }
            BlockKind::System { text, level } => {
                push_field(&mut out, "system");
                push_field(&mut out, text);
                push_field(&mut out, &format!("{level:?}"));
            }
        }
        out
    }
}

fn push_field(out: &mut String, value: &str) {
    let _ = write!(out, "{}:", value.len());
    out.push_str(value);
    out.push('|');
}

/// 渲染整个 transcript。
pub fn render_transcript(
    blocks: &[TranscriptBlock],
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(MIN_WIDTH as u16));
    let mut lines = Vec::new();
    for block in blocks.iter().filter(|block| block.state.is_visible()) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(render_block(block, width, theme));
    }
    lines
}

/// 渲染单个 block；输出可继续交给 `Paragraph` 或 scrollback commit。
pub fn render_block(block: &TranscriptBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(MIN_WIDTH);
    let mut lines = match &block.kind {
        BlockKind::User { text } => render_user(text, width, theme),
        BlockKind::Assistant { text } => render_assistant(text, width, theme),
        BlockKind::Thinking { text, duration } => render_thinking(
            text,
            *duration,
            block.state == BlockState::Live,
            width,
            theme,
        ),
        BlockKind::Tool(tool) => render_tool(tool, width, theme),
        BlockKind::System { text, level } => render_system(text, *level, width, theme),
    };
    if block.state == BlockState::Live
        && matches!(block.kind, BlockKind::Assistant { .. })
        && let Some(line) = lines.last_mut()
    {
        line.spans.push(Span::styled(
            theme.glyph.cursor.to_string(),
            fg(theme.accent.assistant),
        ));
    }
    lines
}

fn render_user(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let outer = " ".repeat(usize::from(theme.spacing.outer_pad));
    let first_prefix = format!("{outer}{} ", theme.glyph.user);
    let continuation = " ".repeat(first_prefix.width());
    render_prefixed_text(
        &text,
        width,
        &first_prefix,
        &continuation,
        fg(theme.accent.user),
        fg(theme.text.primary),
    )
}

fn render_assistant(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let prefix = format!("{} ", theme.glyph.assistant);
    let continuation = " ".repeat(prefix.width());
    let body_width = width.saturating_sub(prefix.width()).max(1);
    let body = super::markdown::render(&text, body_width, theme);
    prefix_lines(body, &prefix, &continuation, fg(theme.accent.assistant))
}

fn render_thinking(
    text: &str,
    duration: Option<Duration>,
    live: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let prefix = format!("{} ", theme.glyph.thinking);
    let continuation = " ".repeat(prefix.width());
    let mut lines = Vec::new();

    if live {
        lines.push(Line::from(vec![
            Span::styled(prefix.clone(), fg(theme.accent.thinking)),
            Span::styled("Thinking…", fg(theme.accent.thinking)),
        ]));
        if let Some(latest) = text.lines().rev().find(|line| !line.trim().is_empty()) {
            let body_width = width.saturating_sub(continuation.width() + 2).max(1);
            for seg in wrap_text(latest, body_width) {
                lines.push(Line::from(vec![
                    Span::styled(continuation.clone(), fg(theme.text.dim)),
                    Span::styled(format!("{} ", theme.glyph.quote), fg(theme.accent.thinking)),
                    Span::styled(seg, fg(theme.text.muted)),
                ]));
            }
        }
    } else {
        let label = duration.map_or_else(
            || "Thought".to_string(),
            |duration| format!("Thought for {}", format_duration(duration)),
        );
        lines.push(Line::from(vec![
            Span::styled(prefix, fg(theme.accent.thinking)),
            Span::styled(label, fg(theme.text.muted)),
        ]));
    }
    lines
}

fn render_tool(tool: &ToolBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let name = sanitize_text(&tool.name);
    let summary = tool
        .summary
        .as_deref()
        .map(sanitize_text)
        .filter(|summary| !summary.is_empty());

    // 头统一成 `⚙ {工具名} [{标记}] {正文}`：后端的 display summary 有两种风格
    // （`[OK] edit /p` 与 `edit /p`），这里先拆再拼，避免出现 "edit [OK] edit /p"
    // 这种工具名说两遍、成功说两遍的样子。
    let (marker, rest) = split_summary_marker(summary.as_deref().unwrap_or_default());
    let (rest, _had_name) = strip_tool_prefix(rest, &name);
    let marker_text = marker
        .map(|marker| format!(" [{marker}]"))
        .unwrap_or_default();
    // 缩短的预算要扣掉前面已经占掉的列（`⚙ name [OK]`），否则头部仍会折行。
    let budget = width
        .saturating_sub(theme.glyph.tool.width() + name.width() + marker_text.width() + 3)
        .max(24);
    let rest = shorten_header_paths(rest, budget);
    let header = if rest.is_empty() {
        format!("{} {name}{marker_text}", theme.glyph.tool)
    } else {
        format!("{} {name}{marker_text} {rest}", theme.glyph.tool)
    };
    let mut lines = render_prefixed_text(
        &header,
        width,
        "",
        "",
        fg(theme.accent.tool),
        fg(theme.accent.tool),
    );
    lines.extend(render_tool_state(tool, marker, theme));
    lines.extend(render_tool_body(tool, width, theme, summary.as_deref()));
    lines
}

/// 把后端 summary 拆成 `(终态标记, 正文)`：`[OK] edit /p` → `(Some("OK"), "edit /p")`。
///
/// 标记是后端 display 投影给的（`[OK]` / `[ERR]` / `[FAIL]`），拆出来是为了：
/// ① 头里能重新排成"名字在前、标记居中"；② 状态行据此判断"成功是不是已经说过了"。
fn split_summary_marker(summary: &str) -> (Option<&str>, &str) {
    let trimmed = summary.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((marker, tail)) = rest.split_once(']')
    {
        return (Some(marker), tail.trim_start());
    }
    (None, trimmed)
}

/// 去掉正文开头的工具名（`edit /p` → `/p`），返回 `(剩余, 是否真的去掉过)`。
fn strip_tool_prefix<'a>(text: &'a str, name: &str) -> (&'a str, bool) {
    if name.is_empty() {
        return (text, false);
    }
    match text.get(..name.len()) {
        Some(head)
            if head.eq_ignore_ascii_case(name)
                && text[name.len()..]
                    .chars()
                    .next()
                    .is_none_or(|ch| ch == ' ' || ch == ':' || ch == '/') =>
        {
            (text[name.len()..].trim_start(), true)
        }
        _ => (text, false),
    }
}

/// summary 的标记是否已经说明了成功。
fn marker_states_success(marker: Option<&str>) -> bool {
    marker.is_some_and(|marker| {
        let upper = marker.to_ascii_uppercase();
        upper == "OK" || upper == "DONE"
    })
}

/// 头部里的长路径缩短：home 前缀换 `~`，仍然过长的路径中段省略。
///
/// **只作用于头部摘要**，不动正文——正文是工具的真实输出，改了就不再是证据。
fn shorten_header_paths(text: &str, budget: usize) -> String {
    let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
    let mut out = String::new();
    for (idx, token) in text.split(' ').enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        let mut token = token.to_owned();
        if let Some(home) = home.as_deref()
            && let Some(rest) = token.strip_prefix(home)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            token = format!("~{rest}");
        }
        if token.width() > budget && token.contains('/') {
            token = elide_middle(&token, budget);
        }
        out.push_str(&token);
    }
    out
}

/// 中段省略：保留开头（能看出是哪棵树）与结尾（文件名/行号），中间换 `…`。
fn elide_middle(text: &str, max: usize) -> String {
    if text.width() <= max || max < 6 {
        return text.to_owned();
    }
    let head_budget = max / 3;
    let tail_budget = max.saturating_sub(head_budget).saturating_sub(1);
    let mut head = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if used + width > head_budget {
            break;
        }
        head.push(ch);
        used += width;
    }
    let mut tail_chars: Vec<char> = Vec::new();
    let mut tail_used = 0usize;
    for ch in text.chars().rev() {
        let width = ch.width().unwrap_or(0);
        if tail_used + width > tail_budget {
            break;
        }
        tail_chars.push(ch);
        tail_used += width;
    }
    tail_chars.reverse();
    format!("{head}…{}", tail_chars.into_iter().collect::<String>())
}

fn render_tool_state(tool: &ToolBlock, marker: Option<&str>, theme: &Theme) -> Vec<Line<'static>> {
    let (glyph, label, style) = match tool.state {
        ToolState::Prepared => (
            theme.glyph.running,
            "preparing".to_string(),
            fg(theme.accent.running),
        ),
        ToolState::Running => (
            theme.glyph.running,
            "running".to_string(),
            fg(theme.accent.running),
        ),
        ToolState::Success => (
            theme.glyph.success,
            "done".to_string(),
            fg(theme.accent.success),
        ),
        ToolState::Failed => (
            theme.glyph.failure,
            tool.failure
                .as_deref()
                .map(sanitize_text)
                .filter(|failure| !failure.is_empty())
                .unwrap_or_else(|| "failed".to_string()),
            fg(theme.accent.error),
        ),
        ToolState::Cancelled => (
            theme.glyph.failure,
            "cancelled".to_string(),
            fg(theme.semantic.warning),
        ),
        ToolState::Backgrounded => (
            theme.glyph.running,
            "backgrounded".to_string(),
            fg(theme.text.muted),
        ),
    };
    let mut metrics = Vec::new();
    if let Some(duration) = tool.duration {
        // 亚 100ms 打印成 `0.0s` 是纯噪声（实测每个快工具都带一行 0.0s）。
        if duration >= Duration::from_millis(100) {
            metrics.push(format_duration(duration));
        }
    }
    if let Some(bytes) = tool.bytes {
        metrics.push(format_bytes(bytes));
    }

    // 分工：**summary 说"做了什么"，状态行说"结果如何"**。summary 已经自带终态
    // 时（后端给的 `[OK]`），成功态就不再重复一遍 `✓ done`；失败/取消**保留**
    // 标签——那里的 label 是原因（`exit 1`），不是重复的结论。
    let label_is_redundant =
        matches!(tool.state, ToolState::Success) && marker_states_success(marker);
    let mut spans = vec![Span::styled("  ".to_string(), fg(theme.text.dim))];
    if !label_is_redundant {
        spans.push(Span::styled(format!("{glyph} "), style));
        spans.push(Span::styled(label, style));
    }
    if !metrics.is_empty() {
        let sep = if label_is_redundant { "" } else { " · " };
        spans.push(Span::styled(
            format!("{sep}{}", metrics.join(" · ")),
            fg(theme.text.dim),
        ));
    }
    // 结论与度量都没有 → 整行不画（空行也是噪声）。
    if label_is_redundant && metrics.is_empty() {
        return Vec::new();
    }
    vec![Line::from(spans)]
}

fn render_tool_body(
    tool: &ToolBlock,
    width: usize,
    theme: &Theme,
    summary: Option<&str>,
) -> Vec<Line<'static>> {
    let (text, diff) = if let Some(diff) = tool.diff.as_deref() {
        (diff, true)
    } else if let Some(output) = tool.output.as_deref() {
        (output, false)
    } else if let Some(progress) = tool.progress.as_deref() {
        (progress, false)
    } else {
        return Vec::new();
    };
    let text = sanitize_text(text);
    let mut source: Vec<&str> = text.lines().collect();
    if source.is_empty() {
        return Vec::new();
    }
    // 正文开头与 summary 逐字相同时不再重复：实测 `read` 的头部摘要与正文首行
    // 是同一句（`L1: timeout = 30`），上下各印一遍纯属噪声。
    let skip = summary
        .map(|summary| {
            source
                .iter()
                .take_while(|line| line.trim() == summary.trim())
                .count()
        })
        .unwrap_or(0);
    if skip > 0 {
        source.drain(..skip);
        if source.is_empty() {
            return Vec::new();
        }
    }

    let running = tool.state.is_running();
    let (selected, folded) = if running {
        let start = source.len().saturating_sub(TOOL_BODY_RUNNING_TAIL);
        (source[start..].to_vec(), 0)
    } else if source.len() <= TOOL_BODY_EDGE * 2 {
        (source.clone(), 0)
    } else {
        let mut selected: Vec<&str> = source.iter().take(TOOL_BODY_EDGE).copied().collect();
        let folded = source.len() - TOOL_BODY_EDGE * 2;
        selected.extend(source.iter().skip(source.len() - TOOL_BODY_EDGE).copied());
        (selected, folded)
    };

    let body_width = width.saturating_sub(4).max(1);
    let mut out = Vec::new();
    for (idx, line) in selected.into_iter().enumerate() {
        if folded > 0 && idx == TOOL_BODY_EDGE {
            out.push(Line::from(vec![
                Span::styled("    ".to_string(), fg(theme.text.dim)),
                Span::styled(
                    format!("{}折叠 {folded} 行{}", theme.glyph.fold, theme.glyph.fold),
                    fg(theme.text.dim),
                ),
            ]));
        }
        let style = tool_body_style(line, diff, theme);
        for seg in wrap_text(line, body_width) {
            out.push(Line::from(vec![
                Span::styled("    ".to_string(), fg(theme.text.dim)),
                Span::styled(seg, style),
            ]));
        }
    }
    out
}

fn tool_body_style(line: &str, diff: bool, theme: &Theme) -> Style {
    if diff {
        if line.starts_with("@@") {
            fg(theme.semantic.command)
        } else if line.starts_with('+') && !line.starts_with("+++") {
            fg(theme.diff.add_fg)
        } else if line.starts_with('-') && !line.starts_with("---") {
            fg(theme.diff.del_fg)
        } else {
            fg(theme.diff.equal_fg)
        }
    } else {
        fg(theme.text.secondary)
    }
}

fn render_system(
    text: &str,
    level: SystemLevel,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let (glyph, style) = match level {
        SystemLevel::Info => (theme.glyph.system, fg(theme.text.dim)),
        SystemLevel::Warning => (theme.glyph.failure, fg(theme.semantic.warning)),
        SystemLevel::Error => (theme.glyph.failure, fg(theme.accent.error)),
    };
    let prefix = format!("{glyph} ");
    let continuation = " ".repeat(prefix.width());
    render_prefixed_text(&text, width, &prefix, &continuation, style, style)
}

fn prefix_lines(
    lines: Vec<Line<'static>>,
    first_prefix: &str,
    continuation: &str,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let prefix = if idx == 0 { first_prefix } else { continuation };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(prefix.to_string(), prefix_style));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn render_prefixed_text(
    text: &str,
    width: usize,
    first_prefix: &str,
    continuation: &str,
    prefix_style: Style,
    text_style: Style,
) -> Vec<Line<'static>> {
    let prefix_width = first_prefix.width().max(continuation.width());
    let body_width = width.saturating_sub(prefix_width).max(1);
    let segments = wrap_text(text, body_width);
    segments
        .into_iter()
        .enumerate()
        .map(|(idx, seg)| {
            let prefix = if idx == 0 { first_prefix } else { continuation };
            let mut spans = vec![Span::styled(prefix.to_string(), prefix_style)];
            if !seg.is_empty() {
                spans.push(Span::styled(seg, text_style));
            }
            Line::from(spans)
        })
        .collect()
}

fn fg(color: ratatui::style::Color) -> Style {
    Style::new().fg(color)
}

fn sanitize_text(input: &str) -> String {
    strip_ansi_escapes(input)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f32();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        format!("{seconds:.0}s")
    } else {
        let minutes = (duration.as_secs() / 60).max(1);
        let seconds = duration.as_secs() % 60;
        format!("{minutes}m{seconds:02}s")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes:.0} B")
    } else if bytes < MIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{:.1} MB", bytes / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
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

    #[test]
    fn user_block_wraps_cjk_without_exceeding_width() {
        let block = TranscriptBlock::new(
            "u1",
            BlockKind::User {
                text: "解释一下 v2 的终端渲染设计，并确保中文宽字符不会被切裂。".to_string(),
            },
        );
        let lines = render_block(&block, 24, &theme());
        assert!(lines.len() > 1);
        assert!(text_of(&lines).contains("❯"));
        assert!(lines.iter().all(|line| line.width() <= 24));
    }

    #[test]
    fn assistant_markdown_uses_theme_tokens() {
        let block = TranscriptBlock::new(
            "a1",
            BlockKind::Assistant {
                text: "# 标题\n\n正文 `inline`".to_string(),
            },
        );
        let lines = render_block(&block, 40, &theme());
        let heading = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "标题")
            .expect("heading span");
        assert_eq!(heading.style.fg, Some(theme().markdown.h1));
        let code = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "inline")
            .expect("inline code span");
        assert_eq!(code.style.fg, Some(theme().markdown.code));
    }

    #[test]
    fn live_thinking_shows_latest_line() {
        let block = TranscriptBlock::new(
            "t1",
            BlockKind::Thinking {
                text: "first\nlatest thought".to_string(),
                duration: None,
            },
        );
        let text = text_of(&render_block(&block, 40, &theme()));
        assert!(text.contains("Thinking…"));
        assert!(text.contains("latest thought"));
        assert!(!text.contains("first"));
    }

    /// 工具卡的**去冗余**回归锁。四条都来自真机实拍：
    /// `⚙ edit [OK] edit /path`（工具名两次）、`✓ done`（成功两次说）、
    /// `read` 摘要与正文逐字重复、`0.0s`（亚 100ms 的无信息量度量）。
    #[test]
    fn tool_card_does_not_repeat_what_summary_already_says() {
        let theme = theme();

        // ① 后端 summary 自带工具名 + `[OK]`：头不重复名字，状态行不再说 done。
        let tool = ToolBlock {
            name: "edit".into(),
            summary: Some("[OK] edit /tmp/a.txt".into()),
            state: ToolState::Success,
            output: None,
            diff: None,
            progress: None,
            failure: None,
            duration: Some(Duration::from_millis(40)),
            bytes: Some(96),
        };
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(tool)),
            100,
            &theme,
        ));
        assert_eq!(
            text.matches("edit").count(),
            1,
            "工具名只能说一次：\n{text}"
        );
        assert!(
            !text.contains("done"),
            "summary 已有 [OK]，不该再说 done：\n{text}"
        );
        assert!(text.contains("96 B"), "度量要保留：\n{text}");
        assert!(!text.contains("0.0s"), "亚 100ms 不显示耗时：\n{text}");

        // ② summary 与正文逐字相同时不重复正文。
        let tool = ToolBlock {
            name: "read".into(),
            summary: Some("L1: timeout = 30".into()),
            state: ToolState::Success,
            output: Some("L1: timeout = 30".into()),
            diff: None,
            progress: None,
            failure: None,
            duration: Some(Duration::from_millis(500)),
            bytes: None,
        };
        let text = text_of(&render_block(
            &TranscriptBlock::new("t2", BlockKind::Tool(tool)),
            100,
            &theme,
        ));
        assert_eq!(
            text.matches("L1: timeout = 30").count(),
            1,
            "摘要与正文重复时应只留一处：\n{text}"
        );

        // ③ 失败态**保留**原因（`exit 1` 不是重复的结论，是信息）。
        let tool = ToolBlock {
            name: "bash".into(),
            summary: Some("cargo clippy".into()),
            state: ToolState::Failed,
            output: Some("error: unused import".into()),
            diff: None,
            progress: None,
            failure: Some("exit 1".into()),
            duration: Some(Duration::from_millis(900)),
            bytes: None,
        };
        let text = text_of(&render_block(
            &TranscriptBlock::new("t3", BlockKind::Tool(tool)),
            100,
            &theme,
        ));
        assert!(text.contains("exit 1"), "失败原因不能被去重吃掉：\n{text}");
    }

    /// 头部路径缩短：home 前缀换 `~`，过长中段省略；**正文不动**（正文是证据）。
    #[test]
    fn tool_header_shortens_paths_but_body_keeps_them() {
        let theme = theme();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/u".into());
        let long = format!("{home}/projects/very/deep/tree/with/many/segments/file.txt");
        let tool = ToolBlock {
            name: "write".into(),
            summary: Some(format!("[OK] {long}")),
            state: ToolState::Success,
            output: Some(long.clone()),
            diff: None,
            progress: None,
            failure: None,
            duration: None,
            bytes: None,
        };
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(tool)),
            60,
            &theme,
        ));
        let header = text.lines().next().unwrap_or_default();
        assert!(header.starts_with("⚙ "), "{text}");
        assert!(
            !header.contains(&format!("{home}/projects")),
            "头部应把 home 换成 ~：{header}"
        );
        assert!(header.contains('…'), "头部过长路径应中段省略：{header}");
        // 正文照旧完整（它是对齐/复制的依据，不能被缩写出错）。正文按宽度折行，
        // 所以分头尾两段判，而不是整串比对。
        assert!(
            text.contains(&format!("{home}/projects/very")),
            "正文不得被缩短（home 前缀应原样保留）：\n{text}"
        );
        assert!(
            text.contains("file.txt"),
            "正文不得被缩短（尾部应原样保留）：\n{text}"
        );
    }

    #[test]
    fn tool_failure_is_inline_and_fold_is_visible() {
        let tool = ToolBlock {
            name: "exec".to_string(),
            summary: Some("cargo test".to_string()),
            state: ToolState::Failed,
            output: Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight".to_string()),
            diff: None,
            progress: None,
            failure: Some("exit 1".to_string()),
            duration: Some(Duration::from_millis(1200)),
            bytes: Some(18 * 1024),
        };
        let block = TranscriptBlock::new("tool1", BlockKind::Tool(tool));
        let text = text_of(&render_block(&block, 60, &theme()));
        assert!(text.contains("exec cargo test"));
        assert!(text.contains("exit 1"));
        assert!(text.contains("折叠 2 行"));
    }

    #[test]
    fn system_error_uses_failure_glyph() {
        let block = TranscriptBlock::new(
            "s1",
            BlockKind::System {
                text: "connection lost".to_string(),
                level: SystemLevel::Error,
            },
        );
        let text = text_of(&render_block(&block, 40, &theme()));
        assert!(text.starts_with('✗'), "{text}");
    }

    #[test]
    fn render_sanitizes_terminal_control_sequences() {
        let block = TranscriptBlock::new(
            "u2",
            BlockKind::User {
                text: "\x1b[31mred\x1b[0m\x07\nok".to_string(),
            },
        );
        let rendered = text_of(&render_block(&block, 40, &theme()));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(rendered.contains("red"));
        assert!(rendered.contains("ok"));
    }

    #[test]
    fn discarded_blocks_are_not_rendered() {
        let mut block = TranscriptBlock::new(
            "x",
            BlockKind::System {
                text: "hidden".to_string(),
                level: SystemLevel::Info,
            },
        );
        block.discard();
        assert!(render_transcript(&[block], 40, &theme()).is_empty());
    }

    #[test]
    fn sealed_block_rejects_text_mutation() {
        let mut block = TranscriptBlock::new(
            "a2",
            BlockKind::Assistant {
                text: "one".to_string(),
            },
        );
        assert!(block.seal());
        assert!(!block.replace_text("two"));
        assert!(matches!(block.kind, BlockKind::Assistant { ref text } if text == "one"));
    }

    #[test]
    fn all_themes_render_without_color_leaks() {
        for kind in [
            ThemeKind::QaqhNight,
            ThemeKind::QaqhDay,
            ThemeKind::Terminal,
        ] {
            let theme = Theme::resolve(kind, ColorSupport::TrueColor);
            let blocks = vec![
                TranscriptBlock::new(
                    "u",
                    BlockKind::User {
                        text: "hello 世界".to_string(),
                    },
                ),
                TranscriptBlock::new(
                    "a",
                    BlockKind::Assistant {
                        text: "# title\nbody".to_string(),
                    },
                ),
            ];
            assert!(!render_transcript(&blocks, 40, &theme).is_empty());
        }
    }

    #[test]
    fn long_transcript_render_is_bounded() {
        let blocks: Vec<_> = (0..500)
            .map(|idx| {
                TranscriptBlock::new(
                    format!("b{idx}"),
                    BlockKind::Assistant {
                        text: "streaming text".to_string(),
                    },
                )
            })
            .collect();
        let rendered = render_transcript(&blocks, 80, &theme());
        assert!(rendered.len() >= 500);
    }

    #[test]
    fn transcript_snapshot_is_stable() {
        let blocks = vec![
            TranscriptBlock::new(
                "u",
                BlockKind::User {
                    text: "hello 世界".to_string(),
                },
            ),
            TranscriptBlock::new(
                "a",
                BlockKind::Assistant {
                    text: "# Title\nbody".to_string(),
                },
            )
            .with_state(BlockState::Sealed),
            TranscriptBlock::new(
                "t",
                BlockKind::Thinking {
                    text: String::new(),
                    duration: Some(Duration::from_millis(1200)),
                },
            )
            .with_state(BlockState::Sealed),
            TranscriptBlock::new(
                "tool",
                BlockKind::Tool(ToolBlock {
                    name: "read".to_string(),
                    summary: Some("plan.md".to_string()),
                    state: ToolState::Success,
                    output: Some("line one\nline two".to_string()),
                    diff: None,
                    progress: None,
                    failure: None,
                    duration: Some(Duration::from_millis(800)),
                    bytes: Some(1024),
                }),
            ),
            TranscriptBlock::new(
                "s",
                BlockKind::System {
                    text: "note".to_string(),
                    level: SystemLevel::Info,
                },
            ),
        ];
        assert_eq!(
            text_of(&render_transcript(&blocks, 40, &theme())),
            "  ❯ hello 世界\n\n◆ Title\n  \n  body\n\n◇ Thought for 1.2s\n\n\
             ⚙ read plan.md\n  ✓ done · 0.8s · 1.0 KB\n    line one\n    line two\n\n· note"
        );
    }
}
