//! Timeline model → V2 transcript view model。
//!
//! 适配层是唯一允许同时看到 `timeline_model` 与 V2 block 类型的地方；
//! renderer 保持纯输入，便于快照和主题矩阵测试。

use std::collections::HashSet;

use crate::app::timeline_model::{Block, ToolCard, Turn};
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
}
