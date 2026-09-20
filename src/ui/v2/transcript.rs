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
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

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
    pub revision: u64,
    pub state: BlockState,
    pub kind: BlockKind,
}

impl TranscriptBlock {
    pub fn new(id: impl Into<BlockId>, kind: BlockKind) -> Self {
        Self {
            id: id.into(),
            revision: 1,
            state: BlockState::Live,
            kind,
        }
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
    let body = render_markdown(&text, body_width, theme);
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
    let summary = tool.summary.as_deref().map(sanitize_text);
    let mut header = format!("{} {name}", theme.glyph.tool);
    if let Some(summary) = summary.as_deref().filter(|summary| !summary.is_empty()) {
        header.push(' ');
        header.push_str(summary);
    }
    let mut lines = render_prefixed_text(
        &header,
        width,
        "",
        "",
        fg(theme.accent.tool),
        fg(theme.accent.tool),
    );
    lines.extend(render_tool_state(tool, theme));
    lines.extend(render_tool_body(tool, width, theme));
    lines
}

fn render_tool_state(tool: &ToolBlock, theme: &Theme) -> Vec<Line<'static>> {
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
        metrics.push(format_duration(duration));
    }
    if let Some(bytes) = tool.bytes {
        metrics.push(format_bytes(bytes));
    }
    let mut spans = vec![
        Span::styled("  ".to_string(), fg(theme.text.dim)),
        Span::styled(format!("{glyph} "), style),
        Span::styled(label, style),
    ];
    if !metrics.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", metrics.join(" · ")),
            fg(theme.text.dim),
        ));
    }
    vec![Line::from(spans)]
}

fn render_tool_body(tool: &ToolBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
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
    let source: Vec<&str> = text.lines().collect();
    if source.is_empty() {
        return Vec::new();
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

fn render_markdown(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            let code_style = fg(theme.markdown.code);
            for seg in wrap_text(raw, width.saturating_sub(2).max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("  ".to_string(), Style::default()),
                    Span::styled(seg, code_style),
                ]));
            }
            continue;
        }
        if let Some((level, rest)) = heading(raw) {
            let style = heading_style(level, theme);
            for seg in wrap_text(rest, width) {
                lines.push(Line::from(Span::styled(seg, style)));
            }
            continue;
        }
        if is_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(40)),
                fg(theme.markdown.rule),
            )));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.trim_start();
            for seg in wrap_text(rest, width.saturating_sub(2).max(1)) {
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", theme.glyph.quote), fg(theme.markdown.quote)),
                    Span::styled(seg, fg(theme.markdown.quote)),
                ]));
            }
            continue;
        }
        if let Some((prefix, rest)) = list_item(trimmed) {
            let (prefix, rest, style) = task_item(prefix, rest, theme);
            let body_width = width.saturating_sub(prefix.width()).max(1);
            for (idx, seg) in wrap_text(rest, body_width).into_iter().enumerate() {
                let shown_prefix = if idx == 0 {
                    prefix.clone()
                } else {
                    " ".repeat(prefix.width())
                };
                let mut spans = vec![Span::styled(shown_prefix, style)];
                spans.extend(inline_spans(&seg, style, theme));
                lines.push(Line::from(spans));
            }
            continue;
        }
        for seg in wrap_text(raw, width) {
            lines.push(Line::from(inline_spans(
                &seg,
                fg(theme.markdown.text),
                theme,
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
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

fn heading(raw: &str) -> Option<(usize, &str)> {
    let trimmed = raw.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) || trimmed.chars().nth(level) != Some(' ') {
        return None;
    }
    trimmed.get(level + 1..).map(|rest| (level, rest))
}

fn heading_style(level: usize, theme: &Theme) -> Style {
    let color = match level {
        1 => theme.markdown.h1,
        2 => theme.markdown.h2,
        3 => theme.markdown.h3,
        4 => theme.markdown.h4,
        5 => theme.markdown.h5,
        _ => theme.markdown.h6,
    };
    fg(color).add_modifier(Modifier::BOLD)
}

fn is_rule(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 3
        && (value.chars().all(|ch| ch == '-')
            || value.chars().all(|ch| ch == '*')
            || value.chars().all(|ch| ch == '_'))
}

fn list_item(value: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = value.strip_prefix(marker) {
            return Some(("• ".to_string(), rest));
        }
    }
    let digits = value.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && value.chars().nth(digits) == Some('.')
        && value.chars().nth(digits + 1) == Some(' ')
    {
        return value
            .get(digits + 2..)
            .map(|rest| (value[..=digits].to_string(), rest));
    }
    None
}

fn task_item<'a>(prefix: String, rest: &'a str, theme: &Theme) -> (String, &'a str, Style) {
    if let Some(value) = rest.strip_prefix("[x] ") {
        return (
            format!("{} ", theme.glyph.success),
            value,
            fg(theme.markdown.task_done),
        );
    }
    if let Some(value) = rest.strip_prefix("[ ] ") {
        return ("○ ".to_string(), value, fg(theme.markdown.task_todo));
    }
    (prefix, rest, fg(theme.markdown.text))
}

fn inline_spans(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base));
        }
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            spans.push(Span::styled(rest[start..].to_string(), base));
            return spans;
        };
        spans.push(Span::styled(
            after[..end].to_string(),
            fg(theme.markdown.code),
        ));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
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
            "  ❯ hello 世界\n\n◆ Title\n  body\n\n◇ Thought for 1.2s\n\n\
             ⚙ read plan.md\n  ✓ done · 0.8s · 1.0 KB\n    line one\n    line two\n\n· note"
        );
    }
}
