//! V2 Transcript block 模型与主题化渲染。
//!
//! 直接输出 ratatui `Line`，颜色只从 [`Theme`] token 取。
//! adapter 负责把 `Turn/Block/ToolCard` 投影到 view model，避免在渲染层混入
//! 后端契约。

use std::fmt;
use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::wrap_text;
use crate::app::timeline_model::strip_ansi_escapes;
use crate::theme::Theme;

use super::display_tool_name;

const MIN_WIDTH: usize = 20;
const TOOL_BODY_EDGE: usize = 3;
const TOOL_BODY_RUNNING_TAIL: usize = 6;

/// 稳定块身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(String);

impl BlockId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
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

/// 块的渲染生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockState {
    Live,
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// 类型化工具头部（09-18 展示契约 §3.3 的 view 侧镜像）。
///
/// adapter 从 `ToolCard.display` 投影而来；`None` → 走 legacy summary 兜底。
/// 头部正文（命令/路径/查询）由后端声明为「真相字段」，渲染层不再从
/// 模型向 JSON 里考古。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolHeader {
    Shell {
        command: String,
    },
    Path {
        path: String,
    },
    Query {
        query: String,
        scope: Option<String>,
    },
    Other {
        label: String,
    },
}

/// stdout / stderr 分离的流正文（`ToolBody::Streams` 的 view 侧镜像）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolStreams {
    pub stdout: String,
    pub stderr: String,
}

/// ToolBlock 的 V2 view model。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// 用户显式展开后显示完整正文；默认仍保持终态卡的前后文折叠。
    pub expanded: bool,
    /// 类型化头部；有值时头部正文以它为准，summary 仅作 legacy 兜底。
    pub header: Option<ToolHeader>,
    /// 分离流正文；有值时优先于 `output`（exec 的 stdout/stderr 不再混排）。
    pub streams: Option<ToolStreams>,
    /// 终态退出码（display body/outcome）。`0` 不上屏（成功无需报数）。
    pub exit_code: Option<i32>,
    /// 后端明确告知正文被截断。
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
        expanded: bool,
    },
    Tool(Box<ToolBlock>),
    System {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBlock {
    pub id: BlockId,
    pub turn_id: String,
    pub revision: u64,
    pub state: BlockState,
    pub kind: BlockKind,
    /// 权威源 fact 墙钟（epoch ms，v2 信封 `ts_ms`）。`None` = 时间不可知。
    ///
    /// 只有用户回合头会渲染它；其余块保留该字段是为了让上层（消息菜单 / 导出）
    /// 拿到同一个权威值，而不是各自去猜。
    pub at_ms: Option<u64>,
}

impl TranscriptBlock {
    #[cfg(test)]
    pub fn new(id: impl Into<BlockId>, kind: BlockKind) -> Self {
        Self {
            id: id.into(),
            turn_id: String::new(),
            revision: 1,
            state: BlockState::Live,
            kind,
            at_ms: None,
        }
    }

    #[cfg(test)]
    pub fn with_state(mut self, state: BlockState) -> Self {
        self.state = state;
        self
    }

    #[cfg(test)]
    pub fn seal(&mut self) -> bool {
        if self.state != BlockState::Live {
            return false;
        }
        self.state = BlockState::Sealed;
        true
    }

    /// 替换纯文本块内容；工具块不走此入口。
    ///
    /// 封口后拒绝修改，防止 `Sealed` 内容在重放时发生静默变化。
    #[cfg(test)]
    pub fn replace_text(&mut self, value: impl Into<String>) -> bool {
        if self.state != BlockState::Live {
            return false;
        }
        let value = value.into();
        match &mut self.kind {
            BlockKind::User { text }
            | BlockKind::Assistant { text }
            | BlockKind::Thinking { text, .. }
            | BlockKind::System { text } => {
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
    for block in blocks {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(render_block(block, width, theme));
    }
    lines
}

/// 渲染单个 block。
pub fn render_block(block: &TranscriptBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(MIN_WIDTH);
    let mut lines = match &block.kind {
        BlockKind::User { text } => render_user(block.at_ms, text, width, theme),
        BlockKind::Assistant { text } => render_assistant(text, width, theme),
        BlockKind::Thinking {
            text,
            duration,
            expanded,
        } => render_thinking(
            text,
            *duration,
            block.state == BlockState::Live,
            *expanded,
            width,
            theme,
        ),
        BlockKind::Tool(tool) => render_tool(tool, block.at_ms, width, theme),
        BlockKind::System { text } => render_system(text, width, theme),
    };
    if block.state == BlockState::Live
        && matches!(block.kind, BlockKind::Assistant { .. })
    {
        use unicode_width::UnicodeWidthStr;
        let cursor = Span::styled(
            theme.glyph.cursor.to_string(),
            fg(theme.accent.assistant),
        );
        let cursor_width = theme.glyph.cursor.width();
        // 流式光标不能突破宽度预算：最后一行已满宽时（表格边框/代码围栏
        // 恰好收敛）另起一行，否则会被 Paragraph 截断成不可见。
        match lines.last_mut() {
            Some(line) if line.width() + cursor_width <= width => line.spans.push(cursor),
            Some(_) => lines.push(Line::from(vec![cursor])),
            None => {}
        }
    }
    lines
}

fn render_user(at_ms: Option<u64>, text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let outer = " ".repeat(usize::from(theme.spacing.outer_pad));
    // 时间戳直接来自权威源 fact 墙钟；缺席时**什么都不画**，不用本地时钟兜底
    // （那会让「服务端时间」与「猜的时间」长得一模一样）。
    let stamp = at_ms
        .and_then(format_wall_clock)
        .map(|stamp| format!("{stamp} "))
        .unwrap_or_default();
    let first_prefix = format!("{outer}{stamp}{} ", theme.glyph.user);
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

/// 权威墙钟（epoch ms）→ 本地 `MM-DD HH:MM`。
///
/// **纯函数**：只依赖入参、不含 `now()`，所以渲染缓存键与快照测试都稳定。
pub(crate) fn format_wall_clock(ts_ms: u64) -> Option<String> {
    let utc = chrono::DateTime::from_timestamp_millis(i64::try_from(ts_ms).ok()?)?;
    Some(
        utc.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
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
    expanded: bool,
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
        let hint = if text.is_empty() {
            ""
        } else if expanded {
            "（点击收起）"
        } else {
            "（点击展开）"
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, fg(theme.accent.thinking)),
            Span::styled(format!("{label}{hint}"), fg(theme.text.muted)),
        ]));
        if expanded {
            let body_width = width.saturating_sub(continuation.width() + 2).max(1);
            if text.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(continuation.clone(), fg(theme.text.dim)),
                    Span::styled("正文不可用", fg(theme.text.dim)),
                ]));
            } else {
                for line in text.lines() {
                    for seg in wrap_text(line, body_width) {
                        lines.push(Line::from(vec![
                            Span::styled(continuation.clone(), fg(theme.text.dim)),
                            Span::styled(
                                format!("{} ", theme.glyph.quote),
                                fg(theme.accent.thinking),
                            ),
                            Span::styled(seg, fg(theme.text.muted)),
                        ]));
                    }
                }
            }
        }
    }
    lines
}

fn render_tool(
    tool: &ToolBlock,
    at_ms: Option<u64>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // wire 名是模型向标识符（`exec`/`todo_write`），对人一律标题式（`Exec`）。
    let name = display_tool_name(&tool.name);
    let summary = tool
        .summary
        .as_deref()
        .map(sanitize_text)
        .filter(|summary| !summary.is_empty());

    // 头统一成 `⚙ {工具名} {正文}`。正文两级来源：
    // ① 类型化头部（display 契约）——命令/路径是后端声明的真相字段；
    // ② legacy summary——display 缺席的 H16 兜底（`[OK]` 标记拆解与工具名
    //    去重已随旧会话兼容臂一并删除，2026-10-03）。
    let rest = match tool.header.as_ref() {
        Some(header) => typed_header_rest(header),
        None => summary.as_deref().unwrap_or_default().to_owned(),
    };
    // 失败态的 legacy 摘要首行就是失败理由（`project_tool_summary` 取 output
    // 首行的投影），状态行已承载同一条理由——头里再放一遍即双重显示。
    // typed header 是命令/路径等「做了什么」的真相字段，不在此列。summary
    // 本身仍传给正文做逐字去重，信息不丢失。
    let rest = if tool.state == ToolState::Failed
        && tool.header.is_none()
        && tool.failure.is_some()
    {
        String::new()
    } else {
        rest
    };
    // 缩短的预算要扣掉前面已经占掉的列，否则头部仍会折行。
    let budget = width
        .saturating_sub(theme.glyph.tool.width() + name.width() + 3)
        .max(24);
    let rest = shorten_header_paths(&rest, budget);
    let header = if rest.is_empty() {
        format!("{} {name}", theme.glyph.tool)
    } else {
        format!("{} {name} {rest}", theme.glyph.tool)
    };
    let mut lines = render_prefixed_text(
        &header,
        width,
        "",
        "",
        fg(theme.accent.tool),
        fg(theme.accent.tool),
    );
    lines.extend(render_tool_state(tool, at_ms, theme));
    // 失败态的正文去重键置空：legacy summary（已从头部抑制）与失败理由常是
    // 同一行，逐字去重会把**证据**从正文里吃掉——状态行只放分类 code，正文
    // 必须完整承载理由。成功态的摘要去重照旧。
    let body_dedup_key = if tool.state == ToolState::Failed {
        None
    } else {
        summary.as_deref()
    };
    lines.extend(render_tool_body(tool, width, theme, body_dedup_key));
    lines
}

/// 类型化头部 → 头部正文。Shell 带 `$` 前缀点明「经 shell 执行」；
/// 命令/路径本身不再拆标记、去重名（它们不含工具名，也不含终态结论）。
fn typed_header_rest(header: &ToolHeader) -> String {
    match header {
        ToolHeader::Shell { command } => format!("$ {}", sanitize_text(command)),
        ToolHeader::Path { path } => sanitize_text(path),
        ToolHeader::Query { query, scope } => {
            let query = sanitize_text(query);
            match scope.as_deref().filter(|scope| !scope.is_empty()) {
                Some(scope) => format!("{query} · {}", sanitize_text(scope)),
                None => query,
            }
        }
        ToolHeader::Other { label } => sanitize_text(label),
    }
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

fn render_tool_state(tool: &ToolBlock, at_ms: Option<u64>, theme: &Theme) -> Vec<Line<'static>> {
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

    // 分工：**summary 说"做了什么"，状态行说"结果如何"**（状态行永远渲染，
    // 成功态 `✓ done` 也不再被 `[OK]` 标记抑制——标记机制已随旧会话兼容臂删除）。
    let mut spans = vec![Span::styled("  ".to_string(), fg(theme.text.dim))];
    spans.push(Span::styled(format!("{glyph} "), style));
    spans.push(Span::styled(label, style));
    if !metrics.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", metrics.join(" · ")),
            fg(theme.text.dim),
        ));
    }
    // 终态墙钟（runtime 盖戳，MM-DD HH:MM）：dim 尾缀；缺席不画——不用本地
    // 时钟兜底，与用户行同纪律。
    let stamp = at_ms
        .and_then(format_wall_clock)
        .map(|stamp| format!(" · {stamp}"))
        .unwrap_or_default();
    if !stamp.is_empty() {
        spans.push(Span::styled(stamp, fg(theme.text.dim)));
    }
    vec![Line::from(spans)]
}

fn render_tool_body(
    tool: &ToolBlock,
    width: usize,
    theme: &Theme,
    summary: Option<&str>,
) -> Vec<Line<'static>> {
    // 正文行统一成 `(文本, 是否 stderr)`：diff 优先（edit/write），其次分离流
    // （exec 的 stdout/stderr 不混排、stderr 用警示色另加段标签），最后 legacy 文本。
    let mut source: Vec<(String, bool)> = Vec::new();
    let mut is_diff = false;
    if let Some(diff) = tool.diff.as_deref().filter(|diff| !diff.is_empty()) {
        is_diff = true;
        source.extend(
            sanitize_text(diff)
                .lines()
                .map(|line| (line.to_owned(), false)),
        );
    } else if let Some(streams) = tool.streams.as_ref() {
        let stdout = sanitize_text(&streams.stdout);
        let stderr = sanitize_text(&streams.stderr);
        source.extend(stdout.lines().map(|line| (line.to_owned(), false)));
        source.extend(stderr.lines().map(|line| (line.to_owned(), true)));
    } else if let Some(text) = tool
        .output
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| tool.progress.as_deref().filter(|text| !text.is_empty()))
    {
        source.extend(
            sanitize_text(text)
                .lines()
                .map(|line| (line.to_owned(), false)),
        );
    }
    if source.is_empty() {
        return Vec::new();
    }
    // 正文开头与 summary 逐字相同时不再重复：实测 `read` 的头部摘要与正文首行
    // 是同一句（`L1: timeout = 30`），上下各印一遍纯属噪声。
    let skip = summary
        .map(|summary| {
            source
                .iter()
                .take_while(|(line, _)| line.trim() == summary.trim())
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
    let (selected, folded) = if tool.expanded {
        (source.clone(), 0)
    } else if running {
        let start = source.len().saturating_sub(TOOL_BODY_RUNNING_TAIL);
        (source[start..].to_vec(), 0)
    } else if source.len() <= TOOL_BODY_EDGE * 2 {
        (source.clone(), 0)
    } else {
        let mut selected: Vec<(String, bool)> =
            source.iter().take(TOOL_BODY_EDGE).cloned().collect();
        let folded = source.len() - TOOL_BODY_EDGE * 2;
        selected.extend(
            source
                .iter()
                .skip(source.len() - TOOL_BODY_EDGE)
                .cloned()
                .collect::<Vec<_>>(),
        );
        (selected, folded)
    };

    let body_width = width.saturating_sub(4).max(1);
    let mut out = Vec::new();
    let mut stderr_label_pending = selected.iter().any(|(_, is_stderr)| *is_stderr);
    for (idx, (line, is_stderr)) in selected.into_iter().enumerate() {
        if folded > 0 && idx == TOOL_BODY_EDGE {
            out.push(Line::from(vec![
                Span::styled("    ".to_string(), fg(theme.text.dim)),
                Span::styled(
                    format!(
                        "{}折叠 {folded} 行{}（点击展开）",
                        theme.glyph.fold, theme.glyph.fold
                    ),
                    fg(theme.text.dim),
                ),
            ]));
        }
        if is_stderr && stderr_label_pending {
            stderr_label_pending = false;
            out.push(Line::from(vec![
                Span::styled("    ".to_string(), fg(theme.text.dim)),
                Span::styled("⚠ stderr", fg(theme.semantic.warning)),
            ]));
        }
        let style = if is_stderr {
            fg(theme.semantic.warning)
        } else {
            tool_body_style(&line, is_diff, theme)
        };
        for seg in wrap_text(&line, body_width) {
            out.push(Line::from(vec![
                Span::styled("    ".to_string(), fg(theme.text.dim)),
                Span::styled(seg, style),
            ]));
        }
    }
    if tool.truncated {
        out.push(Line::from(vec![
            Span::styled("    ".to_string(), fg(theme.text.dim)),
            Span::styled("… 已截断", fg(theme.text.dim)),
        ]));
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

fn render_system(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text = sanitize_text(text);
    let glyph = theme.glyph.system;
    let style = fg(theme.text.dim);
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

    /// 工具块测试构造器：默认无类型化头部/流/截断（legacy summary 路径）。
    fn tool_block(name: &str, summary: Option<&str>, state: ToolState) -> ToolBlock {
        ToolBlock {
            name: name.to_string(),
            summary: summary.map(str::to_string),
            state,
            output: None,
            diff: None,
            progress: None,
            failure: None,
            duration: None,
            bytes: None,
            expanded: false,
            header: None,
            streams: None,
            exit_code: None,
            truncated: false,
        }
    }

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

    /// 全宽度扫描：1..=60 列逐列渲染各块型，断言不 panic 且不超出预算。
    ///
    /// `MIN_WIDTH=20` 钳制的契约：终端窄于 20 列时仍按 20 列渲染，横向超出
    /// 由 Paragraph 截断兜底（极窄下降级为横截而不是版面破碎/panic）；
    /// ≥20 列时必须完整收敛在给定宽度内。中文宽字符在每个宽度档都不能被切裂。
    #[test]
    fn render_block_sweeps_widths_without_overflow() {
        let theme = theme();
        let blocks = [
            TranscriptBlock::new(
                "u1",
                BlockKind::User {
                    text: "中文宽度扫描：解释终端渲染设计的边界条件与 CJK 宽字符处理。".into(),
                },
            ),
            TranscriptBlock::new(
                "a1",
                BlockKind::Assistant {
                    text: "# 标题\n\n正文段落，中英 mixed words、`inline code`。\n\n```rust\nfn main() { println!(\"你好世界\"); }\n```\n\n| 列一 | 列二 |\n| --- | --- |\n| 甲 | 乙 |\n"
                        .into(),
                },
            ),
            TranscriptBlock::new(
                "t1",
                BlockKind::Thinking {
                    text: "思考链第一行\n第二行更长的思考内容".into(),
                    duration: None,
                    expanded: true,
                },
            ),
            TranscriptBlock::new(
                "s1",
                BlockKind::System {
                    text: "系统提示：压缩完成".into(),
                },
            ),
            TranscriptBlock::new(
                "tool1",
                BlockKind::Tool(Box::new(tool_block(
                    "exec",
                    Some("cargo test --workspace"),
                    ToolState::Running,
                ))),
            ),
            TranscriptBlock::new(
                "tool2",
                BlockKind::Tool(Box::new({
                    let mut tool =
                        tool_block("exec", Some("cargo test --workspace"), ToolState::Success);
                    tool.output = Some("stdout 全文正文，很长的一段工具输出用来压测换行……".into());
                    tool
                })),
            ),
        ];
        for width in 1..=60 {
            for block in &blocks {
                let lines = render_block(block, width, &theme);
                let budget = width.max(MIN_WIDTH);
                for line in &lines {
                    assert!(
                        line.width() <= budget,
                        "width {width}（budget {budget}）溢出：{:?}",
                        text_of(std::slice::from_ref(line))
                    );
                }
            }
        }
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
                expanded: false,
            },
        );
        let text = text_of(&render_block(&block, 40, &theme()));
        assert!(text.contains("Thinking…"));
        assert!(text.contains("latest thought"));
        assert!(!text.contains("first"));
    }

    #[test]
    fn expanded_thinking_reveals_full_history() {
        let block = TranscriptBlock::new(
            "thinking-history",
            BlockKind::Thinking {
                text: "first line\nsecond line".to_string(),
                duration: Some(Duration::from_millis(1200)),
                expanded: true,
            },
        )
        .with_state(BlockState::Sealed);
        let text = text_of(&render_block(&block, 60, &theme()));
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("second line"), "{text}");
        assert!(text.contains("点击收起"), "{text}");
    }

    /// 工具卡的**去冗余**回归锁。`[OK]` 标记拆解已随旧会话兼容臂删除
    /// （2026-10-03），这里保留仍然成立的两条。
    #[test]
    fn tool_card_does_not_repeat_what_summary_already_says() {
        let theme = theme();

        // ① wire 名展示为大写形（Edit）；摘要与正文逐字相同时不重复正文。
        let mut tool = tool_block("read", Some("L1: timeout = 30"), ToolState::Success);
        tool.output = Some("L1: timeout = 30".into());
        tool.duration = Some(Duration::from_millis(500));
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            100,
            &theme,
        ));
        assert_eq!(
            text.matches("Read").count(),
            1,
            "工具名只能说一次：\n{text}"
        );
        assert!(!text.contains("read "), "wire 名不应以小写形上屏：\n{text}");
        assert_eq!(
            text.matches("L1: timeout = 30").count(),
            1,
            "摘要与正文重复时应只留一处：\n{text}"
        );

        // ② 失败态**保留**原因（`exit 1` 不是重复的结论，是信息）。
        let mut tool = tool_block("bash", Some("cargo clippy"), ToolState::Failed);
        tool.output = Some("error: unused import".into());
        tool.failure = Some("exit 1".into());
        tool.duration = Some(Duration::from_millis(900));
        let text = text_of(&render_block(
            &TranscriptBlock::new("t2", BlockKind::Tool(Box::new(tool))),
            100,
            &theme,
        ));
        assert!(text.contains("exit 1"), "失败原因不能被去重吃掉：\n{text}");
    }

    /// 契约切片：终态墙钟 dim 尾缀；缺席不画（不用本地时钟兜底）。
    #[test]
    fn tool_state_line_carries_authoritative_wall_clock_when_known() {
        let theme = theme();
        let ts: u64 = 1_759_488_000_000;
        let expected = format!(
            " · {}",
            format_wall_clock(ts).expect("固定 epoch 必须可格式化")
        );

        let mut tool = tool_block("exec", None, ToolState::Success);
        let with_ts = TranscriptBlock {
            at_ms: Some(ts),
            ..TranscriptBlock::new("w1", BlockKind::Tool(Box::new(tool.clone())))
        };
        let text = text_of(&render_block(&with_ts, 90, &theme));
        assert!(text.contains(&expected), "状态行带墙钟尾缀：
{text}");

        tool.duration = Some(Duration::from_millis(40)); // 亚 100ms 不显示耗时
        let no_ts = TranscriptBlock::new("w2", BlockKind::Tool(Box::new(tool)));
        let text = text_of(&render_block(&no_ts, 90, &theme));
        assert!(
            !text.contains(" · "),
            "墙钟缺席时状态行不画时间：
{text}"
        );
    }

    /// 失败态**单显示**回归锁（2026-10-03 双重显示事故）：legacy 摘要首行 =
    /// 失败理由，状态行已承载同一条理由——头里再放一遍就是重复。新数据下
    /// 错误文本只出现在状态行，正文只留证据（Hint/后续行）。
    #[test]
    fn failed_tool_shows_the_reason_exactly_once() {
        let theme = theme();
        let reason = "file changed since read";
        let mut tool = tool_block("edit", Some(reason), ToolState::Failed);
        tool.output = Some(format!("{reason}
Hint: re-read the file"));
        // 适配器最终产出：正文有证据 → 状态行只放分类 code。
        tool.failure = Some("stale_file".into());
        let text = text_of(&render_block(
            &TranscriptBlock::new("f1", BlockKind::Tool(Box::new(tool))),
            100,
            &theme,
        ));
        let header = text.lines().next().unwrap_or_default();
        assert_eq!(
            header,
            format!("{} Edit", theme.glyph.tool),
            "失败态头部不再重复理由，只留工具名：
{text}"
        );
        assert_eq!(
            text.matches(reason).count(),
            1,
            "错误理由只能出现一次（状态行）：
{text}"
        );
        assert!(text.contains("Hint: re-read the file"), "正文证据保留：
{text}");
    }

    /// 旧 journal 回放（failure.message 曾是整段 output）：标签已由 adapter
    /// 压成单行，渲染层靠「正文逐字去重 + 头部抑制」保证首行不再三处出现。
    #[test]
    fn replayed_failure_reason_appears_once_not_thrice() {
        let theme = theme();
        let reason = "error: could not compile";
        let mut tool = tool_block("bash", Some(reason), ToolState::Failed);
        tool.output = Some(format!("{reason}
more context follows"));
        // 旧 journal 回放 + 适配器产出：正文有证据 → 裸 code。
        tool.failure = Some("tool_execution_failed".into());
        let text = text_of(&render_block(
            &TranscriptBlock::new("f2", BlockKind::Tool(Box::new(tool))),
            100,
            &theme,
        ));
        assert_eq!(
            text.matches(reason).count(),
            1,
            "旧数据回放也不得三重显示：
{text}"
        );
        assert!(text.contains("more context follows"), "证据后续行保留：
{text}");
    }

    /// typed header 是「做了什么」的真相字段：失败态照常保留命令，错误只在
    /// 正文（stderr）出现一次。
    #[test]
    fn typed_header_failure_keeps_command_and_single_error_display() {
        let theme = theme();
        let mut tool = tool_block("exec", None, ToolState::Failed);
        tool.header = Some(ToolHeader::Shell {
            command: "cargo build".into(),
        });
        tool.streams = Some(ToolStreams {
            stdout: String::new(),
            stderr: "error: could not compile".into(),
        });
        tool.exit_code = Some(101);
        tool.failure = Some("exit 101".into());
        let text = text_of(&render_block(
            &TranscriptBlock::new("f3", BlockKind::Tool(Box::new(tool))),
            90,
            &theme,
        ));
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("cargo build"),
            "typed 头部保留命令真相字段：{header}"
        );
        assert_eq!(text.matches("could not compile").count(), 1, "{text}");
    }

    /// 头部路径缩短：home 前缀换 `~`，过长中段省略；**正文不动**（正文是证据）。
    #[test]
    fn tool_header_shortens_paths_but_body_keeps_them() {
        let theme = theme();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/u".into());
        let long = format!("{home}/projects/very/deep/tree/with/many/segments/file.txt");
        // 现行 wire：write 走 typed Path 头；summary 为 None → 正文无逐字去重。
        let mut tool = tool_block("write", None, ToolState::Success);
        tool.header = Some(ToolHeader::Path { path: long.clone() });
        tool.output = Some(long.clone());
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
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
        // 折行位置随终端宽度与 HOME 长度变化，所以剥掉空白后再比对。
        let unwrapped: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            unwrapped.contains(&format!("{home}/projects/very")),
            "正文不得被缩短（home 前缀应原样保留）：\n{text}"
        );
        assert!(
            unwrapped.contains("file.txt"),
            "正文不得被缩短（尾部应原样保留）：\n{text}"
        );
    }

    #[test]
    fn expanded_tool_body_reveals_the_folded_middle() {
        let mut tool = tool_block("exec", Some("cargo test"), ToolState::Success);
        tool.output = Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight".into());
        tool.expanded = true;
        let text = text_of(&render_block(
            &TranscriptBlock::new("tool-expanded", BlockKind::Tool(Box::new(tool))),
            60,
            &theme(),
        ));
        assert!(text.contains("four"), "展开后必须看到中段：\n{text}");
        assert!(!text.contains("折叠 2 行"), "展开态不应再画折叠提示");
    }

    #[test]
    fn tool_failure_is_inline_and_fold_is_visible() {
        let mut tool = tool_block("exec", Some("cargo test"), ToolState::Failed);
        tool.output = Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight".into());
        tool.failure = Some("exit 1".into());
        tool.duration = Some(Duration::from_millis(1200));
        tool.bytes = Some(18 * 1024);
        let block = TranscriptBlock::new("tool1", BlockKind::Tool(Box::new(tool)));
        let text = text_of(&render_block(&block, 60, &theme()));
        // 失败态头部不再带 legacy 摘要（它与状态行理由重复，见
        // failed_tool_shows_the_reason_exactly_once）；标签与折叠提示照旧。
        let header = text.lines().next().unwrap_or_default();
        assert_eq!(header, format!("{} Exec", theme().glyph.tool), "{text}");
        assert!(text.contains("exit 1"));
        assert!(text.contains("折叠 2 行"));
    }

    /// display 契约的 Shell 头部：头部只有命令真相字段（`$` 前缀），名字大写，
    /// summary（`exit 0 · …`）不再和头部/状态行重复。
    #[test]
    fn shell_header_shows_command_and_capitalized_name() {
        let mut tool = tool_block("exec", None, ToolState::Success);
        tool.header = Some(ToolHeader::Shell {
            command: "cargo check --tests".into(),
        });
        tool.output = Some("warning: unused import".into());
        tool.duration = Some(Duration::from_millis(800));
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            90,
            &theme(),
        ));
        let header = text.lines().next().unwrap_or_default();
        assert_eq!(header, "⚙ Exec $ cargo check --tests", "{text}");
        assert!(text.contains("done"), "{text}");
        assert!(text.contains("0.8s"), "{text}");
        assert!(
            !text.contains("exit 0"),
            "成功退出码是噪声，不该上屏：\n{text}"
        );
        assert!(!text.contains("exec"), "wire 名不上屏：\n{text}");
    }

    /// 分离流正文：stdout 原样，stderr 有警示段标签，不再把模型向 JSON 当正文。
    #[test]
    fn streams_body_labels_stderr() {
        let mut tool = tool_block("exec", None, ToolState::Failed);
        tool.streams = Some(ToolStreams {
            stdout: "compiling qaqh-tui".into(),
            stderr: "error: could not compile".into(),
        });
        tool.failure = Some("exit 101".into());
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            80,
            &theme(),
        ));
        assert!(text.contains("compiling qaqh-tui"), "{text}");
        assert!(text.contains("⚠ stderr"), "{text}");
        assert!(text.contains("error: could not compile"), "{text}");
        assert!(text.contains("exit 101"), "{text}");
        assert!(
            !text.contains("{\"status\""),
            "模型向 JSON 不得上屏：\n{text}"
        );
    }

    #[test]
    fn truncated_body_announces_itself() {
        let mut tool = tool_block("read", Some("plan.md"), ToolState::Success);
        tool.output = Some("L1\nL2\nL3\nL4\nL5\nL6\nL7".into());
        tool.truncated = true;
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            60,
            &theme(),
        ));
        assert!(text.contains("已截断"), "{text}");
    }

    #[test]
    fn tool_names_display_in_title_case() {
        assert_eq!(display_tool_name("exec"), "Exec");
        assert_eq!(display_tool_name("read"), "Read");
        assert_eq!(display_tool_name("write"), "Write");
        assert_eq!(display_tool_name("todo_write"), "Todo Write");
        assert_eq!(display_tool_name(""), "");
        assert_eq!(display_tool_name("_a"), "A");
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
                    expanded: false,
                },
            )
            .with_state(BlockState::Sealed),
            TranscriptBlock::new(
                "tool",
                BlockKind::Tool(Box::new({
                    let mut tool = tool_block("read", Some("plan.md"), ToolState::Success);
                    tool.output = Some("line one\nline two".into());
                    tool.duration = Some(Duration::from_millis(800));
                    tool.bytes = Some(1024);
                    tool
                })),
            ),
            TranscriptBlock::new(
                "s",
                BlockKind::System {
                    text: "note".to_string(),
                },
            ),
        ];
        assert_eq!(
            text_of(&render_transcript(&blocks, 40, &theme())),
            "  ❯ hello 世界\n\n◆ Title\n  \n  body\n\n◇ Thought for 1.2s\n\n\
             ⚙ Read plan.md\n  ✓ done · 0.8s · 1.0 KB\n    line one\n    line two\n\n· note"
        );
    }
}
