//! Timeline model → V2 commit plan。
//!
//! 这个 projector 不直接输出终端内容，也不依赖 App：
//! - 输入是已经由 `TimelineModel` 归约后的 turn；
//! - 输出是首次从 `Live` 进入 `Sealed` 的 V2 block；
//! - `replay_all` 返回全部 sealed block，交给 commit ledger 去重。
//!
//! 这样事件语义仍由既有 `TimelineModel` 负责，V2 只负责“什么时候值得提交”。

#![allow(dead_code)] // M3.3 先冻结投影 API，App 接线由后续 feature flag 承载。

use std::collections::HashMap;

use crate::app::timeline_model::Turn;
use crate::ui::v2::adapter;
use crate::ui::v2::transcript::{BlockId, BlockState, TranscriptBlock};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlockKey {
    turn_id: String,
    block_id: BlockId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeenState {
    Live,
    Planned,
}

#[derive(Debug, Clone, Default)]
pub struct V2TranscriptState {
    seen: HashMap<BlockKey, SeenState>,
    turn_had_rounds: HashMap<String, bool>,
}

impl V2TranscriptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同步一个 turn，返回本 turn 首次进入 Sealed 的 block。
    pub fn sync_turn(&mut self, turn: &Turn) -> Vec<TranscriptBlock> {
        self.note_turn_shape(turn);
        let mut candidates = Vec::new();
        for block in adapter::from_turn(turn) {
            let key = BlockKey {
                turn_id: turn.turn_id.clone(),
                block_id: block.id.clone(),
            };
            let previous = self.seen.get(&key).copied();
            match block.state {
                BlockState::Sealed => {
                    if previous != Some(SeenState::Planned) {
                        self.seen.insert(key, SeenState::Planned);
                        candidates.push(block);
                    }
                }
                BlockState::Live | BlockState::Committed => {
                    if previous.is_none() {
                        self.seen.insert(key, SeenState::Live);
                    }
                }
                BlockState::Discarded => {}
            }
        }
        candidates
    }

    /// 同步多个 turn，保持 timeline 顺序。
    pub fn sync_turns(&mut self, turns: &[Turn]) -> Vec<TranscriptBlock> {
        let mut candidates = Vec::new();
        for turn in turns {
            candidates.extend(self.sync_turn(turn));
        }
        candidates
    }

    /// resize / session switch 的完整重放计划。
    pub fn replay_all(&self, turns: &[Turn]) -> Vec<TranscriptBlock> {
        adapter::from_turns(turns)
            .into_iter()
            .filter(|block| block.state == BlockState::Sealed)
            .collect()
    }

    /// re-baseline 后用权威快照重建“已见”状态，防止下一次增量重复计划。
    pub fn reset_from_turns(&mut self, turns: &[Turn]) {
        self.seen.clear();
        self.turn_had_rounds.clear();
        for turn in turns {
            self.note_turn_shape(turn);
            for block in adapter::from_turn(turn) {
                let key = BlockKey {
                    turn_id: turn.turn_id.clone(),
                    block_id: block.id.clone(),
                };
                let state = match block.state {
                    BlockState::Sealed => SeenState::Planned,
                    BlockState::Live | BlockState::Committed => SeenState::Live,
                    BlockState::Discarded => continue,
                };
                self.seen.insert(key, state);
            }
        }
    }

    pub fn clear(&mut self) {
        self.seen.clear();
        self.turn_had_rounds.clear();
    }

    fn note_turn_shape(&mut self, turn: &Turn) {
        let had_rounds = self
            .turn_had_rounds
            .get(&turn.turn_id)
            .copied()
            .unwrap_or(false);
        if turn.rounds.is_empty() && had_rounds {
            // 后端允许同 turn_id 原地 reopen：rounds 被清空即进入新的一代。
            self.seen.retain(|key, _| key.turn_id != turn.turn_id);
            self.turn_had_rounds.insert(turn.turn_id.clone(), false);
        }
        if !turn.rounds.is_empty() {
            self.turn_had_rounds.insert(turn.turn_id.clone(), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::TimelineModel;
    use qaqh_client::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
        TimelineTurnState,
    };

    fn entry(seq: u64, turn: &str, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry {
            timeline_seq: seq,
            turn_id: turn.to_string(),
            round_num: Some(0),
            event,
        }
    }

    fn apply(model: &mut TimelineModel, seq: u64, turn: &str, event: TimelineEvent) {
        model.apply(&entry(seq, turn, event));
    }

    fn open_text(seq: u64, turn: &str, block: &str, text: &str, model: &mut TimelineModel) {
        apply(
            model,
            seq,
            turn,
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: block.to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: text.to_string(),
                    tool: None,
                },
            },
        );
    }

    #[test]
    fn live_block_is_not_planned_until_sealed() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        let mut state = V2TranscriptState::new();
        assert_eq!(state.sync_turn(&model.turns[0]).len(), 1, "user prompt");

        open_text(2, "turn-1", "b1", "partial", &mut model);
        assert!(state.sync_turn(&model.turns[0]).is_empty());
        assert!(state.sync_turn(&model.turns[0]).is_empty());
    }

    #[test]
    fn sealed_block_is_planned_once() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        open_text(2, "turn-1", "b1", "answer", &mut model);
        let mut state = V2TranscriptState::new();
        assert_eq!(state.sync_turn(&model.turns[0]).len(), 1);
        apply(
            &mut model,
            3,
            "turn-1",
            TimelineEvent::BlockSealed {
                block_id: "b1".to_string(),
            },
        );
        let first = state.sync_turn(&model.turns[0]);
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first[0].kind,
            crate::ui::v2::transcript::BlockKind::Assistant { .. }
        ));
        assert!(state.sync_turn(&model.turns[0]).is_empty());
    }

    #[test]
    fn turn_sealed_seals_open_blocks_and_is_replayable() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        open_text(2, "turn-1", "b1", "answer", &mut model);
        apply(
            &mut model,
            3,
            "turn-1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        );
        let mut state = V2TranscriptState::new();
        let planned = state.sync_turn(&model.turns[0]);
        assert_eq!(planned.len(), 2, "user + sealed open block");
        let replay = state.replay_all(&model.turns);
        assert_eq!(replay.len(), 2);
        assert!(replay.iter().all(|block| block.state == BlockState::Sealed));
    }

    #[test]
    fn rebaseline_reset_does_not_replan_old_blocks() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        open_text(2, "turn-1", "b1", "answer", &mut model);
        apply(
            &mut model,
            3,
            "turn-1",
            TimelineEvent::BlockSealed {
                block_id: "b1".to_string(),
            },
        );
        let mut state = V2TranscriptState::new();
        state.reset_from_turns(&model.turns);
        assert!(state.sync_turn(&model.turns[0]).is_empty());
        assert_eq!(state.replay_all(&model.turns).len(), 2);
    }

    #[test]
    fn reopen_clears_previous_generation() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "first".to_string(),
            },
        );
        open_text(2, "turn-1", "b1", "answer", &mut model);
        apply(
            &mut model,
            3,
            "turn-1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        );
        let mut state = V2TranscriptState::new();
        state.reset_from_turns(&model.turns);

        apply(
            &mut model,
            4,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "second".to_string(),
            },
        );
        let planned = state.sync_turn(&model.turns[0]);
        assert_eq!(planned.len(), 1, "reopened user prompt");
        assert!(matches!(
            planned[0].kind,
            crate::ui::v2::transcript::BlockKind::User { .. }
        ));
    }

    #[test]
    fn large_turn_is_projected_linearly() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        for idx in 0..1_000_u64 {
            let block = format!("b{idx}");
            open_text(idx + 2, "turn-1", &block, "answer", &mut model);
            apply(
                &mut model,
                idx + 1_002,
                "turn-1",
                TimelineEvent::BlockSealed { block_id: block },
            );
        }
        let mut state = V2TranscriptState::new();
        assert_eq!(state.sync_turn(&model.turns[0]).len(), 1_001);
        assert!(state.sync_turn(&model.turns[0]).is_empty());
    }
}
