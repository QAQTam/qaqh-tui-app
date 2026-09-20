//! Timeline → V2 projector → commit pump 的组合运行时。
//!
//! 这一层是 App 与 terminal 之间的可复用接口：App 只需要在 timeline 事件后调用
//! `sync_turn` / `replay_all`，拿到的就是已经过 ledger 裁决、可以 `insert_before`
//! 的 `PendingCommit` 列表。

#![allow(dead_code)] // M3.4 运行时先冻结；V2 Agent 外壳由后续里程碑接线。

use crate::app::timeline_model::Turn;
use crate::terminal::transcript::{PendingCommit, TranscriptCommitPump};
use crate::ui::v2::projector::V2TranscriptState;

#[derive(Debug, Clone)]
pub struct V2TranscriptRuntime {
    projector: V2TranscriptState,
    pump: TranscriptCommitPump,
}

impl Default for V2TranscriptRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl V2TranscriptRuntime {
    pub fn new() -> Self {
        Self {
            projector: V2TranscriptState::new(),
            pump: TranscriptCommitPump::default(),
        }
    }

    /// 增量同步一个 turn；只有首次 Sealed 的块会进入返回队列。
    pub fn sync_turn(&mut self, seed: &str, turn: &Turn) -> Vec<PendingCommit> {
        let candidates = self.projector.sync_turn(turn);
        self.pump.enqueue(seed, candidates);
        self.pump.drain_emittable()
    }

    /// re-baseline / resize / session switch 的完整重放。
    pub fn replay_all(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        let candidates = self.projector.replay_all(turns);
        self.pump.enqueue(seed, candidates);
        self.pump.drain_emittable()
    }

    /// 用权威快照重建 projector 的已见状态。
    pub fn reset_from_turns(&mut self, turns: &[Turn]) {
        self.projector.reset_from_turns(turns);
    }

    pub fn clear(&mut self) {
        self.projector.clear();
    }

    pub fn pending_len(&self) -> usize {
        self.pump.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::TimelineModel;
    use qaqh_client::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
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

    #[test]
    fn runtime_emits_user_then_sealed_answer_once() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        let mut runtime = V2TranscriptRuntime::new();
        assert_eq!(runtime.sync_turn("seed", &model.turns[0]).len(), 1);
        assert!(runtime.sync_turn("seed", &model.turns[0]).is_empty());

        apply(
            &mut model,
            2,
            "turn-1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: "answer".to_string(),
                    tool: None,
                },
            },
        );
        assert!(runtime.sync_turn("seed", &model.turns[0]).is_empty());
        apply(
            &mut model,
            3,
            "turn-1",
            TimelineEvent::BlockSealed {
                block_id: "b1".to_string(),
            },
        );
        assert_eq!(runtime.sync_turn("seed", &model.turns[0]).len(), 1);
        assert!(runtime.sync_turn("seed", &model.turns[0]).is_empty());
    }

    #[test]
    fn runtime_replay_uses_ledger_deduplication() {
        let mut model = TimelineModel::default();
        apply(
            &mut model,
            1,
            "turn-1",
            TimelineEvent::TurnOpened {
                user_text: "hello".to_string(),
            },
        );
        apply(
            &mut model,
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
        );
        let mut runtime = V2TranscriptRuntime::new();
        assert_eq!(runtime.replay_all("seed", &model.turns).len(), 2);
        assert!(runtime.replay_all("seed", &model.turns).is_empty());
    }
}
