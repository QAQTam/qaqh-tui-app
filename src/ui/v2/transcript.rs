//! V2 Transcript block 模型与主题化渲染。
//!
//! 直接输出 ratatui `Line`，颜色只从 [`Theme`] token 取。
//! adapter 负责把 `Turn/Block/ToolCard` 投影到 view model，避免在渲染层混入
//! 后端契约。

use std::fmt;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::render_line::wrap_text;
use crate::app::timeline_model::{LineStats, strip_ansi_escapes};
use crate::theme::Theme;

use super::display_tool_name;

/// Path 头部的操作语义（动词事实源）。直接吃权威 wire 类型，本仓不手抄镜像
/// ——纪律同 protocol/mod.rs。
pub use qaqh_client::TimelinePathOp as PathOp;
const MIN_WIDTH: usize = 20;

/// 工具正文的预览行数（终态）。与三家一致取 3——再多就不是「扫一眼」了。
const TOOL_BODY_PREVIEW: usize = 3;

/// 正文首行与续行的缩进。`└` 把输出明确挂到上面那次调用下面；续行对齐到
/// 同一列，于是整张卡只有**两档**左边缘（头 0 列 / 正文 2 列）。
const TOOL_BODY_FIRST: &str = "  └ ";
const TOOL_BODY_CONT: &str = "    ";

/// 时长短于这个阈值不上屏：每个快工具都挂一个 `0.3s` 是纯噪声。
const TOOL_DURATION_MIN: Duration = Duration::from_secs(1);

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
///
/// Path 的 `op` 是动词的**事实源**（§3.3 wire 契约）：edit→Edited、
/// write→Wrote、patch→Patched、delete→Deleted。没有它，头部只剩
/// 「改过某个文件」一种说法——改一行、整文件覆盖和删除读起来一样。
///
/// op 用权威 wire 类型 `qaqh_client::TimelinePathOp`（Copy），本仓不手抄镜像
/// ——纪律同 protocol/mod.rs。它没实现 `Hash`，所以这里的 `Hash` derive 全部
/// 撤除：这些 view 类型从未被哈希过（全仓零使用点），derive 是历史遗留的死
/// 面——而且 wire 类型缺哪个 trait 都不该倒逼本仓改后端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolHeader {
    Shell {
        command: String,
    },
    Path {
        path: String,
        op: PathOp,
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

/// `todo_write` 的线名。专用渲染靠它分流，不走通用工具卡。
pub(super) const TODO_TOOL_NAME: &str = "todo_write";

/// todo 工具家族谓词（对齐后端 `qaqh-runtime::dashboard::is_todo_tool`）。
///
/// 渲染侧曾只认 `todo_write` 精确名（B1）：模型工作时的状态推进全部走
/// `todo_update`，在名字门就被丢弃，置顶面板冻结在最后一次整表写。
/// 所有「这是不是 todo 工具」的判定一律走本函数，禁止再各自手写。
pub fn is_todo_tool(name: &str) -> bool {
    matches!(name, "todo_write" | "todo_update" | "todo_list")
}

/// 折叠态最多画几行清单（超出的交给「点击展开」）。
const TODO_PREVIEW_ROWS: usize = 3;

/// 一条待办的状态。真实契约只有这三态，服务端会拒绝其它取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// `todo_write` 的单条待办。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TodoItem {
    /// 服务端分配的稳定 id；模型新建条目时会省略。
    pub id: Option<String>,
    pub title: String,
    pub status: TodoStatus,
    /// 完成证据（单个字符串，不是数组）。
    pub evidence: Option<String>,
}

/// `todo_write` 的清单视图模型。
///
/// 契约来自实测会话日志（`~/.qaqh/sessions/*/messages.jsonl`）：
/// 入参是 `{"items":[{"id"?,"title","status","description"?,"evidence"?}]}`。
/// 字段名是 **`items`** —— 既不是 Claude Code 的 `todos`，也不是 Codex 的
/// `plan`，三个参考实现的解析器一个都不能直接抄。
///
/// 服务端语义：整表替换（回参带 `replaced`/`total`/`assigned`）；`title` 每次
/// 必填，漏填整次调用被拒；`id` 不能凭空引用，省略即新建。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TodoBlock {
    pub items: Vec<TodoItem>,
}

impl TodoBlock {
    /// 解析 `todo_write` 的 `args_json`。
    ///
    /// **任一条目畸形就整体返回 `None`**（调用方回退普通工具卡），不做部分渲染：
    /// 半个计划比没有计划更误导人。`title` 缺失正是模型最常犯的错——服务端会整次
    /// 拒绝（`invalid arguments: missing field 'title'`）。
    pub fn parse(args_json: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
        let items = value.get("items")?.as_array()?;
        if items.is_empty() {
            return None;
        }
        let items = items.iter().map(todo_item).collect::<Option<Vec<_>>>()?;
        Some(Self { items })
    }

    /// 解析 `todo_update` 的入参 delta（单一形态 `{id, status, evidence?}`，
    /// 一次一条——后端 `split.rs::handle_update` 的 `reject_fields` 明确拒绝
    /// `items`/批量字段）。
    ///
    /// 返回 `(目标 id, 新状态, 新证据)`；evidence 缺省 = 不动原值（与后端
    /// `exec_todo_set` 语义一致）。id 缺失或 status 非法时返回 `None`，调用方
    /// 忽略这条 delta（面板维持上一次已知状态，不整体作废）。
    pub fn parse_update(args_json: &str) -> Option<(String, TodoStatus, Option<Option<String>>)> {
        let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
        let id = value.get("id").and_then(serde_json::Value::as_str)?.trim();
        if id.is_empty() {
            return None;
        }
        let status = match value.get("status").and_then(serde_json::Value::as_str)? {
            "pending" => TodoStatus::Pending,
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            _ => return None,
        };
        let evidence = match value.get("evidence") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => v
                .as_str()
                .map(|text| Some(text.trim().to_owned()))
                .or(Some(None)),
        };
        Some((id.to_owned(), status, evidence))
    }

    /// 把一条 `todo_update` delta 按 id 折叠进清单（就地修改）。
    ///
    /// id 找不到就忽略——delta 可能先于 write 到达（乱序回放）或指向已删除
    /// 条目，宁可少改也不错改。
    pub fn apply_update(&mut self, id: &str, status: TodoStatus, evidence: Option<Option<String>>) {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id.as_deref() == Some(id))
        else {
            return;
        };
        item.status = status;
        if let Some(evidence) = evidence {
            item.evidence = evidence;
        }
    }

    /// `(待办, 进行中, 已完成)`。
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for item in &self.items {
            match item.status {
                TodoStatus::Pending => counts.0 += 1,
                TodoStatus::InProgress => counts.1 += 1,
                TodoStatus::Completed => counts.2 += 1,
            }
        }
        counts
    }

    /// 渲染顺序：进行中 → 待办 → 已完成（沿用 Codex plan cell 的优先级）。
    ///
    /// 稳定排序，所以同状态内保持模型给的原始顺序。
    fn ordered(&self) -> Vec<&TodoItem> {
        let rank = |status: TodoStatus| match status {
            TodoStatus::InProgress => 0u8,
            TodoStatus::Pending => 1,
            TodoStatus::Completed => 2,
        };
        let mut items: Vec<&TodoItem> = self.items.iter().collect();
        items.sort_by_key(|item| rank(item.status));
        items
    }
}

fn todo_item(value: &serde_json::Value) -> Option<TodoItem> {
    let id = todo_id(value.get("id"));
    // `title` 省略是**合法入参**：带既有 id 时后端沿用该条目的原标题（2026-10-04
    // 起）。卡片只有本次入参、看不到上一版清单，所以这里存空串、渲染时退回显示
    // id；常驻面板走 `adapter::current_todo`，那边会沿历史把真标题补回来。
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if title.is_empty() && id.is_none() {
        // 既没标题又没 id：这条指代不了任何东西，按畸形整体回退。
        return None;
    }
    let status = match value.get("status")?.as_str()? {
        "pending" => TodoStatus::Pending,
        "in_progress" => TodoStatus::InProgress,
        "completed" => TodoStatus::Completed,
        _ => return None,
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|field| field.as_str())
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(str::to_owned)
    };
    Some(TodoItem {
        id,
        title,
        status,
        evidence: text("evidence"),
    })
}

/// `id` 归一化：后端接受 `"T1"` / `"1"` / `1` 三种写法，这里逐字对齐它的
/// 归一化规则（`parse_todo_id`）。
///
/// 面板要靠 id 去上一版清单里继承标题，两边必须落到同一个键上——各自保留
/// 原样会让 `1` 和 `T1` 变成两个身份，继承随之落空。
fn todo_id(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            match raw.strip_prefix('T') {
                Some(rest) if rest.parse::<u32>().is_ok() => Some(raw.to_owned()),
                _ => raw.parse::<u32>().ok().map(|number| format!("T{number}")),
            }
        }
        serde_json::Value::Number(number) => number.as_u64().map(|number| format!("T{number}")),
        _ => None,
    }
}

/// ToolBlock 的 V2 view model。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    pub name: String,
    pub summary: Option<String>,
    /// `todo_write` 的清单；其余工具恒为 `None`。有值时走专用渲染。
    pub todo: Option<TodoBlock>,
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
    /// 行差数字（渲染唯一出口）：终态权威值，或运行中的流式估算。
    pub line_stats: Option<LineStats>,
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
    let mut index = 0;
    while index < blocks.len() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let group = lookup_group_len(&blocks[index..]);
        if group >= 2 {
            lines.extend(render_lookup_group(
                &blocks[index..index + group],
                width,
                theme,
            ));
            index += group;
        } else {
            lines.extend(render_block(&blocks[index], width, theme));
            index += 1;
        }
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
        BlockKind::Tool(tool) => render_tool(tool, width, theme),
        BlockKind::System { text } => render_system(text, width, theme),
    };
    if block.state == BlockState::Live && matches!(block.kind, BlockKind::Assistant { .. }) {
        use unicode_width::UnicodeWidthStr;
        let cursor = Span::styled(theme.glyph.cursor.to_string(), fg(theme.accent.assistant));
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

fn render_tool(tool: &ToolBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    // todo 家族有专属形状：没有「命令/路径」正文，服务端输出是
    // `{replaced,total,assigned,current_id,...}` 记账 JSON——那串东西对人没有
    // 信息量，照通用工具卡透出就是"屏幕上一条 JSON"。update 的入参 delta
    // 同样不该走通用卡（B1 家族谓词）。
    if is_todo_tool(&tool.name) {
        return render_todo_tool(tool, width, theme);
    }
    let name = display_tool_name(&tool.name);
    let summary = tool_summary(tool);

    // 头统一成 `{标记} {动词} {正文}`。正文两级来源：
    // ① 类型化头部（display 契约）——命令/路径是后端声明的真相字段；
    // ② legacy summary——display 缺席的兜底。
    let rest = tool_head_rest(tool, summary.as_deref());
    // 缩短的预算要扣掉前面已经占掉的列，否则头部仍会被裁掉尾部。
    let budget = width
        .saturating_sub(theme.glyph.tool.width() + name.width() + 3)
        .max(24);
    let rest = shorten_header_paths(&rest, budget);
    // 终态失败时头部只剩 `Failed (exit 101)`：不写清是谁失败就没法定位，
    // 用工具名兜底。成功/运行中不必——动词本身已经说明是哪个家族。
    let rest = if rest.is_empty()
        && matches!(
            tool.state,
            ToolState::Failed | ToolState::Cancelled | ToolState::Backgrounded
        ) {
        name
    } else {
        rest
    };

    let mut lines = vec![tool_header(tool, &rest, width, theme)];
    // 失败态的正文去重键置空：legacy summary（已从头部抑制）与失败理由常是
    // 同一行，逐字去重会把**证据**从正文里吃掉——头部只放分类 code，正文
    // 必须完整承载理由。成功态的摘要去重照旧。
    let body_dedup_key = if tool.state == ToolState::Failed {
        None
    } else {
        summary.as_deref()
    };
    lines.extend(render_tool_body(tool, width, theme, body_dedup_key));
    lines
}

/// 清洗过的 legacy 摘要（display 契约缺席时的兜底）。
fn tool_summary(tool: &ToolBlock) -> Option<String> {
    tool.summary
        .as_deref()
        .map(sanitize_text)
        .filter(|summary| !summary.is_empty())
}

/// 头部正文（动词后面那段）：typed header 优先，legacy summary 兜底。
fn tool_head_rest(tool: &ToolBlock, summary: Option<&str>) -> String {
    let rest = match tool.header.as_ref() {
        Some(header) => typed_header_rest(header),
        None => summary.unwrap_or_default().to_owned(),
    };
    // 失败态的 legacy 摘要首行就是失败理由（`project_tool_summary` 取 output
    // 首行的投影），正文已承载同一条理由——头里再放一遍即双重显示。
    // typed header 是命令/路径等「做了什么」的真相字段，不在此列。
    if tool.state == ToolState::Failed && tool.header.is_none() && tool.failure.is_some() {
        String::new()
    } else {
        rest
    }
}

fn tool_of(block: &TranscriptBlock) -> Option<&ToolBlock> {
    match &block.kind {
        BlockKind::Tool(tool) => Some(tool),
        _ => None,
    }
}

/// 查询族：只读、无副作用，正文是「看过哪些地方」而不是「发生了什么」。
fn is_lookup_tool(name: &str) -> bool {
    matches!(
        name,
        "read" | "read_file" | "grep" | "search" | "glob" | "list"
    )
}

/// 连续查询族的合并长度；`<2` 表示不合并。
///
/// 只合并**同回合、同工具、全部成功、都未展开、都没被截断**的连续块：这正是
/// 「一口气读了 N 个文件」的形状。混进失败/运行中/展开/截断就不再合并——那几种
/// 状态各自的卡片形状本身就是有用的信息，不该被一张汇总卡盖掉。
pub fn lookup_group_len(blocks: &[TranscriptBlock]) -> usize {
    let Some(first) = blocks.first() else {
        return 0;
    };
    let Some(head) = tool_of(first) else {
        return 0;
    };
    if !is_lookup_tool(&head.name) || !groupable(head) {
        return 0;
    }
    let mut len = 1;
    for block in &blocks[1..] {
        let Some(tool) = tool_of(block) else { break };
        if block.turn_id != first.turn_id || tool.name != head.name || !groupable(tool) {
            break;
        }
        len += 1;
    }
    if len < 2 { 0 } else { len }
}

fn groupable(tool: &ToolBlock) -> bool {
    tool.state == ToolState::Success && !tool.expanded && !tool.truncated
}

/// 连续查询族合并成一张卡。
///
/// 模型读 6 个文件会画出 6 张几乎一样的卡（6 行头 + 最多 18 行正文 + 5 个空行），
/// 而它们回答的是同一个问题——「看了哪些地方」。合并后是一行结论加一份清单，
/// 读起来是「读了 6 个文件」而不是 6 次噪音。
///
/// 展开即**拆组**（`lookup_group_len` 遇到展开态就不再合并）：展开的意图本来
/// 就是「给我看每一次调用」。
///
/// 清单用 `text.secondary` 而不是正文那档 `text.muted`：这里列出的是这张卡的
/// **主体内容**（看了哪些地方），不是需要退到背景里的输出证据。
pub fn render_lookup_group(
    blocks: &[TranscriptBlock],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(head) = blocks.first().and_then(tool_of) else {
        return Vec::new();
    };
    let (marker, marker_style) = tool_marker(ToolState::Success, theme);
    let verb = tool_verb(head, &head.name, ToolState::Success)
        .map(str::to_owned)
        .unwrap_or_else(|| display_tool_name(&head.name));
    let noun = match head.name.as_str() {
        "read" | "read_file" => "files",
        "grep" | "search" => "queries",
        "glob" | "list" => "patterns",
        _ => "calls",
    };
    let mut lines = vec![clip_line(
        vec![
            Span::styled(format!("{marker} "), marker_style),
            Span::styled(verb, fg(theme.text.primary).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} {noun}", blocks.len()),
                fg(theme.text.secondary),
            ),
            Span::styled("（点击展开）", fg(theme.text.dim)),
        ],
        width,
    )];
    let body_width = width.saturating_sub(TOOL_BODY_CONT.width()).max(1);
    for block in blocks {
        let Some(tool) = tool_of(block) else { continue };
        let summary = tool_summary(tool);
        let target = tool_head_rest(tool, summary.as_deref());
        let target = if target.is_empty() {
            display_tool_name(&tool.name)
        } else {
            shorten_header_paths(&target, body_width)
        };
        for (row, seg) in wrap_text(&target, body_width).into_iter().enumerate() {
            let prefix = if lines.len() == 1 && row == 0 {
                TOOL_BODY_FIRST
            } else {
                TOOL_BODY_CONT
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), fg(theme.text.dim)),
                Span::styled(seg, fg(theme.text.secondary)),
            ]));
        }
    }
    lines
}

/// 工具卡头部：`{标记} {动词} {正文} {徽标} · {时长}`。
///
/// 状态（成功/失败/运行中）由**标记颜色 + 动词**承载，不再另起一行——参考三家
/// （Codex `• Ran …`、Claude `⏺ Bash(…)`、bugent `⏺ Bash …`）都是这个形状，
/// 而"另起一行写 ✓ done"在成功态是零信息量的一行。
fn tool_header(tool: &ToolBlock, rest: &str, width: usize, theme: &Theme) -> Line<'static> {
    let (marker, marker_style) = tool_marker(tool.state, theme);
    let mut spans = vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(
            tool_head_word(tool),
            fg(theme.text.primary).add_modifier(Modifier::BOLD),
        ),
    ];
    if !rest.is_empty() {
        spans.push(Span::styled(format!(" {rest}"), fg(theme.text.secondary)));
    }
    // 行差是**文件改动的徽标**，跟在路径后面（Codex `• Edited path (+2 -1)`），
    // 不再和时长/字节混成一条 `·` 分隔的指标带。
    if let Some(stats) = tool.line_stats {
        spans.push(Span::styled(" (", fg(theme.text.dim)));
        spans.push(Span::styled(
            format!("+{}", stats.add),
            fg(theme.diff.add_fg),
        ));
        spans.push(Span::styled(" ", fg(theme.text.dim)));
        spans.push(Span::styled(
            format!("−{}", stats.del),
            fg(theme.diff.del_fg),
        ));
        spans.push(Span::styled(")", fg(theme.text.dim)));
        if stats.estimating {
            spans.push(Span::styled(" 估算", fg(theme.text.dim)));
        }
    }
    if let Some(duration) = tool.duration.filter(|d| *d >= TOOL_DURATION_MIN) {
        spans.push(Span::styled(
            format!(" · {}", format_duration(duration)),
            fg(theme.text.dim),
        ));
    }
    clip_line(spans, width)
}

/// 状态标记与配色（状态的唯一出口）。
fn tool_marker(state: ToolState, theme: &Theme) -> (&'static str, Style) {
    match state {
        ToolState::Prepared | ToolState::Running => (theme.glyph.running, fg(theme.accent.running)),
        ToolState::Success => (theme.glyph.success, fg(theme.accent.success)),
        ToolState::Failed => (theme.glyph.failure, fg(theme.accent.error)),
        ToolState::Cancelled => (theme.glyph.failure, fg(theme.semantic.warning)),
        ToolState::Backgrounded => (theme.glyph.running, fg(theme.text.muted)),
    }
}

/// 头部的动词。
///
/// 头一行要读成一句人话（`Ran cargo test`），不是协议名（`Exec cargo test`
/// ——命令本身已经说明了它在执行）。只覆盖能一眼看懂的家族，其余回退到标题化
/// 的工具名（至少不是 `todo_write` 这种 wire 标识符）。
///
/// 语义源是**类型化事实**而不是工具名折并：
/// - exec/read/grep/web/todo 各家族共享一条 wire 名臂——这些名字在不同
///   harness 间同义（Codex `Ran`、Claude `Bash`、Grok `Run Command` 都不区分
///   同族别名），折并是安全的；
/// - 文件改动族**只认 Path 头的 `op`**（edit/write/apply_patch/copy_range/
///   delete 在 wire 上各发各的 op，语义差异恰好是用户关心的：改一行 vs 整
///   文件覆盖 vs 多文件补丁 vs 删除），名字臂不再折并它们；
/// - 运行中的文件卡给**中性现在时**（`Edit`，非 `Editing`）：此刻没有任何
///   编辑证据可显示（diff 在执行完才存在），过去式的进行时版是伪形态；
///   exec 族不同——它有流式输出 tail 作证据，`Running` 站得住。
fn tool_verb(tool: &ToolBlock, name: &str, state: ToolState) -> Option<&'static str> {
    let running = state.is_running();
    // 文件改动族：op 在手则 op 是唯一动词源；无 op（legacy/Other 头）回退
    // 名字臂与 exec 同等待遇——比编一个家族词诚实。
    if let Some(ToolHeader::Path { op, .. }) = tool.header.as_ref() {
        return path_op_verb(*op, running);
    }
    Some(match (name, running) {
        ("exec" | "shell" | "bash" | "run", true) => "Running",
        ("exec" | "shell" | "bash" | "run", false) => "Ran",
        ("read" | "read_file", true) => "Reading",
        ("read" | "read_file", false) => "Read",
        ("edit" | "write" | "apply_patch" | "delete", true) => "Editing",
        ("edit" | "write" | "apply_patch" | "delete", false) => "Edited",
        ("grep" | "search", true) => "Searching",
        ("grep" | "search", false) => "Searched",
        ("glob" | "list", true) => "Listing",
        ("glob" | "list", false) => "Listed",
        ("web_fetch" | "web_search", true) => "Fetching",
        ("web_fetch" | "web_search", false) => "Fetched",
        ("todo_write", true) => "Updating Plan",
        ("todo_write", false) => "Updated Plan",
        ("todo_update", true) => "Updating Plan",
        ("todo_update", false) => "Updated Plan",
        ("todo_list", true) => "Reading Plan",
        ("todo_list", false) => "Read Plan",
        _ => return None,
    })
}

/// Path op → 动词（运行中 = 中性现在时，终态 = 过去式）。
fn path_op_verb(op: PathOp, running: bool) -> Option<&'static str> {
    let (past, present) = match op {
        PathOp::Edit => ("Edited", "Edit"),
        PathOp::Write => ("Wrote", "Write"),
        PathOp::Patch => ("Patched", "Patch"),
        PathOp::Delete => ("Deleted", "Delete"),
        PathOp::Read | PathOp::List => ("Read", "Reading"),
        PathOp::Unknown => return None,
    };
    Some(if running { present } else { past })
}

/// 头部动词：终态失败/取消用状态词（理由进括号），其余走 [`tool_verb`]。
fn tool_head_word(tool: &ToolBlock) -> String {
    match tool.state {
        ToolState::Failed => match (tool.exit_code, tool.failure.as_deref()) {
            (Some(code), _) => format!("Failed (exit {code})"),
            (None, Some(code)) if !code.is_empty() => format!("Failed ({code})"),
            _ => "Failed".to_string(),
        },
        ToolState::Cancelled => "Cancelled".to_string(),
        ToolState::Backgrounded => "Backgrounded".to_string(),
        _ => tool_verb(tool, &tool.name, tool.state)
            .map(str::to_owned)
            .unwrap_or_else(|| display_tool_name(&tool.name)),
    }
}

/// 头部**裁剪**而不是折行。
///
/// 折成两行、第二行还不对齐的头部比截断更难看；而且命令的后半段（重定向、
/// 管道）在窄屏上本来就该靠展开看，不该撑破版面。
fn clip_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let budget = width - used;
        let text = span.content.as_ref();
        if text.width() <= budget {
            used += text.width();
            out.push(span);
            continue;
        }
        let clipped = truncate_width(text, budget);
        out.push(Span::styled(clipped, span.style));
        break;
    }
    Line::from(out)
}

/// `todo_write` 的专用渲染。
///
/// - **成功**：画清单（进行中 → 待办 → 已完成），绝不画 `model.text` 里那串
///   `{"replaced":0,"total":5,"assigned":[...],"status":"ok"}` 记账 JSON；
/// - **失败/取消**：错误正文是证据，照常显示。实测 4 次调用挂了 2 次
///   （`items[0] references unknown id T1`、`invalid arguments: missing field
///   'title'`），把失败理由吞掉会让用户以为计划更新成功了；
/// - **运行中**：进度照常显示（参数可能还没到齐）。
fn render_todo_tool(tool: &ToolBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let failed = matches!(tool.state, ToolState::Failed | ToolState::Cancelled);
    let (marker, marker_style) = tool_marker(tool.state, theme);
    let mut spans = vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(
            tool_head_word(tool),
            fg(theme.text.primary).add_modifier(Modifier::BOLD),
        ),
    ];
    if failed {
        // 失败态头部只剩 `Failed (…)`：补上工具名，否则不知道是哪次调用挂了。
        spans.push(Span::styled(
            format!(" {}", display_tool_name(&tool.name)),
            fg(theme.text.secondary),
        ));
    } else if let Some(todo) = tool.todo.as_ref().filter(|todo| !todo.items.is_empty()) {
        // 头部只报进度：进行中的那项由 `[>]` 标记 + 置顶位置表达，不再把
        // `N 项 · N 进行中 · N 待办` 全堆上去。失败态不报——清单根本没画出来，
        // 报一个进度数字只会误导。
        let (_, _, completed) = todo.counts();
        spans.push(Span::styled(
            format!(" · {completed}/{} 完成", todo.items.len()),
            fg(theme.text.dim),
        ));
    }
    let mut lines = vec![clip_line(spans, width)];
    if failed || tool.state.is_running() {
        lines.extend(render_tool_body(tool, width, theme, None));
    } else if let Some(todo) = tool.todo.as_ref() {
        lines.extend(render_todo_items(todo, width, theme, tool.expanded));
    } else {
        // 成功但参数解析不出来（契约漂移）：退回通用正文。宁可露出原始返回，
        // 也不静默吞掉一次计划更新。
        lines.extend(render_tool_body(tool, width, theme, None));
    }
    lines
}

/// 清单正文。
///
/// 标记一律用**纯 ASCII 且等宽三格**（`[ ] [>] [x] `）：`☐ ◐ ☑` 属于 East
/// Asian Ambiguous，宽度随终端 locale 变（bugent 实测用户的终端把 `☐` 显示成了
/// 连字符），而 `✳`/`▶` 这类还会被彩色 emoji 字体抢走——本仓刚在这上面栽过。
fn render_todo_items(
    todo: &TodoBlock,
    width: usize,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    const MARKER_WIDTH: usize = 4;
    let ordered = todo.ordered();
    let hidden = ordered.len().saturating_sub(TODO_PREVIEW_ROWS);
    let shown: &[&TodoItem] = if expanded || hidden == 0 {
        &ordered
    } else {
        &ordered[..TODO_PREVIEW_ROWS]
    };
    let body_width = width
        .saturating_sub(TOOL_BODY_CONT.width() + MARKER_WIDTH)
        .max(1);
    let mut out = Vec::new();
    for (index, item) in shown.iter().enumerate() {
        let (marker, style) = todo_marker(item.status, theme);
        let text = todo_item_text(item);
        for (row, seg) in wrap_text(&text, body_width).into_iter().enumerate() {
            // 首行挂 `└` 明示这是头部行下面的子列表；续行对齐到同一列。
            let prefix = if index == 0 && row == 0 {
                TOOL_BODY_FIRST
            } else {
                TOOL_BODY_CONT
            };
            let marker = if row == 0 {
                marker.to_string()
            } else {
                " ".repeat(MARKER_WIDTH)
            };
            out.push(Line::from(vec![
                Span::styled(prefix.to_string(), fg(theme.text.dim)),
                Span::styled(marker, style),
                Span::styled(seg, style),
            ]));
        }
    }
    if hidden > 0 && !expanded {
        out.push(Line::from(vec![
            Span::styled(TOOL_BODY_CONT.to_string(), fg(theme.text.dim)),
            Span::styled(fold_hint(hidden), fg(theme.text.dim)),
        ]));
    }
    out
}

/// 折叠提示：短到一眼扫过（`… +12 行（点击展开）`）。
fn fold_hint(hidden: usize) -> String {
    format!("… +{hidden} 行（点击展开）")
}

fn todo_marker(status: TodoStatus, theme: &Theme) -> (&'static str, Style) {
    match status {
        TodoStatus::Pending => ("[ ] ", fg(theme.text.secondary)),
        TodoStatus::InProgress => (
            "[>] ",
            fg(theme.accent.running).add_modifier(Modifier::BOLD),
        ),
        TodoStatus::Completed => (
            "[x] ",
            fg(theme.text.dim).add_modifier(Modifier::CROSSED_OUT),
        ),
    }
}

fn todo_item_text(item: &TodoItem) -> String {
    let title = if item.title.is_empty() {
        // 入参省略 title（后端语义：沿用同 id 的旧标题）。卡片只有本次入参，
        // 拿不到旧标题，退回显示 id —— 常驻面板会补上真标题。
        item.id.clone().unwrap_or_else(|| "未命名任务".to_string())
    } else {
        item.title.clone()
    };
    match (item.status, item.evidence.as_deref()) {
        (TodoStatus::Completed, Some(evidence)) => format!("{title} — {evidence}"),
        _ => title,
    }
}

/// sticky 面板里清单行的缩进与标记宽度（与卡片内清单同一套 ASCII 标记）。
const TODO_PANEL_INDENT: &str = "  ";
const TODO_MARKER_WIDTH: usize = 4;

/// sticky 待办面板（贴在 composer 输入带上方的那块常驻区）。
///
/// 与 [`render_todo_tool`] 的分工：卡片回答「这次更新改了什么」（带状态、耗时、
/// 失败正文），面板回答「现在整体是什么状态」——所以面板画**全量**清单、不带
/// 时间戳，且进行中的那项被 [`TodoBlock::ordered`] 排在第一行：面板看不到
/// 「现在在干什么」就白占了屏幕。
///
/// 定高摘要：行数由 `max_rows` 封顶，放不下只报数不折行（完整清单在 F4 的
/// Workspace Todo 面板）。**全部完成时返回空**——计划已经落地，常驻区该让位
/// 给转录区了；那条「全部打勾」的记录仍然留在 transcript 的卡片里。
pub fn compose_todo_panel(
    todo: &TodoBlock,
    width: usize,
    max_rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if todo.items.is_empty() || max_rows < 2 {
        return Vec::new();
    }
    let (_, _, completed) = todo.counts();
    if completed == todo.items.len() {
        return Vec::new();
    }
    let ordered = todo.ordered();
    // 头部只报进度：进行中的那项由 `[>]` 标记 + 置顶位置表达，再写一遍是冗余，
    // 而且窄屏下「· 进行中」会先把头部撑爆。
    let header = format!("待办 · {completed}/{} 完成", todo.items.len());
    let mut out = vec![Line::from(Span::styled(
        truncate_width(&header, width),
        fg(theme.text.primary).add_modifier(Modifier::BOLD),
    ))];

    // 标题占一行；真要截断时，溢出提示再占一行。
    let room = max_rows - 1;
    let (shown, hidden) = if ordered.len() <= room {
        (ordered.as_slice(), 0)
    } else {
        let keep = room.saturating_sub(1);
        (&ordered[..keep], ordered.len() - keep)
    };
    let body_width = width
        .saturating_sub(TODO_PANEL_INDENT.width() + TODO_MARKER_WIDTH)
        .max(1);
    for item in shown {
        let (marker, style) = todo_marker(item.status, theme);
        out.push(Line::from(vec![
            Span::styled(TODO_PANEL_INDENT.to_string(), fg(theme.text.dim)),
            Span::styled(marker.to_string(), style),
            Span::styled(truncate_width(&todo_item_text(item), body_width), style),
        ]));
    }
    if hidden > 0 {
        out.push(Line::from(vec![
            Span::styled(TODO_PANEL_INDENT.to_string(), fg(theme.text.dim)),
            Span::styled(
                format!("{} 还有 {hidden} 项", theme.glyph.fold),
                fg(theme.text.dim),
            ),
        ]));
    }
    out
}

/// 按显示宽度截断并补省略号（面板是定高摘要，不折行；CJK 按两格算）。
fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if used + width + 1 > max {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

/// 类型化头部 → 头部正文。
///
/// 命令/路径本身不拆标记、不去重名（它们不含工具名，也不含终态结论）。命令不再
/// 带 `$` 前缀：动词已经是 `Ran`，「执行」这件事不需要第三个符号再说一遍。
fn typed_header_rest(header: &ToolHeader) -> String {
    match header {
        ToolHeader::Shell { command } => sanitize_text(command),
        ToolHeader::Path { path, .. } => sanitize_text(path),
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

/// 工具正文：`  └ ` 起头、整体降一档亮度。
///
/// 预览只取**一边** 3 行，不再头尾各取 3：头尾同时出现时，读者分不清哪几行是
/// 连续的。取哪一边按内容的读法定——流式输出是时序的，**结论在最后**（取尾）；
/// diff 是按文件顺序排的，**改动从前往后读**（取头）。运行中只取尾部且不画
/// 折叠提示（那行会跟着输出一直跳）。
fn render_tool_body(
    tool: &ToolBlock,
    width: usize,
    theme: &Theme,
    summary: Option<&str>,
) -> Vec<Line<'static>> {
    // 正文行统一成 `(文本, 是否 stderr)`：diff 优先（edit/write），其次分离流
    // （exec 的 stdout/stderr 不混排），最后 legacy 文本。
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
    let (selected, hidden, hint_first) = if tool.expanded || source.len() <= TOOL_BODY_PREVIEW {
        (source.clone(), 0, false)
    } else if running {
        let start = source.len() - TOOL_BODY_PREVIEW;
        (source[start..].to_vec(), 0, false)
    } else if is_diff {
        (
            source[..TOOL_BODY_PREVIEW].to_vec(),
            source.len() - TOOL_BODY_PREVIEW,
            false,
        )
    } else {
        let start = source.len() - TOOL_BODY_PREVIEW;
        (
            source[start..].to_vec(),
            source.len() - TOOL_BODY_PREVIEW,
            true,
        )
    };

    let body_width = width.saturating_sub(TOOL_BODY_CONT.width()).max(1);
    let mut out = Vec::new();
    if hidden > 0 && hint_first {
        out.push(Line::from(vec![
            Span::styled(TOOL_BODY_FIRST.to_string(), fg(theme.text.dim)),
            Span::styled(fold_hint(hidden), fg(theme.text.dim)),
        ]));
    }
    for (line, is_stderr) in selected {
        // stderr 不再单独占一行打标签：颜色本身就是区分（参考三家都只换色）。
        let style = if is_stderr {
            fg(theme.semantic.warning)
        } else {
            tool_body_style(&line, is_diff, theme)
        };
        for seg in wrap_text(&line, body_width) {
            out.push(Line::from(vec![
                Span::styled(body_prefix(&out), fg(theme.text.dim)),
                Span::styled(seg, style),
            ]));
        }
    }
    if hidden > 0 && !hint_first {
        out.push(Line::from(vec![
            Span::styled(TOOL_BODY_CONT.to_string(), fg(theme.text.dim)),
            Span::styled(fold_hint(hidden), fg(theme.text.dim)),
        ]));
    }
    if tool.truncated {
        out.push(Line::from(vec![
            Span::styled(TOOL_BODY_CONT.to_string(), fg(theme.text.dim)),
            Span::styled("… 已截断", fg(theme.text.dim)),
        ]));
    }
    out
}

/// 正文前缀：整块的第一行挂 `└`，其余对齐到同一列。
fn body_prefix(out: &[Line<'static>]) -> &'static str {
    if out.is_empty() {
        TOOL_BODY_FIRST
    } else {
        TOOL_BODY_CONT
    }
}

fn tool_body_style(line: &str, diff: bool, theme: &Theme) -> Style {
    if diff {
        if line.starts_with("@@") {
            // 段头是**元数据**不是内容：用 gutter 色（比正文更暗）让它退到背景里，
            // 而不是拿 `semantic.command`（命令黄）喊出来。
            fg(theme.diff.gutter_fg)
        } else if line.starts_with('+') && !line.starts_with("+++") {
            fg(theme.diff.add_fg)
        } else if line.starts_with('-') && !line.starts_with("---") {
            fg(theme.diff.del_fg)
        } else {
            fg(theme.diff.equal_fg)
        }
    } else {
        fg(theme.text.muted)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    /// 工具块测试构造器：默认无类型化头部/流/截断（legacy summary 路径）。
    fn tool_block(name: &str, summary: Option<&str>, state: ToolState) -> ToolBlock {
        ToolBlock {
            name: name.to_string(),
            summary: summary.map(str::to_string),
            todo: None,
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
            line_stats: None,
        }
    }

    /// 实测入参：取自 `~/.qaqh/sessions/01a1055a-.../messages.jsonl`。
    /// 字段名是 `items`，`description` 只在变化时出现。
    /// `title` 自 2026-10-04 起可选：带既有 `id` 时省略 = 后端沿用原标题。
    const REAL_TODO_ARGS: &str = r#"{"items":[
        {"description":"用 codegraph + 源码定位 runtime 内最重模块及其职责边界","status":"in_progress","title":"重区地图：qaqh-runtime 模块职责与体量分布"},
        {"description":"registry.rs / timeline.rs / engine_turn.rs / ringing/hub.rs 的解耦候选","status":"pending","title":"耦合热点：god file 的扇入扇出与可拆点"},
        {"description":"fallback、降级、双路径、legacy 兼容、文本再解析等","status":"pending","title":"防御性/兼容设计清单（带证据）"}
    ]}"#;

    /// 服务端成功返回（`tool.output` / `model.text`）就是这串记账 JSON——
    /// 也就是现在会被直接铺到屏幕上的那串东西。
    const REAL_TODO_OUTPUT: &str = r#"{"replaced":0,"total":3,"assigned":["T1","T2","T3"],"current_id":"T1","message":"Plan updated: 3 item(s) (3 new).","timeis":"UTC+8 2026-10-04 13:24","status":"ok"}"#;

    fn todo_tool(args: &str, state: ToolState) -> ToolBlock {
        let mut tool = tool_block("todo_write", None, state);
        tool.todo = TodoBlock::parse(args);
        tool
    }

    fn todo_text(tool: &ToolBlock) -> String {
        text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool.clone()))),
            60,
            &theme(),
        ))
    }

    #[test]
    fn todo_block_parses_real_wire_payload() {
        let todo = TodoBlock::parse(REAL_TODO_ARGS).expect("parse");
        assert_eq!(todo.items.len(), 3);
        assert_eq!(todo.items[0].status, TodoStatus::InProgress);
        assert_eq!(
            todo.items[0].title,
            "重区地图：qaqh-runtime 模块职责与体量分布"
        );
        assert_eq!(todo.counts(), (2, 1, 0));

        // 契约是 `items`——不是 Claude Code 的 `todos`，也不是 Codex 的 `plan`。
        assert!(TodoBlock::parse(r#"{"todos":[{"title":"a","status":"pending"}]}"#).is_none());
        assert!(TodoBlock::parse(r#"{"plan":[{"step":"a","status":"pending"}]}"#).is_none());
        // 既没 title 也没 id：这条指代不了任何东西，按畸形整体回退。
        assert!(TodoBlock::parse(r#"{"items":[{"status":"pending"}]}"#).is_none());
        // 未知状态不接受（服务端只认三态）。
        assert!(TodoBlock::parse(r#"{"items":[{"title":"a","status":"open"}]}"#).is_none());
        // 空清单与畸形 JSON 都不接管渲染。
        assert!(TodoBlock::parse(r#"{"items":[]}"#).is_none());
        assert!(TodoBlock::parse("not json").is_none());
    }

    /// 带既有 id、省略 title 的条目是**合法入参**（后端沿用原标题）。
    ///
    /// 卡片只有本次入参、看不到上一版清单，所以退回显示 id；id 的三种写法
    /// （`"T1"` / `"1"` / `1`）必须归一化成同一个键，否则面板继承标题会落空。
    #[test]
    fn todo_block_accepts_title_less_items_and_normalizes_ids() {
        let todo =
            TodoBlock::parse(r#"{"items":[{"id":"T1","status":"completed"}]}"#).expect("parse");
        assert_eq!(todo.items[0].title, "");
        assert_eq!(todo.items[0].id.as_deref(), Some("T1"));
        assert!(
            todo_text(&todo_tool(
                r#"{"items":[{"id":"T1","status":"completed"}]}"#,
                ToolState::Success
            ))
            .contains("[x] T1")
        );

        for raw in [
            r#"{"items":[{"id":"T1","status":"pending"}]}"#,
            r#"{"items":[{"id":"1","status":"pending"}]}"#,
            r#"{"items":[{"id":1,"status":"pending"}]}"#,
        ] {
            let todo = TodoBlock::parse(raw).expect(raw);
            assert_eq!(todo.items[0].id.as_deref(), Some("T1"), "{raw}");
        }
        // 非法 id 不能当成「没有 id」蒙混过关：它和空标题一起构成畸形条目。
        assert!(TodoBlock::parse(r#"{"items":[{"id":"nope","status":"pending"}]}"#).is_none());
    }

    /// 成功态：画清单，**不画那串记账 JSON**；头部只报进度，不再堆
    /// `N 项 · N 进行中 · N 待办`，也不另起一行 `✓ done`。
    #[test]
    fn todo_write_success_renders_checklist_instead_of_json() {
        let mut tool = todo_tool(REAL_TODO_ARGS, ToolState::Success);
        tool.output = Some(REAL_TODO_OUTPUT.to_string());
        let text = todo_text(&tool);

        assert!(text.contains("Updated Plan · 0/3 完成"), "{text}");
        assert!(!text.contains("done"), "成功态不再另起状态行：{text}");
        assert!(text.contains("[>] 重区地图"), "{text}");
        assert!(text.contains("[ ] 耦合热点"), "{text}");

        // 记账 JSON 一个字都不该上屏。
        assert!(!text.contains("replaced"), "{text}");
        assert!(!text.contains("assigned"), "{text}");
        assert!(!text.contains("current_id"), "{text}");
        assert!(!text.contains("Plan updated"), "{text}");

        // 服务端 `display.header.label` 就是 `"todo"`，拼进头部会得到重复。
        assert!(!text.contains("Todo Write todo"), "{text}");
    }

    /// 失败态：错误正文是证据，照常显示；此时**不能**画清单——那会让人以为
    /// 计划更新成功了。实测 4 次调用挂了 2 次，其中一次入参本身是可解析的
    /// （id 引用了不存在的项），所以必须靠状态判定而不是"参数能不能解析"。
    #[test]
    fn todo_write_failure_keeps_error_body_and_hides_checklist() {
        let mut tool = todo_tool(REAL_TODO_ARGS, ToolState::Failed);
        tool.failure = Some("not_found".to_string());
        tool.output = Some(
            "items[0] references unknown id T1\nHint: Omit \"id\" to assign a new one, or use todo_list to inspect existing IDs."
                .to_string(),
        );
        let text = todo_text(&tool);

        assert!(text.contains("references unknown id T1"), "{text}");
        assert!(text.contains("Hint: Omit"), "{text}");
        assert!(!text.contains("[>] 重区地图"), "{text}");
        // 头部 = 状态词 + 分类 code + 工具名；失败态不报进度（清单没画）。
        assert!(text.contains("Failed (not_found) Todo Write"), "{text}");
        assert!(!text.contains("完成"), "{text}");
    }

    /// 成功但参数解析不出来（契约漂移）：退回通用正文，宁可露出原始返回，
    /// 也不静默吞掉一次计划更新。
    #[test]
    fn todo_write_unparsable_args_fall_back_to_raw_body() {
        let mut tool = tool_block("todo_write", None, ToolState::Success);
        tool.output = Some(REAL_TODO_OUTPUT.to_string());
        assert!(tool.todo.is_none());
        let text = todo_text(&tool);
        assert!(text.contains("replaced"), "{text}");
    }

    /// 排序按 Codex plan cell 的优先级（进行中 → 待办 → 已完成），折叠时
    /// 只留前三行并把溢出交给「点击展开」。
    #[test]
    fn todo_write_orders_active_first_and_folds_overflow() {
        let args = r#"{"items":[
            {"title":"已完成甲","status":"completed"},
            {"title":"待办乙","status":"pending"},
            {"title":"进行中丙","status":"in_progress"},
            {"title":"待办丁","status":"pending"},
            {"title":"已完成戊","status":"completed"}
        ]}"#;
        let collapsed = todo_text(&todo_tool(args, ToolState::Success));
        let active = collapsed.find("进行中丙").expect("active");
        let pending = collapsed.find("待办乙").expect("pending");
        assert!(active < pending, "进行中必须排在待办之前：{collapsed}");
        assert!(
            !collapsed.contains("已完成甲"),
            "折叠时不该画已完成的尾部：{collapsed}"
        );
        assert!(collapsed.contains("… +2 行（点击展开）"), "{collapsed}");

        let mut expanded = todo_tool(args, ToolState::Success);
        expanded.expanded = true;
        let full = todo_text(&expanded);
        assert!(full.contains("已完成甲"), "{full}");
        assert!(!full.contains("点击展开"), "{full}");
    }

    /// sticky 面板：进行中的那项钉在第一行，行数由 `max_rows` 封顶。
    #[test]
    fn todo_panel_pins_active_item_and_folds_to_budget() {
        let args = r#"{"items":[
            {"title":"已完成甲","status":"completed"},
            {"title":"待办乙","status":"pending"},
            {"title":"进行中丙","status":"in_progress"},
            {"title":"待办丁","status":"pending"},
            {"title":"已完成戊","status":"completed"}
        ]}"#;
        let todo = TodoBlock::parse(args).expect("parse");

        let lines = compose_todo_panel(&todo, 40, 4, &theme());
        assert_eq!(lines.len(), 4, "标题 + 2 项 + 溢出提示");
        let text = text_of(&lines);
        assert!(text.starts_with("待办 · 2/5 完成"), "{text}");
        assert!(text.contains("[>] 进行中丙"), "{text}");
        let active = text.find("进行中丙").expect("active");
        let pending = text.find("待办乙").expect("pending");
        assert!(active < pending, "进行中必须排在待办之前：{text}");
        assert!(text.contains("还有 3 项"), "{text}");

        // 行数够时不留溢出提示。
        let full = compose_todo_panel(&todo, 40, 6, &theme());
        assert_eq!(full.len(), 6);
        assert!(!text_of(&full).contains("还有"), "{}", text_of(&full));
    }

    /// 全部完成后面板让位（那条「全部打勾」的记录留在 transcript 卡片里）；
    /// 行数连标题 + 一项都放不下时也不画——半截面板比没有更糟。
    #[test]
    fn todo_panel_hides_when_everything_is_done_or_too_short() {
        let done = TodoBlock::parse(
            r#"{"items":[{"title":"甲","status":"completed"},{"title":"乙","status":"completed"}]}"#,
        )
        .expect("parse");
        assert!(compose_todo_panel(&done, 40, 6, &theme()).is_empty());

        let mixed = TodoBlock::parse(r#"{"items":[{"title":"甲","status":"in_progress"}]}"#)
            .expect("parse");
        assert!(compose_todo_panel(&mixed, 40, 1, &theme()).is_empty());
        assert_eq!(compose_todo_panel(&mixed, 40, 2, &theme()).len(), 2);
    }

    /// 面板是定高摘要：超宽标题按**显示宽度**截断补省略号，不折行、不溢出
    /// （中文按两格算，按字符数截断会撑破边框）。
    #[test]
    fn todo_panel_truncates_long_titles_to_width() {
        let todo = TodoBlock::parse(
            r#"{"items":[{"title":"这是一条特别特别长的待办标题用来验证截断行为","status":"in_progress"}]}"#,
        )
        .expect("parse");
        for width in 20..40usize {
            let lines = compose_todo_panel(&todo, width, 4, &theme());
            assert_eq!(lines.len(), 2, "width={width}");
            for line in &lines {
                assert!(
                    line.width() <= width,
                    "width={width} 面板行超出预算：{}",
                    line.width()
                );
            }
            let item = text_of(&lines[1..]);
            assert!(item.ends_with('…'), "width={width} 未截断：{item}");
        }
    }

    /// 行差是**头部徽标**（跟在路径后面），不再和时长/字节混成一条指标带。
    #[test]
    fn line_stats_render_as_a_header_badge() {
        let render = |tool: &ToolBlock| {
            text_of(&render_block(
                &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool.clone()))),
                60,
                &theme(),
            ))
        };
        let mut tool = tool_block("edit", None, ToolState::Success);
        tool.line_stats = Some(LineStats {
            add: 4,
            del: 2,
            estimating: false,
        });
        let text = render(&tool);
        let header = text.lines().next().unwrap_or_default();
        assert!(header.contains("+4"), "{text}");
        assert!(header.contains("−2"), "{text}");
        assert!(!header.contains("估算"), "终态权威值不得带估算角标: {text}");

        tool.line_stats = Some(LineStats {
            add: 4,
            del: 2,
            estimating: true,
        });
        let text = render(&tool);
        assert!(text.contains("估算"), "流式估算必须自报家门: {text}");
    }

    /// 无正文的成功调用**只占一行**。
    ///
    /// 旧形状每次调用强制两行（头 + `✓ done`），而 `✓ done` 在成功态是零信息量
    /// ——三家参考都把它并进头部。
    #[test]
    fn successful_tool_without_output_is_a_single_line() {
        let mut tool = tool_block("glob", None, ToolState::Success);
        tool.header = Some(ToolHeader::Other {
            label: "**/*.rs".into(),
        });
        let lines = render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            60,
            &theme(),
        );
        assert_eq!(lines.len(), 1, "{}", text_of(&lines));
        assert_eq!(text_of(&lines), "✓ Listed **/*.rs");
    }

    /// 正文挂 `└` 连接符、整体降一档亮度：三档左边缘（0/2/4）收成两档。
    #[test]
    fn tool_body_hangs_off_the_header_with_a_connector() {
        let theme = theme();
        let mut tool = tool_block("read", None, ToolState::Success);
        tool.output = Some("first\nsecond".into());
        let lines = render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            60,
            &theme,
        );
        assert_eq!(
            text_of(&lines),
            "✓ Read\n  └ first\n    second",
            "{}",
            text_of(&lines)
        );
        // 头部 primary（加粗），正文 muted——权重分层。
        assert_eq!(
            lines[1].spans.last().unwrap().style.fg,
            Some(theme.text.muted)
        );
    }

    /// 头部**裁剪**而不是折行：折成两行、第二行还不对齐的头部比截断更难看。
    #[test]
    fn tool_header_clips_instead_of_wrapping() {
        let mut tool = tool_block("exec", None, ToolState::Success);
        tool.header = Some(ToolHeader::Shell {
            command: "cargo test --workspace --all-features -- --nocapture".into(),
        });
        let lines = render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            40,
            &theme(),
        );
        assert_eq!(lines.len(), 1, "头部不得折行");
        let text = text_of(&lines);
        assert!(text.width() <= 40, "头部不得超宽：{text}");
        assert!(text.contains('…'), "超宽头部应截断：{text}");
    }

    fn read_block(id: &str, path: &str) -> TranscriptBlock {
        read_block_in(id, path, ToolState::Success, false)
    }

    fn read_block_in(id: &str, path: &str, state: ToolState, expanded: bool) -> TranscriptBlock {
        let mut tool = tool_block("read", None, state);
        tool.header = Some(ToolHeader::Path {
            path: path.into(),
            op: PathOp::Read,
        });
        tool.expanded = expanded;
        TranscriptBlock::new(id, BlockKind::Tool(Box::new(tool)))
    }

    fn grep_block(id: &str, query: &str) -> TranscriptBlock {
        let mut tool = tool_block("grep", None, ToolState::Success);
        tool.header = Some(ToolHeader::Query {
            query: query.into(),
            scope: None,
        });
        TranscriptBlock::new(id, BlockKind::Tool(Box::new(tool)))
    }

    /// 连续读文件合并成一张卡：一行结论 + 一份清单。
    ///
    /// 旧形状：3 张卡 = 3 行头 + 3 行正文 + 2 个空行；合并后 4 行。
    #[test]
    fn consecutive_reads_collapse_into_one_card() {
        let blocks = vec![
            read_block("r1", "src/a.rs"),
            read_block("r2", "src/b.rs"),
            read_block("r3", "src/c.rs"),
        ];
        let text = text_of(&render_transcript(&blocks, 60, &theme()));
        let expected = "✓ Read 3 files（点击展开）\n  └ src/a.rs\n    src/b.rs\n    src/c.rs";
        assert_eq!(text, expected, "{text}");
    }

    /// 合并的三个前提：**同工具、全成功、未展开**。破一个就拆回独立卡——失败、
    /// 运行中、展开态各自的卡片形状本身就是信息。
    #[test]
    fn lookup_group_needs_same_tool_success_and_collapsed() {
        // 不同工具不合并（read 与 grep 回答的是不同问题）。
        assert_eq!(
            lookup_group_len(&[read_block("r1", "a.rs"), grep_block("g1", "x")]),
            0
        );
        // 中间夹失败：首块自己就不成组（len 1）。
        assert_eq!(
            lookup_group_len(&[
                read_block("r1", "a.rs"),
                read_block_in("r2", "b.rs", ToolState::Failed, false),
            ]),
            0
        );
        // 失败**之前**的两张照常合并。
        assert_eq!(
            lookup_group_len(&[
                read_block("r1", "a.rs"),
                read_block("r2", "b.rs"),
                read_block_in("r3", "c.rs", ToolState::Failed, false),
            ]),
            2
        );
        // 展开态不合并。
        assert_eq!(
            lookup_group_len(&[
                read_block_in("r1", "a.rs", ToolState::Success, true),
                read_block("r2", "b.rs"),
            ]),
            0
        );
        // 单张不成组。
        assert_eq!(lookup_group_len(&[read_block("r1", "a.rs")]), 0);
    }

    /// 展开即拆组：展开的那张回到独立卡，**其余连续的照常合并**（展开的意图
    /// 就是「给我看这一次调用」，不该把邻居也一起炸开）。
    #[test]
    fn expanded_member_drops_out_of_the_group() {
        let blocks = vec![
            read_block_in("r1", "src/a.rs", ToolState::Success, true),
            read_block("r2", "src/b.rs"),
            read_block("r3", "src/c.rs"),
        ];
        let text = text_of(&render_transcript(&blocks, 60, &theme()));
        assert!(text.contains("✓ Read src/a.rs"), "{text}");
        assert!(text.contains("✓ Read 2 files（点击展开）"), "{text}");
        assert_eq!(text.matches("✓ Read").count(), 2, "{text}");
    }

    /// diff 段头 `@@` 是**元数据**：用 gutter 色（比正文暗）退到背景里，而不是拿
    /// `semantic.command`（命令黄）喊出来。
    #[test]
    fn diff_hunk_header_uses_the_gutter_token() {
        let theme = theme();
        let mut tool = tool_block("edit", None, ToolState::Success);
        tool.diff = Some("@@ -1,2 +1,2 @@\n-old\n+new".into());
        let lines = render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            60,
            &theme,
        );
        let hunk = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.starts_with("@@")))
            .and_then(|line| line.spans.last())
            .map(|span| span.style)
            .expect("hunk header line");
        assert_eq!(hunk.fg, Some(theme.diff.gutter_fg));
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

    /// 工具卡**不打时间戳**。
    ///
    /// 参考三家都不给工具调用盖墙钟：转录本身按时间顺序读，每个调用再挂一个
    /// `MM-DD HH:MM` 只是把时间信息重复 N 遍。权威墙钟仍用在用户行上。
    #[test]
    fn tool_card_carries_no_wall_clock() {
        let theme = theme();
        let ts: u64 = 1_759_488_000_000;
        let stamp = format_wall_clock(ts).expect("固定 epoch 必须可格式化");

        let tool = tool_block("exec", None, ToolState::Success);
        let with_ts = TranscriptBlock {
            at_ms: Some(ts),
            ..TranscriptBlock::new("w1", BlockKind::Tool(Box::new(tool.clone())))
        };
        let text = text_of(&render_block(&with_ts, 90, &theme));
        assert!(!text.contains(&stamp), "工具卡不该带墙钟：\n{text}");
    }

    /// 失败态**单显示**回归锁（2026-10-03 双重显示事故）：legacy 摘要首行 =
    /// 失败理由，头部只放分类 code——理由再放一遍就是重复。正文完整承载理由
    /// （失败态不做逐字去重，那是**证据**）。
    #[test]
    fn failed_tool_shows_the_reason_exactly_once() {
        let theme = theme();
        let reason = "file changed since read";
        let mut tool = tool_block("edit", Some(reason), ToolState::Failed);
        tool.output = Some(format!(
            "{reason}
Hint: re-read the file"
        ));
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
            format!("{} Failed (stale_file) Edit", theme.glyph.failure),
            "失败态头部只放分类 code + 工具名，不重复理由：\n{text}"
        );
        assert_eq!(
            text.matches(reason).count(),
            1,
            "错误理由只能出现一次（状态行）：
{text}"
        );
        assert!(
            text.contains("Hint: re-read the file"),
            "正文证据保留：
{text}"
        );
    }

    /// 旧 journal 回放（failure.message 曾是整段 output）：标签已由 adapter
    /// 压成单行，渲染层靠「正文逐字去重 + 头部抑制」保证首行不再三处出现。
    #[test]
    fn replayed_failure_reason_appears_once_not_thrice() {
        let theme = theme();
        let reason = "error: could not compile";
        let mut tool = tool_block("bash", Some(reason), ToolState::Failed);
        tool.output = Some(format!(
            "{reason}
more context follows"
        ));
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
        assert!(
            text.contains("more context follows"),
            "证据后续行保留：
{text}"
        );
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
        // 现行 wire：write 走 typed Path 头（op=Write）；summary 为 None → 正文无逐字去重。
        let mut tool = tool_block("write", None, ToolState::Success);
        tool.header = Some(ToolHeader::Path {
            path: long.clone(),
            op: PathOp::Write,
        });
        tool.output = Some(long.clone());
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            60,
            &theme,
        ));
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.starts_with(&format!("{} ", theme.glyph.success)),
            "{text}"
        );
        assert!(header.contains("Wrote"), "op 是动词事实源：{header}");
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
        assert!(!text.contains("点击展开"), "展开态不应再画折叠提示：{text}");
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
        // 失败态头部只放分类 code + 工具名（legacy 摘要与它重复，见
        // failed_tool_shows_the_reason_exactly_once）；折叠提示照旧。
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.starts_with(&format!("{} Failed (exit 1) Exec", theme().glyph.failure)),
            "{text}"
        );
        // ≥1s 的耗时值得报（亚秒的由 `shell_header_reads_as_a_sentence` 锁掉）。
        assert!(header.contains("1.2s"), "{text}");
        assert!(text.contains("… +5 行（点击展开）"), "{text}");
    }

    /// display 契约的 Shell 头部：`Ran $ <命令>`——命令是真相字段，动词承担状态，
    /// 亚秒耗时不上屏（`0.8s` 对每个快工具都是噪声）。
    #[test]
    fn shell_header_reads_as_a_sentence() {
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
        assert_eq!(header, "✓ Ran cargo check --tests", "{text}");
        assert!(!text.contains("done"), "成功态不再另起状态行：{text}");
        assert!(!text.contains("0.8s"), "亚秒耗时不上屏：{text}");
        assert!(
            !text.contains("exit 0"),
            "成功退出码是噪声，不该上屏：\n{text}"
        );
        assert!(!text.contains("exec"), "wire 名不上屏：\n{text}");
    }

    /// 文件改动族动词矩阵：**op 是唯一事实源**，四个 wire 语义各出一词，不再
    /// 折并成一个 "Edited"；运行中给中性现在时（此刻没有 diff 证据可显示，
    /// 过去式的进行时版是伪形态）；失败态照旧让位给 `Failed (code)` 状态词。
    #[test]
    fn path_op_drives_file_mutation_verbs() {
        let cases = [
            (PathOp::Edit, "edit", ("Edit", "Edited")),
            (PathOp::Write, "write", ("Write", "Wrote")),
            (PathOp::Patch, "apply_patch", ("Patch", "Patched")),
            (PathOp::Delete, "delete", ("Delete", "Deleted")),
        ];
        for (op, name, (present, past)) in cases {
            for (state, expected) in [(ToolState::Running, present), (ToolState::Success, past)] {
                let mut tool = tool_block(name, None, state);
                tool.header = Some(ToolHeader::Path {
                    path: "src/lib.rs".into(),
                    op,
                });
                let text = text_of(&render_block(
                    &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
                    80,
                    &theme(),
                ));
                let header = text.lines().next().unwrap_or_default();
                assert!(
                    header.contains(expected),
                    "{op:?} × {state:?} 应读作 `{expected}`：{text}"
                );
                assert!(
                    !header.contains("Editing") || expected == "Edit",
                    "{op:?} 运行中不得读作 Edited 家族统称：{text}"
                );
            }
        }
    }

    /// 无 Path 头的文件族（legacy 会话 / Other 头兜底）：回退名字臂而不是编造
    /// 家族词——但比 op 缺席前更诚实的是，Unknown op 明确不给动词。
    #[test]
    fn file_family_without_path_header_falls_back_to_name_arm() {
        for name in ["edit", "write", "apply_patch", "delete"] {
            let mut tool = tool_block(name, None, ToolState::Success);
            tool.header = Some(ToolHeader::Other {
                label: "legacy".into(),
            });
            let text = text_of(&render_block(
                &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
                80,
                &theme(),
            ));
            let header = text.lines().next().unwrap_or_default();
            assert!(
                header.contains("Edited"),
                "{name} 无 op 时回退名字臂（现行 wire 兼容）：{text}"
            );
        }
    }

    /// 未入词表的新工具：实名兜底是稳态（Grok 治理纪律——词表每加一项必须
    /// 有配套文案，否则裸实名比编造的家族词诚实）。`copy_range` 发 Write op
    /// 但名字不在任何折并臂里，正好钉住这条边界。
    #[test]
    fn unknown_tools_fall_back_to_display_name() {
        let mut tool = tool_block("copy_range", None, ToolState::Success);
        tool.header = Some(ToolHeader::Path {
            path: "src/a.rs".into(),
            op: PathOp::Write,
        });
        let text = text_of(&render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            80,
            &theme(),
        ));
        let header = text.lines().next().unwrap_or_default();
        // op 在手：op 赢过名字兜底——这是路径首刀的语义。
        assert!(header.contains("Wrote"), "op 在手则 op 是动词源：{text}");

        let tool = tool_block("copy_range", None, ToolState::Success);
        let text = text_of(&render_block(
            &TranscriptBlock::new("t2", BlockKind::Tool(Box::new(tool))),
            80,
            &theme(),
        ));
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("Copy Range"),
            "无 op 无词表 → 实名兜底（不编造家族词）：{text}"
        );
    }

    /// 分离流正文：stdout/stderr 都进正文，**stderr 只换色不占行**（参考三家
    /// 都不额外打标签——颜色本身就是区分）。
    #[test]
    fn streams_body_colors_stderr_without_a_label() {
        let theme = theme();
        let mut tool = tool_block("exec", None, ToolState::Failed);
        tool.streams = Some(ToolStreams {
            stdout: "compiling qaqh-tui".into(),
            stderr: "error: could not compile".into(),
        });
        tool.failure = Some("exit 101".into());
        let lines = render_block(
            &TranscriptBlock::new("t1", BlockKind::Tool(Box::new(tool))),
            80,
            &theme,
        );
        let text = text_of(&lines);
        assert!(text.contains("compiling qaqh-tui"), "{text}");
        assert!(!text.contains("stderr"), "不再占一行打标签：{text}");
        assert!(text.contains("error: could not compile"), "{text}");
        assert!(text.contains("exit 101"), "{text}");
        assert!(
            !text.contains("{\"status\""),
            "模型向 JSON 不得上屏：\n{text}"
        );

        // 颜色是 stderr 的唯一出口：那一行必须是警示色。
        let stderr_style = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("could not compile"))
            })
            .and_then(|line| line.spans.last())
            .map(|span| span.style)
            .expect("stderr line");
        assert_eq!(stderr_style.fg, Some(theme.semantic.warning));
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
             ✓ Read plan.md\n  └ line one\n    line two\n\n· note"
        );
    }
}
