//! Timeline model → V2 transcript view model。
//!
//! 适配层是唯一允许同时看到 `timeline_model` 与 V2 block 类型的地方；
//! renderer 保持纯输入，便于快照和主题矩阵测试。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::app::timeline_model::{Block, CompactionMark, LineStats, TimelineModel, ToolCard, Turn};
use crate::ui::v2::transcript::{
    BlockId, BlockKind, BlockState, TODO_TOOL_NAME, TodoBlock, ToolBlock, ToolHeader, ToolState,
    ToolStreams, TranscriptBlock,
};
use qaqh_client::{
    TimelineBlockKind, TimelineBlockState, TimelineToolBody, TimelineToolHeader, TimelineToolState,
};
pub fn from_turns(turns: &[Turn]) -> Vec<TranscriptBlock> {
    from_turns_with_expanded(turns, &HashSet::new())
}

pub fn from_turns_with_expanded(
    turns: &[Turn],
    expanded_tools: &HashSet<String>,
) -> Vec<TranscriptBlock> {
    from_turns_with_expanded_blocks(turns, expanded_tools, &HashSet::new())
}

/// 从**整个模型**取块：除回合内容外还要带上无损的窗口外状态（压缩分隔锚）。
///
/// 这是渲染路径的入口：`from_turns_*` 只看 `turns`，看不到
/// `compaction_marks`，用它渲染会静默丢掉「此前已压缩」这条事实。
pub fn from_model_with_expanded_blocks(
    model: &TimelineModel,
    expanded_tools: &HashSet<String>,
    expanded_thinking: &HashSet<String>,
) -> Vec<TranscriptBlock> {
    let blocks = from_turns_with_expanded_blocks(&model.turns, expanded_tools, expanded_thinking);
    splice_compaction_marks(blocks, &model.turns, &model.compaction_marks)
}

/// 把压缩分隔条插进块序列。
///
/// 锚定语义（与 webui W3 同口径）：
/// - 锚点回合仍在窗口里 → 插在该回合**所有块之后**；
/// - 锚点回合已被淘汰（`cap_turns` / 深翻页）或当时还没有回合 → 插在**最前面**。
///
/// 分隔条用 `BlockKind::System` 承载：它本来就是「非对话的系统陈述」，不需要为
/// 一个分隔条再造一种块类型（那会波及命中测试、导出、缓存键三处）。
pub fn splice_compaction_marks(
    mut blocks: Vec<TranscriptBlock>,
    turns: &[Turn],
    marks: &[CompactionMark],
) -> Vec<TranscriptBlock> {
    if marks.is_empty() {
        return blocks;
    }
    // 每个锚点解析成「插在 blocks 的哪个下标之前」。多个锚点时倒序插入，
    // 保证前面的插入不让后面的下标失效。
    let mut insertions: Vec<(usize, usize)> = Vec::with_capacity(marks.len());
    for (mark_index, mark) in marks.iter().enumerate() {
        let at = match mark.after_turn_id.as_deref() {
            Some(turn_id) => match turns.iter().position(|turn| turn.turn_id == turn_id) {
                // 该回合之后 = 该回合最后一块的下一块。
                Some(turn_pos) => {
                    let last_turn_id = &turns[turn_pos].turn_id;
                    blocks
                        .iter()
                        .rposition(|block| &block.turn_id == last_turn_id)
                        .map_or(0, |index| index + 1)
                }
                // 锚点已被淘汰：顶到最前，分隔条不随锚点消失而消失。
                None => 0,
            },
            None => 0,
        };
        insertions.push((at, mark_index));
    }
    insertions.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for (at, mark_index) in insertions {
        let at = at.min(blocks.len());
        blocks.insert(at, compaction_block(&marks[mark_index]));
    }
    blocks
}

fn compaction_block(mark: &CompactionMark) -> TranscriptBlock {
    TranscriptBlock {
        id: BlockId::new(format!("compaction:{}", mark.context_revision)),
        turn_id: mark.after_turn_id.clone().unwrap_or_default(),
        revision: mark.context_revision,
        state: BlockState::Sealed,
        kind: BlockKind::System {
            text: "── 此前已压缩 ──".to_string(),
        },
        at_ms: None,
    }
}

pub fn from_turns_with_expanded_blocks(
    turns: &[Turn],
    expanded_tools: &HashSet<String>,
    expanded_thinking: &HashSet<String>,
) -> Vec<TranscriptBlock> {
    let capacity = turns
        .iter()
        .map(|turn| {
            1 + turn
                .rounds
                .iter()
                .map(|round| round.blocks.len())
                .sum::<usize>()
        })
        .sum();
    let mut blocks = Vec::with_capacity(capacity);
    for turn in turns {
        blocks.extend(from_turn(turn));
    }
    for block in &mut blocks {
        match &mut block.kind {
            BlockKind::Tool(tool) => {
                tool.expanded = expanded_tools.contains(&block.id.to_string());
            }
            BlockKind::Thinking { expanded, .. } => {
                *expanded = expanded_thinking.contains(&block.id.to_string());
            }
            _ => {}
        }
    }
    blocks
}

pub fn from_turn(turn: &Turn) -> Vec<TranscriptBlock> {
    let mut blocks = Vec::new();
    if !turn.user_text.is_empty() {
        blocks.push(TranscriptBlock {
            id: BlockId::new(format!("{}:user", turn.turn_id)),
            turn_id: turn.turn_id.clone(),
            revision: 1,
            state: BlockState::Sealed,
            kind: BlockKind::User {
                text: turn.user_text.clone(),
            },
            // 权威源 fact 墙钟由 v2 信封回填到回合上（见 `Turn::started_at_ms`）。
            at_ms: turn.started_at_ms,
        });
    }
    for round in &turn.rounds {
        for block in &round.blocks {
            if let Some(block) = from_block(&turn.turn_id, turn.sealed, block) {
                blocks.push(block);
            }
        }
    }
    blocks
}

fn from_block(turn_id: &str, turn_sealed: bool, block: &Block) -> Option<TranscriptBlock> {
    let state = if turn_sealed {
        BlockState::Sealed
    } else {
        match block.state {
            TimelineBlockState::Open => BlockState::Live,
            TimelineBlockState::Sealed => BlockState::Sealed,
        }
    };
    // 工具块带终态墙钟（runtime 盖戳）；其余块缺席保持 None。
    let tool_at_ms = match block.kind {
        TimelineBlockKind::Tool => block.tool.as_ref().and_then(|tool| tool.completed_at_ms),
        _ => None,
    };
    let kind = match block.kind {
        TimelineBlockKind::Reasoning => {
            if block.text.is_empty() && state == BlockState::Sealed {
                return None;
            }
            BlockKind::Thinking {
                text: block.text.clone(),
                duration: None,
                expanded: false,
            }
        }
        TimelineBlockKind::Text => BlockKind::Assistant {
            text: block.text.clone(),
        },
        TimelineBlockKind::Tool => {
            let tool = block.tool.as_ref()?;
            BlockKind::Tool(Box::new(from_tool(tool)))
        }
        TimelineBlockKind::Notice => BlockKind::System {
            text: block.text.clone(),
        },
    };
    Some(TranscriptBlock {
        id: BlockId::new(block.block_id.clone()),
        turn_id: turn_id.to_string(),
        revision: block.rev,
        state,
        kind,
        at_ms: tool_at_ms,
    })
}

/// `todo_write` 的清单解析；其余工具恒为 `None`。
///
/// 参数取自 `TimelineTool.args_json`（"Original structured arguments as supplied
/// by the tool producer"）：服务端输出里**没有**清单，只有
/// `replaced`/`total`/`assigned`/`current_id` 这几个记账字段，所以清单必须从入参取。
fn todo_block(name: &str, args_json: Option<&str>) -> Option<TodoBlock> {
    if name != TODO_TOOL_NAME {
        return None;
    }
    TodoBlock::parse(args_json?)
}

/// 当前生效的待办清单 = timeline 里**最后一次成功**的 `todo_write` 入参。
///
/// 与 bugent `currentTodoList` 同思路：只认**成功**的调用——被拒的清单不能当成
/// 生效的（实测 `items[0] references unknown id T1` 那次就是硬失败，参数本身还
/// 能解析，只有状态能区分）。`todo_write` 是整表替换，所以最后一次成功的入参
/// 就是全量。
///
/// 走**前向**扫而不是倒扫，是为了补标题：入参里 `title` 省略时后端沿用同 id 的
/// 旧标题，而最后一次入参里看不到那个旧标题——必须带着 id→标题的历史往前走。
/// 代价是丢掉倒扫的提前退出，但每帧只按 timeline 版本算一次（见
/// `FullscreenView::sync_todo`），与 transcript 缓存同阶。
pub fn current_todo(model: &TimelineModel) -> Option<TodoBlock> {
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut current: Option<TodoBlock> = None;
    for turn in &model.turns {
        for round in &turn.rounds {
            for block in &round.blocks {
                let Some(tool) = block.tool.as_ref() else {
                    continue;
                };
                if tool.name != TODO_TOOL_NAME || tool_state(tool.state) != ToolState::Success {
                    continue;
                }
                let Some(mut todo) = todo_block(&tool.name, tool.args_json.as_deref()) else {
                    continue;
                };
                // 入参省略 `id` 的条目由服务端按顺序分配，成功回执里的 `assigned`
                // 就是那份编号——不读回执的话，本轮**新建**的条目在后续轮次里
                // 永远认不出自己的 id，继承必然落空。
                //
                // 数量对不上说明这次混进了「未知 id 被重编」（后端把它们也塞进
                // `assigned`），一一对应不再成立：那就不记这次，宁可少继承也
                // 不错继承。
                let assigned = assigned_ids(tool);
                let anonymous = todo.items.iter().filter(|item| item.id.is_none()).count();
                let resolvable = assigned.len() == anonymous;
                let mut pending = assigned.iter();
                for item in &mut todo.items {
                    let id = match item.id.clone() {
                        Some(id) => Some(id),
                        None if resolvable => pending.next().cloned(),
                        None => None,
                    };
                    let Some(id) = id else {
                        continue;
                    };
                    if item.title.is_empty() {
                        if let Some(title) = titles.get(&id) {
                            item.title = title.clone();
                        }
                    } else {
                        titles.insert(id, item.title.clone());
                    }
                }
                current = Some(todo);
            }
        }
    }
    current
}

/// 成功回执里的 `assigned`：本轮新分配的 ID，按条目顺序排列。
///
/// 拿不到（老会话没落 `output`、回放、回执被截断）就返回空——调用方据此放弃
/// 这一轮的 id 映射，不会误配。
fn assigned_ids(tool: &ToolCard) -> Vec<String> {
    let Some(output) = tool.output.as_deref() else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    value
        .get("assigned")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn from_tool(tool: &ToolCard) -> ToolBlock {
    let display = tool.display.as_ref();
    let header = display.and_then(|display| typed_header(display.header.as_ref()));
    let body = display.map(|display| typed_body(display.body.as_ref()));
    let outcome = display.and_then(|display| display.outcome.as_ref());
    let metrics = display.and_then(|display| display.metrics.as_ref());

    // 正文两级来源：类型化 body → legacy `output` 原样透出（display 缺席的
    // 自定义工具兜底；不再做任何 JSON 拆包——全部内置工具均已挂 typed
    // display，旧会话兼容臂已删，2026-10-03）。
    let typed_has_content = body.as_ref().is_some_and(|body| body.has_content());
    let (raw_output, streams) = if typed_has_content {
        let body = body.as_ref().expect("typed_has_content");
        (body.output.clone(), body.streams.clone())
    } else {
        (non_empty(tool.output.as_deref().unwrap_or_default()), None)
    };
    // 退出码优先级：typed body → 顶层槽 → outcome。前两者互为同一事实的
    // 投影，取先到者。
    let exit_code = body
        .as_ref()
        .and_then(|body| body.exit_code)
        .or(tool.exit_code)
        .or_else(|| outcome.and_then(|outcome| outcome.exit_code));
    let truncated = body.as_ref().is_some_and(|body| body.truncated)
        || outcome
            .and_then(|outcome| outcome.truncated)
            .unwrap_or(false);
    let output = raw_output.map(|text| normalize_cr(&text));
    let streams = streams.map(|streams| ToolStreams {
        stdout: normalize_cr(&streams.stdout),
        stderr: normalize_cr(&streams.stderr),
    });
    let diff = body
        .as_ref()
        .and_then(|body| body.diff.clone())
        .or_else(|| tool.diff.clone().filter(|diff| !diff.is_empty()))
        .map(|diff| normalize_cr(&diff));
    // 进度的 `\r` 覆盖折叠放宽到所有工具：bash 家族在 ToolCard 已归一（幂等），
    // 其余工具（MCP / 自定义）在此处得到同样的终屏语义。
    let progress = (!tool.progress.is_empty()).then(|| normalize_cr(&tool.progress));

    let state = tool_state(tool.state);
    let summary = if header.is_some() {
        // 头部已有命令/路径真相字段：display.summary（`exit 0 · cargo check`）
        // 会与头部、状态行三处重复——结论收敛到状态行，头部只说「做了什么」。
        None
    } else {
        display
            .and_then(|display| display.summary.as_deref())
            .filter(|summary| !summary.is_empty())
            .map(str::to_owned)
            .or_else(|| tool.summary.clone())
    };

    // 失败标签的证据判断用**解析后的正文**（typed body 优先于 legacy）：
    // 正文非空 → 状态行只放分类 code，理由留给正文，不重复。
    let has_terminal_body = diff.is_some()
        || streams.is_some()
        || output.as_deref().is_some_and(|out| !out.trim().is_empty());

    // 行差数字（渲染唯一出口）：后端终态权威值（`display.lines_*`）优先；运行中
    // 退到 `ToolEstimated` 旁路估算。两者都缺席则不画（含「无变更」0/0）。
    let line_stats = display
        .map(|display| (display.lines_added, display.lines_removed))
        .filter(|(add, del)| *add > 0 || *del > 0)
        .map(|(add, del)| LineStats {
            add,
            del,
            estimating: false,
        })
        .or_else(|| {
            tool.stream_estimate.map(|estimate| LineStats {
                add: estimate.add,
                del: estimate.del,
                estimating: true,
            })
        });

    ToolBlock {
        name: tool.name.clone(),
        summary,
        todo: todo_block(&tool.name, tool.args_json.as_deref()),
        state,
        output,
        diff,
        progress,
        failure: failure_label(tool, state, exit_code, has_terminal_body),
        duration: metrics
            .map(|metrics| Duration::from_millis(metrics.elapsed_ms))
            .or_else(|| {
                outcome
                    .and_then(|outcome| outcome.duration_ms)
                    .map(Duration::from_millis)
            }),
        bytes: metrics
            .map(|metrics| metrics.output_bytes)
            .filter(|bytes| *bytes > 0)
            .or_else(|| {
                outcome
                    .and_then(|outcome| outcome.output_bytes)
                    .filter(|bytes| *bytes > 0)
            })
            .or_else(|| (tool.progress_bytes_total > 0).then_some(tool.progress_bytes_total)),
        expanded: false,
        header,
        streams,
        exit_code,
        truncated,
        line_stats,
    }
}

/// 类型化头部（09-18 契约 §3.3）→ view 头部。
///
/// Path 的 `op` **必须保留**：它是文件改动族动词的唯一事实源
/// （edit→Edited / write→Wrote / patch→Patched / delete→Deleted）。曾因
/// 「工具名已经说了操作」被丢弃——工具名随后被动词折并后此理由失效，四个
/// 语义各异的工具族读起来只剩「Edited」一种说法。
fn typed_header(header: Option<&TimelineToolHeader>) -> Option<ToolHeader> {
    match header? {
        TimelineToolHeader::Shell { command } => Some(ToolHeader::Shell {
            command: command.clone(),
        }),
        TimelineToolHeader::Path { path, op } => Some(ToolHeader::Path {
            path: path.clone(),
            op: *op,
        }),
        TimelineToolHeader::Query { query, scope } => Some(ToolHeader::Query {
            query: query.clone(),
            scope: scope.clone(),
        }),
        TimelineToolHeader::Other { label } => Some(ToolHeader::Other {
            label: label.clone(),
        }),
        TimelineToolHeader::Unknown => None,
    }
}

/// 类型化 body 的展开视图。`Subagent` / `None` / 未知变体不在此投影，
/// 调用方回退 legacy 字段（H16）。
#[derive(Clone, Default)]
struct TypedBody {
    output: Option<String>,
    streams: Option<ToolStreams>,
    diff: Option<String>,
    exit_code: Option<i32>,
    truncated: bool,
}

impl TypedBody {
    /// body 是否携带了任何可见正文；否则回退 legacy / JSON 拆包。
    fn has_content(&self) -> bool {
        self.diff.is_some()
            || self.streams.is_some()
            || self
                .output
                .as_deref()
                .is_some_and(|output| !output.is_empty())
    }
}

fn typed_body(body: Option<&TimelineToolBody>) -> TypedBody {
    match body {
        None | Some(TimelineToolBody::None | TimelineToolBody::Unknown) => TypedBody::default(),
        Some(TimelineToolBody::Subagent { .. }) => TypedBody::default(),
        Some(TimelineToolBody::Text { text, truncated }) => TypedBody {
            output: non_empty(text),
            truncated: *truncated,
            ..TypedBody::default()
        },
        Some(TimelineToolBody::Diff { unified, .. }) => TypedBody {
            diff: non_empty(unified),
            ..TypedBody::default()
        },
        Some(TimelineToolBody::Shell {
            output,
            exit_code,
            truncated,
        }) => TypedBody {
            output: non_empty(output),
            exit_code: *exit_code,
            truncated: *truncated,
            ..TypedBody::default()
        },
        Some(TimelineToolBody::Streams {
            stdout,
            stderr,
            exit_code,
            truncated,
            ..
        }) => TypedBody {
            streams: (!stdout.is_empty() || !stderr.is_empty()).then(|| ToolStreams {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            }),
            exit_code: *exit_code,
            truncated: *truncated,
            ..TypedBody::default()
        },
    }
}

/// 失败标签的状态行上界：状态行只放「为什么失败」的一句话，整段证据在
/// 正文——超界就不再是摘要，是复制。
const FAILURE_LABEL_MAX_CHARS: usize = 80;

/// 失败标签（状态行）：分工是**状态行说分类（结果如何）、正文说证据**，
/// 两者不互相复制。
///
/// 优先级：非零退出码 → `exit N`；正文（output/diff）已有证据 → 裸 code
/// （理由就在正文里，拼进标签必是重复）；正文缺席时理由没有别的落点，
/// 状态行承担 `code: 首行理由`（单行有界，不让多行文本挤进单行 Span）。
fn failure_label(
    tool: &ToolCard,
    state: ToolState,
    exit_code: Option<i32>,
    body_has_evidence: bool,
) -> Option<String> {
    if state != ToolState::Failed {
        return None;
    }
    if let Some(code) = exit_code.filter(|code| *code != 0) {
        return Some(format!("exit {code}"));
    }
    let failure = tool.failure.as_ref()?;
    let code = {
        let code = failure.code.trim();
        if code.is_empty() {
            "tool_execution_failed"
        } else {
            code
        }
    };
    if body_has_evidence {
        return Some(code.to_owned());
    }
    let message = first_line_bounded(&failure.message, FAILURE_LABEL_MAX_CHARS);
    if message.is_empty() || message.eq_ignore_ascii_case(code) {
        return Some(code.to_owned());
    }
    Some(format!("{code}: {message}"))
}

/// 首个非空行压平为单行并按字符数有界（UTF-8 边界安全）。与后端
/// `qaqh_domain::timeline::one_line_bounded` 同语义，是旧数据回放的防御副本
/// （新数据在后端已收敛，此函数只在 TUI 侧兜底）。
fn first_line_bounded(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .map(|ch| if ch == '\r' || ch == '\t' { ' ' } else { ch })
        .collect::<String>();
    let trimmed = line.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let mut bounded: String = trimmed.chars().take(max_chars).collect();
    bounded.push('…');
    bounded
}

/// 终屏归一（与后端 exec display 的 `normalize_carriage_returns` 同语义）：
/// CRLF → LF；行内 `\r` 覆盖只留最后一个非空段（apt/spinner 进度行）；
/// 纯 `\r` 行丢弃。对已归一的文本幂等。
fn normalize_cr(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        if let Some(segment) = line.rsplit('\r').find(|segment| !segment.is_empty()) {
            lines.push(segment);
        } else if !line.contains('\r') {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

const fn tool_state(state: TimelineToolState) -> ToolState {
    match state {
        TimelineToolState::Prepared => ToolState::Prepared,
        TimelineToolState::Running => ToolState::Running,
        TimelineToolState::Succeeded => ToolState::Success,
        TimelineToolState::Failed => ToolState::Failed,
        TimelineToolState::Cancelled => ToolState::Cancelled,
        TimelineToolState::Backgrounded => ToolState::Backgrounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{TimelineFailure, TimelineToolDisplay, TimelineTurnState};

    /// `todo_write` 的清单来自入参 `args_json`——服务端输出里只有
    /// `replaced`/`total`/`assigned`/`current_id` 记账，没有清单本身。
    #[test]
    fn todo_block_only_parses_todo_write() {
        let args = r#"{"items":[{"title":"a","status":"pending"}]}"#;
        assert!(todo_block("todo_write", Some(args)).is_some());
        assert!(todo_block("exec", Some(args)).is_none());
        assert!(todo_block("todo_write", None).is_none());
        assert!(todo_block("todo_write", Some("not json")).is_none());
    }

    /// 当前清单 = **最后一次成功**的 `todo_write`。
    ///
    /// 失败的那次参数本身可解析（真实日志里 `id` 引用了不存在的项），只有状态
    /// 能区分——所以扫的时候必须带状态过滤，否则面板会显示一份根本没落地的计划；
    /// 同理运行中的那次也不能算数（参数可能还没到齐）。
    #[test]
    fn current_todo_takes_the_last_successful_write() {
        let model = model(vec![
            turn(vec![todo_tool_block(
                "old",
                TimelineToolState::Succeeded,
                r#"{"items":[{"title":"旧计划","status":"pending"}]}"#,
            )]),
            turn(vec![
                todo_tool_block(
                    "rejected",
                    TimelineToolState::Failed,
                    r#"{"items":[{"id":"T9","title":"被拒的计划","status":"pending"}]}"#,
                ),
                todo_tool_block(
                    "running",
                    TimelineToolState::Running,
                    r#"{"items":[{"title":"运行中的计划","status":"pending"}]}"#,
                ),
            ]),
        ]);

        let todo = current_todo(&model).expect("todo");
        assert_eq!(todo.items.len(), 1);
        assert_eq!(todo.items[0].title, "旧计划");
    }

    /// 没有任何成功的 `todo_write` 时返回 `None`（面板不显示），而不是 panic
    /// 或退回最后一条失败参数。
    #[test]
    fn current_todo_is_none_without_a_successful_write() {
        assert!(current_todo(&TimelineModel::default()).is_none());

        let model = model(vec![
            turn(vec![tool_block_of(
                "other",
                "exec",
                TimelineToolState::Succeeded,
            )]),
            turn(vec![todo_tool_block(
                "cancelled",
                TimelineToolState::Cancelled,
                r#"{"items":[{"title":"取消的计划","status":"pending"}]}"#,
            )]),
        ]);
        assert!(current_todo(&model).is_none());
    }

    /// `TimelineModel` 有私有字段，测试里不能直接写字面量。
    fn model(turns: Vec<Turn>) -> TimelineModel {
        let mut model = TimelineModel::default();
        model.turns = turns;
        model
    }

    /// 入参省略 `title` 时，面板要显示**上一版**的标题（后端沿用同 id 的旧标题），
    /// 而不是空串或光秃秃的 id。
    ///
    /// 这里必须带上成功回执：第一轮建 T1 时入参里没有 id，只有回执的
    /// `assigned` 才说得清「那条就是 T1」。
    #[test]
    fn current_todo_inherits_titles_across_writes() {
        let model = model(vec![
            turn(vec![todo_tool_block_with(
                "first",
                TimelineToolState::Succeeded,
                r#"{"items":[{"title":"跑通后端契约","status":"in_progress"}]}"#,
                Some(
                    r#"{"replaced":0,"total":1,"assigned":["T1"],"current_id":"T1","message":"Plan updated: 1 item(s) (1 new).","status":"ok"}"#,
                ),
            )]),
            turn(vec![todo_tool_block(
                "second",
                TimelineToolState::Succeeded,
                r#"{"items":[{"id":"T1","status":"completed"},{"id":"T2","status":"pending"}]}"#,
            )]),
        ]);
        let todo = current_todo(&model).expect("todo");
        assert_eq!(todo.items.len(), 2);
        assert_eq!(todo.items[0].title, "跑通后端契约");
        // T2 从没出现过：继承不到就保持空，由渲染层退回显示 id。
        assert_eq!(todo.items[1].title, "");
    }

    /// 回执缺失（老会话 / 回放）时不能瞎猜 id：宁可继承不到，也不能把 T1 的
    /// 标题安到 T2 头上。
    #[test]
    fn current_todo_skips_title_inheritance_without_an_assigned_receipt() {
        let model = model(vec![
            turn(vec![todo_tool_block(
                "first",
                TimelineToolState::Succeeded,
                r#"{"items":[{"title":"跑通后端契约","status":"in_progress"}]}"#,
            )]),
            turn(vec![todo_tool_block(
                "second",
                TimelineToolState::Succeeded,
                r#"{"items":[{"id":"T1","status":"completed"}]}"#,
            )]),
        ]);
        let todo = current_todo(&model).expect("todo");
        assert_eq!(todo.items[0].title, "");
    }

    fn todo_tool_block(id: &str, state: TimelineToolState, args: &str) -> Block {
        todo_tool_block_with(id, state, args, None)
    }

    fn todo_tool_block_with(
        id: &str,
        state: TimelineToolState,
        args: &str,
        output: Option<&str>,
    ) -> Block {
        let mut block = tool_block_of(id, "todo_write", state);
        let tool = block.tool.as_mut().expect("tool");
        tool.args_json = Some(args.to_string());
        tool.output = output.map(str::to_owned);
        block
    }

    fn tool_block_of(id: &str, name: &str, state: TimelineToolState) -> Block {
        let mut block = block(id, TimelineBlockKind::Tool, TimelineBlockState::Sealed, "");
        block.tool = Some(tool_card(name, state));
        block
    }

    fn block(id: &str, kind: TimelineBlockKind, state: TimelineBlockState, text: &str) -> Block {
        Block {
            block_id: id.to_string(),
            block_order: 0,
            kind,
            state,
            text: text.to_string(),
            tool: None,
            last_fragment: 0,
            rev: 7,
        }
    }

    fn turn(blocks: Vec<Block>) -> Turn {
        Turn {
            turn_id: "turn-1".to_string(),
            turn_index: Some(1),
            started_at_ms: None,
            user_text: "hello".to_string(),
            state: TimelineTurnState::Running,
            failure: None,
            sealed: false,
            offloaded: false,
            thinking: Default::default(),
            rounds: vec![crate::app::timeline_model::Round {
                round_num: 1,
                sealed: false,
                is_final: false,
                blocks,
            }],
        }
    }

    #[test]
    fn maps_user_and_core_blocks_in_order() {
        let blocks = from_turn(&turn(vec![
            block(
                "a",
                TimelineBlockKind::Text,
                TimelineBlockState::Open,
                "answer",
            ),
            block(
                "t",
                TimelineBlockKind::Reasoning,
                TimelineBlockState::Open,
                "thinking",
            ),
        ]));
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0].kind, BlockKind::User { .. }));
        assert!(matches!(blocks[1].kind, BlockKind::Assistant { .. }));
        assert!(matches!(blocks[2].kind, BlockKind::Thinking { .. }));
        assert_eq!(blocks[1].revision, 7);
    }

    #[test]
    fn expanded_thinking_ids_flow_into_the_view_model() {
        let turns = vec![turn(vec![block(
            "thinking",
            TimelineBlockKind::Reasoning,
            TimelineBlockState::Sealed,
            "first\nsecond",
        )])];
        let expanded = HashSet::from(["thinking".to_string()]);
        let blocks = from_turns_with_expanded_blocks(&turns, &HashSet::new(), &expanded);
        let Some(TranscriptBlock {
            kind: BlockKind::Thinking { expanded, .. },
            ..
        }) = blocks.get(1)
        else {
            panic!("thinking block missing");
        };
        assert!(*expanded);
    }

    #[test]
    fn sealed_empty_reasoning_is_not_rendered() {
        let blocks = from_turn(&turn(vec![block(
            "r",
            TimelineBlockKind::Reasoning,
            TimelineBlockState::Sealed,
            "",
        )]));
        assert_eq!(blocks.len(), 1, "only user block remains");
    }

    #[test]
    fn expanded_tool_ids_flow_into_the_view_model() {
        let mut tool_block = block(
            "tool",
            TimelineBlockKind::Tool,
            TimelineBlockState::Sealed,
            "",
        );
        tool_block.tool = Some(ToolCard {
            exit_code: None,
            completed_at_ms: None,
            tool_call_id: "call".to_string(),
            name: "exec".to_string(),
            state: TimelineToolState::Succeeded,
            summary: Some("cargo test".to_string()),
            args_json: None,
            output: Some("output".to_string()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
            stream_estimate: None,
        });
        let turns = vec![turn(vec![tool_block])];
        let expanded = HashSet::from(["tool".to_string()]);
        let blocks = from_turns_with_expanded(&turns, &expanded);
        let Some(TranscriptBlock {
            kind: BlockKind::Tool(tool),
            ..
        }) = blocks.get(1)
        else {
            panic!("tool block missing");
        };
        assert!(tool.expanded);
    }

    #[test]
    fn maps_tool_terminal_state_and_failure() {
        let mut tool_block = block(
            "tool",
            TimelineBlockKind::Tool,
            TimelineBlockState::Sealed,
            "",
        );
        tool_block.tool = Some(ToolCard {
            exit_code: None,
            completed_at_ms: None,
            tool_call_id: "call".to_string(),
            name: "exec".to_string(),
            state: TimelineToolState::Failed,
            summary: Some("cargo test".to_string()),
            args_json: None,
            output: Some("failure output".to_string()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 2048,
            progress_stream: None,
            failure: Some(TimelineFailure {
                code: "exit_1".to_string(),
                message: "one test failed".to_string(),
            }),
            permission: None,
            display: None,
            stream_estimate: None,
        });
        let blocks = from_turn(&turn(vec![tool_block]));
        let Some(TranscriptBlock {
            kind: BlockKind::Tool(tool),
            ..
        }) = blocks.get(1)
        else {
            panic!("tool block missing");
        };
        assert_eq!(tool.state, ToolState::Failed);
        assert_eq!(tool.bytes, Some(2048));
        assert!(
            tool.failure
                .as_deref()
                .is_some_and(|value| value.contains("exit_1"))
        );
    }

    /// 用户块必须带上**权威**墙钟（来自 v2 信封，落在回合上）；缺席保持 None。
    #[test]
    fn user_block_carries_the_authoritative_wall_clock_only_when_known() {
        let mut unknown = turn(vec![]);
        unknown.started_at_ms = None;
        assert_eq!(from_turn(&unknown)[0].at_ms, None);

        let mut known = turn(vec![]);
        known.started_at_ms = Some(1_759_000_000_000);
        assert_eq!(from_turn(&known)[0].at_ms, Some(1_759_000_000_000));
    }

    /// W3/D10：压缩分隔条插在锚点回合之后，且不改变原有块的顺序。
    #[test]
    fn compaction_mark_splices_after_its_anchor_turn() {
        let mut first = turn(vec![block(
            "a",
            TimelineBlockKind::Text,
            TimelineBlockState::Sealed,
            "one",
        )]);
        first.turn_id = "t1".to_string();
        let mut second = turn(vec![block(
            "b",
            TimelineBlockKind::Text,
            TimelineBlockState::Sealed,
            "two",
        )]);
        second.turn_id = "t2".to_string();
        let turns = vec![first, second];
        let marks = vec![CompactionMark {
            after_turn_id: Some("t1".to_string()),
            context_revision: 9,
        }];

        let blocks = from_turns_with_expanded_blocks(&turns, &HashSet::new(), &HashSet::new());
        let spliced = splice_compaction_marks(blocks, &turns, &marks);
        let kinds: Vec<&str> = spliced
            .iter()
            .map(|block| match &block.kind {
                BlockKind::User { .. } => "user",
                BlockKind::Assistant { .. } => "assistant",
                BlockKind::System { .. } => "divider",
                BlockKind::Thinking { .. } => "thinking",
                BlockKind::Tool(_) => "tool",
            })
            .collect();
        // user(t1) assistant(t1) divider user(t2) assistant(t2)
        assert_eq!(kinds, ["user", "assistant", "divider", "user", "assistant"]);
        assert_eq!(spliced[2].id.to_string(), "compaction:9");
    }

    /// 锚点回合已被淘汰（`cap_turns` / 深翻页）时分隔条顶到最前，而不是消失。
    #[test]
    fn compaction_mark_with_evicted_anchor_goes_to_the_top() {
        let turns = vec![turn(vec![block(
            "a",
            TimelineBlockKind::Text,
            TimelineBlockState::Sealed,
            "one",
        )])];
        let marks = vec![CompactionMark {
            after_turn_id: Some("turn-evicted".to_string()),
            context_revision: 3,
        }];
        let blocks = from_turns_with_expanded_blocks(&turns, &HashSet::new(), &HashSet::new());
        let spliced = splice_compaction_marks(blocks, &turns, &marks);
        assert!(matches!(spliced[0].kind, BlockKind::System { .. }));
        assert_eq!(spliced.len(), 3);
    }

    /// 窗口里还没有回合时的锚（`None`）同样渲染在顶部。
    #[test]
    fn compaction_mark_without_turns_renders_at_the_top() {
        let marks = vec![CompactionMark {
            after_turn_id: None,
            context_revision: 1,
        }];
        let spliced = splice_compaction_marks(Vec::new(), &[], &marks);
        assert_eq!(spliced.len(), 1);
        assert!(matches!(spliced[0].kind, BlockKind::System { .. }));
    }

    /// 分隔条文本是「此前已压缩」——与 webui W3 同一句用户可见文案。
    #[test]
    fn compaction_divider_text_is_user_visible() {
        let marks = vec![CompactionMark {
            after_turn_id: None,
            context_revision: 1,
        }];
        let spliced = splice_compaction_marks(Vec::new(), &[], &marks);
        let Some(TranscriptBlock {
            kind: BlockKind::System { text },
            ..
        }) = spliced.first()
        else {
            panic!("divider missing");
        };
        assert!(text.contains("此前已压缩"), "{text}");
    }

    /// display 契约的 exec 卡：头部是 Shell 命令、正文是 `\r` 归一后的输出，
    /// 模型向 JSON（legacy output）彻底不上屏；summary 收敛进状态行。
    #[test]
    fn display_shell_card_renders_body_not_model_json() {
        let mut card = tool_card("exec", TimelineToolState::Succeeded);
        card.output = Some(
            r#"{"status":"completed","exit_code":0,"output":"hello\r\nworld\r\n"}"#.to_string(),
        );
        card.display = Some(TimelineToolDisplay {
            summary: Some("exit 0 · cargo check".to_string()),
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: Some(TimelineToolHeader::Shell {
                command: "cargo check".to_string(),
            }),
            body: Some(TimelineToolBody::Shell {
                output: "hello\r\nworld\r\n".to_string(),
                exit_code: Some(0),
                truncated: false,
            }),
            metrics: None,
            outcome: None,
        });
        let tool = from_tool(&card);
        assert_eq!(
            tool.header,
            Some(ToolHeader::Shell {
                command: "cargo check".to_string()
            })
        );
        assert_eq!(tool.summary, None, "头部在时 summary 全量去重");
        assert_eq!(tool.output.as_deref(), Some("hello\nworld\n"));
        assert_eq!(tool.streams, None);
        assert_eq!(tool.exit_code, Some(0));
    }

    /// Path 头的 op 是文件改动族动词的唯一事实源，投影**不得丢弃**：
    /// 曾因「工具名已经说了操作」被略去，四个语义各异的工具族
    /// （edit/write/apply_patch/delete）读起来只剩「Edited」一种说法。
    #[test]
    fn path_header_keeps_op_as_verb_fact() {
        for (wire_op, view_op) in [
            (
                qaqh_client::TimelinePathOp::Edit,
                super::super::transcript::PathOp::Edit,
            ),
            (
                qaqh_client::TimelinePathOp::Write,
                super::super::transcript::PathOp::Write,
            ),
            (
                qaqh_client::TimelinePathOp::Patch,
                super::super::transcript::PathOp::Patch,
            ),
            (
                qaqh_client::TimelinePathOp::Delete,
                super::super::transcript::PathOp::Delete,
            ),
            (
                qaqh_client::TimelinePathOp::Read,
                super::super::transcript::PathOp::Read,
            ),
        ] {
            let mut card = tool_card("edit", TimelineToolState::Succeeded);
            card.display = Some(TimelineToolDisplay {
                summary: None,
                diff: None,
                lines_added: 0,
                lines_removed: 0,
                header: Some(TimelineToolHeader::Path {
                    path: "src/lib.rs".to_string(),
                    op: wire_op,
                }),
                body: None,
                metrics: None,
                outcome: None,
            });
            let tool = from_tool(&card);
            assert_eq!(
                tool.header,
                Some(ToolHeader::Path {
                    path: "src/lib.rs".to_string(),
                    op: view_op,
                }),
                "{wire_op:?} 必须原样投影"
            );
        }
    }

    /// H16 兜底（无 display）：output 原样透出——JSON 拆包考古层已删
    /// （2026-10-03），全部内置工具挂 typed display 后不再有 JSON 模型向
    /// 正文需要拆解。
    #[test]
    fn json_output_passes_through_without_display() {
        let json = r#"{"status":"failed","exit_code":2,"output":"boom"}"#;
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.output = Some(json.to_string());
        let tool = from_tool(&card);
        assert_eq!(tool.streams, None, "不再拆流");
        assert_eq!(tool.output.as_deref(), Some(json), "正文原样透出");
    }

    /// 无 display 且 output 不是 exec 形 JSON：正文原样透出，但 `\r` 覆盖行
    /// 折叠成终屏语义（CRLF → LF、行内覆盖只留最后一段）。
    #[test]
    fn plain_output_passes_through_with_cr_normalized() {
        let mut card = tool_card("exec", TimelineToolState::Succeeded);
        card.output = Some("50%\r100%\r\nprogress done\n".to_string());
        let tool = from_tool(&card);
        assert_eq!(tool.streams, None);
        assert_eq!(tool.output.as_deref(), Some("100%\nprogress done\n"));
    }

    /// 非 JSON 的失败 message 照旧完整透出（`code: message`），不受 JSON 拆包影响。
    #[test]
    fn plain_failure_message_is_kept() {
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.failure = Some(TimelineFailure {
            code: "spawn_failed".to_string(),
            message: "no supported shell found".to_string(),
        });
        let tool = from_tool(&card);
        assert_eq!(
            tool.failure.as_deref(),
            Some("spawn_failed: no supported shell found")
        );
    }

    /// 失败标签硬化：多行 / 超长 message 压成单行有界，不挤爆状态行 Span。
    #[test]
    fn failure_label_bounds_messages_to_one_line() {
        // 多行 message：首行。
        let mut card = tool_card("edit", TimelineToolState::Failed);
        card.failure = Some(TimelineFailure {
            code: "stale_file".to_string(),
            message: "file changed since read
Hint: re-read the file"
                .to_string(),
        });
        assert_eq!(
            from_tool(&card).failure.as_deref(),
            Some("stale_file: file changed since read")
        );

        // 无换行的超长单行：有界 + `…`。
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.failure = Some(TimelineFailure {
            code: "execution".to_string(),
            message: "x".repeat(FAILURE_LABEL_MAX_CHARS + 40),
        });
        let label = from_tool(&card).failure.expect("label");
        assert_eq!(
            label.chars().count(),
            "execution: ".len() + FAILURE_LABEL_MAX_CHARS + 1
        );
        assert!(label.ends_with('…'));
        assert!(!label.contains('\n'), "标签必须是单行");
    }

    /// message 与 code 同义（exec timeout：`timeout`/`timeout`）时不再拼第二遍。
    #[test]
    fn failure_label_deduplicates_message_that_repeats_the_code() {
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.failure = Some(TimelineFailure {
            code: "timeout".to_string(),
            message: "timeout".to_string(),
        });
        assert_eq!(from_tool(&card).failure.as_deref(), Some("timeout"));
    }

    /// 契约切片：顶层 exit_code/completed_at_ms 进入 ToolCard，并参与退出码
    /// 优先级——display 缺席时标签仍能给出 `exit N`，无需 JSON 考古。
    #[test]
    fn terminal_slots_flow_through_and_feed_the_exit_label() {
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.output = Some("boom".to_string());
        card.exit_code = Some(101);
        card.completed_at_ms = Some(1_759_488_000_000);
        let tool = from_tool(&card);
        assert_eq!(tool.exit_code, Some(101));
        assert_eq!(tool.failure.as_deref(), Some("exit 101"));

        // typed body 优先于顶层槽（同一事实的两个投影，先到者胜）。
        let mut card = tool_card("exec", TimelineToolState::Failed);
        card.exit_code = Some(1);
        card.display = Some(TimelineToolDisplay {
            summary: None,
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: None,
            body: Some(qaqh_client::TimelineToolBody::Shell {
                output: String::new(),
                exit_code: Some(7),
                truncated: false,
            }),
            metrics: None,
            outcome: None,
        });
        assert_eq!(from_tool(&card).exit_code, Some(7));
    }

    #[test]
    fn terminal_display_lines_become_authoritative_line_stats() {
        let mut card = tool_card("edit", TimelineToolState::Succeeded);
        card.display = Some(TimelineToolDisplay {
            summary: None,
            diff: None,
            lines_added: 3,
            lines_removed: 1,
            header: None,
            body: None,
            metrics: None,
            outcome: None,
        });
        assert_eq!(
            from_tool(&card).line_stats,
            Some(LineStats {
                add: 3,
                del: 1,
                estimating: false
            })
        );
    }

    #[test]
    fn streaming_estimate_is_flagged_estimating_and_loses_to_terminal() {
        use crate::app::timeline_model::StreamEstimate;

        let mut card = tool_card("edit", TimelineToolState::Running);
        card.stream_estimate = Some(StreamEstimate { add: 5, del: 2 });
        assert_eq!(
            from_tool(&card).line_stats,
            Some(LineStats {
                add: 5,
                del: 2,
                estimating: true
            })
        );

        // 终态权威值一到，估算让位。
        card.display = Some(TimelineToolDisplay {
            summary: None,
            diff: None,
            lines_added: 5,
            lines_removed: 4,
            header: None,
            body: None,
            metrics: None,
            outcome: None,
        });
        assert_eq!(
            from_tool(&card).line_stats,
            Some(LineStats {
                add: 5,
                del: 4,
                estimating: false
            })
        );
    }

    fn tool_card(name: &str, state: TimelineToolState) -> ToolCard {
        ToolCard {
            exit_code: None,
            completed_at_ms: None,
            tool_call_id: "c1".to_string(),
            name: name.to_string(),
            state,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
            stream_estimate: None,
        }
    }
}
