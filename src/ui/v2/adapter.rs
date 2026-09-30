//! Timeline model → V2 transcript view model。
//!
//! 适配层是唯一允许同时看到 `timeline_model` 与 V2 block 类型的地方；
//! renderer 保持纯输入，便于快照和主题矩阵测试。

use std::collections::HashSet;

use crate::app::timeline_model::{Block, CompactionMark, TimelineModel, ToolCard, Turn};
use crate::ui::v2::transcript::{
    BlockId, BlockKind, BlockState, ToolBlock, ToolState, TranscriptBlock,
};
use qaqh_client::{TimelineBlockKind, TimelineBlockState, TimelineToolState};

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
            BlockKind::Tool(from_tool(tool))
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
        at_ms: None,
    })
}

fn from_tool(tool: &ToolCard) -> ToolBlock {
    ToolBlock {
        name: tool.name.clone(),
        summary: tool.summary.clone(),
        state: tool_state(tool.state),
        output: tool.output.clone(),
        diff: tool.diff.clone(),
        progress: (!tool.progress.is_empty()).then(|| tool.progress.clone()),
        failure: tool.failure.as_ref().map(|failure| {
            if failure.message.is_empty() {
                failure.code.clone()
            } else {
                format!("{}: {}", failure.code, failure.message)
            }
        }),
        duration: None,
        bytes: (tool.progress_bytes_total > 0).then_some(tool.progress_bytes_total),
        expanded: false,
    }
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
    use qaqh_client::{TimelineFailure, TimelineTurnState};

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
}
