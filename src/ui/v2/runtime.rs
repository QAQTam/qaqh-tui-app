//! Timeline → V2 projector → commit pump 的组合运行时。
//!
//! 这一层是 App 与 terminal 之间的可复用接口：App 只需要在 timeline 事件后调用
//! `sync_timeline` / `replay_from_scratch`，拿到的就是已经过 ledger 裁决、可以
//! `insert_before` 的 `PendingCommit` 列表。

#![allow(dead_code)] // M3.4 运行时先冻结；V2 Agent 外壳由后续里程碑接线。

use std::collections::HashSet;

use crate::app::timeline_model::Turn;
use crate::terminal::transcript::{PendingCommit, TranscriptCommitPump};
use crate::ui::v2::projector::V2TranscriptState;

#[derive(Debug, Clone, Default)]
struct ScrollbackFrontier {
    bootstrapped: bool,
    seen_turn_ids: HashSet<String>,
    max_turn_index: Option<u64>,
}

impl ScrollbackFrontier {
    fn reset_from_turns(&mut self, turns: &[Turn]) {
        self.seen_turn_ids.clear();
        self.max_turn_index = None;
        for turn in turns {
            self.observe(turn);
        }
        self.bootstrapped = !turns.is_empty();
    }

    fn observe(&mut self, turn: &Turn) {
        self.seen_turn_ids.insert(turn.turn_id.clone());
        if let Some(index) = turn.turn_index {
            self.max_turn_index = Some(self.max_turn_index.map_or(index, |max| max.max(index)));
        }
    }

    /// 返回本次可以安全同步的起始下标。
    ///
    /// - 已见过的 turn 必须参与同步，才能接住“同一个活动 turn 后到达的封口事件”；
    /// - 只有 `turn_index` 高于已提交高水位的 turn 才能作为新增内容；
    /// - 位于高水位之前、且从未见过的 turn 一律跳过——这就是分页向前加载和
    ///   re-baseline 带入旧窗口时，不允许倒灌 scrollback 的判据；
    /// - 没有任何重叠或可证明的“更新”边界时返回 `None`，宁可不输出也不冒险
    ///   把旧历史追加到当前 scrollback 尾部。
    fn sync_start(&self, turns: &[Turn]) -> Option<usize> {
        if !self.bootstrapped {
            return (!turns.is_empty()).then_some(0);
        }

        // 旧回合晚封口不能追加到已经写出的新历史之后；从最后一个已知 turn
        // 开始同步，既保持 append-only 顺序，也避免每个 delta 重扫整条历史。
        let last_known = turns
            .iter()
            .rposition(|turn| self.seen_turn_ids.contains(&turn.turn_id));
        let first_after_high_water = turns.iter().position(|turn| {
            turn.turn_index.is_some_and(|index| {
                self.max_turn_index
                    .is_none_or(|max_index| index > max_index)
            })
        });

        match (last_known, first_after_high_water) {
            (Some(known), Some(after)) => Some(known.min(after)),
            (Some(known), None) => Some(known),
            (None, Some(after)) => Some(after),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct V2TranscriptRuntime {
    projector: V2TranscriptState,
    pump: TranscriptCommitPump,
    seed: Option<String>,
    frontier: ScrollbackFrontier,
    timeline_version: Option<u64>,
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
            seed: None,
            frontier: ScrollbackFrontier::default(),
            timeline_version: None,
        }
    }

    /// 增量同步一个 turn；只有首次 Sealed 的块会进入返回队列。
    pub fn sync_turn(&mut self, seed: &str, turn: &Turn) -> Vec<PendingCommit> {
        self.timeline_version = None;
        self.ensure_seed(seed);
        let candidates = self.projector.sync_turn(turn);
        self.frontier.observe(turn);
        self.frontier.bootstrapped = true;
        self.pump.enqueue(seed, candidates);
        self.pump.drain_emittable()
    }

    /// 按 timeline 顺序增量同步；自动过滤分页/re-baseline 带进来的旧回合。
    pub fn sync_timeline(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        self.timeline_version = None;
        self.sync_timeline_inner(seed, turns)
    }

    /// 生产路径：timeline version 未变时零扫描返回。
    pub fn sync_timeline_versioned(
        &mut self,
        seed: &str,
        turns: &[Turn],
        version: u64,
    ) -> Vec<PendingCommit> {
        self.ensure_seed(seed);
        if self.timeline_version == Some(version) {
            return Vec::new();
        }
        let pending = self.sync_timeline_inner(seed, turns);
        self.timeline_version = Some(version);
        pending
    }

    fn sync_timeline_inner(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        self.ensure_seed(seed);
        let Some(start) = self.frontier.sync_start(turns) else {
            return Vec::new();
        };

        let candidates = self.projector.sync_turns(&turns[start..]);
        for turn in &turns[start..] {
            self.frontier.observe(turn);
        }
        self.frontier.bootstrapped = true;
        self.pump.enqueue(seed, candidates);
        self.pump.drain_emittable()
    }

    /// re-baseline / resize / session switch 的完整重放。
    pub fn replay_all(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        self.ensure_seed(seed);
        let candidates = self.projector.replay_all(turns);
        self.frontier.reset_from_turns(turns);
        self.pump.enqueue_replay(seed, candidates);
        self.pump.drain_emittable()
    }

    /// 清空 scrollback 后，为一个 seed 从权威快照完整重放。
    ///
    /// 调用方必须真的清过终端 scrollback；这里会先忘掉该 seed 的 ledger，
    /// 否则完整重放会全部被去重成 `Duplicate`。
    pub fn replay_from_scratch(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        self.begin_replay(seed);
        self.replay_slice(seed, turns)
    }

    /// 清空 scrollback 后完整重放，并记录该快照对应的 timeline version。
    pub fn replay_from_scratch_versioned(
        &mut self,
        seed: &str,
        turns: &[Turn],
        version: u64,
    ) -> Vec<PendingCommit> {
        self.begin_replay(seed);
        let pending = self.replay_slice(seed, turns);
        self.finish_replay(version);
        pending
    }

    /// 准备一次清屏后的分块 replay。
    pub fn begin_replay(&mut self, seed: &str) {
        self.timeline_version = None;
        self.ensure_seed(seed);
        self.pump.reset_seed(seed);
        self.projector.clear();
        self.frontier = ScrollbackFrontier::default();
    }

    /// 重放一段 turn，供首帧分块渐进补齐 scrollback。
    pub fn replay_slice(&mut self, seed: &str, turns: &[Turn]) -> Vec<PendingCommit> {
        self.ensure_seed(seed);
        let candidates = self.projector.replay_all(turns);
        for turn in turns {
            self.frontier.observe(turn);
        }
        self.frontier.bootstrapped |= !turns.is_empty();
        self.pump.enqueue_replay(seed, candidates);
        self.pump.drain_emittable()
    }

    pub fn finish_replay(&mut self, version: u64) {
        self.timeline_version = Some(version);
    }

    /// 用权威快照重建 projector 的已见状态。
    pub fn reset_from_turns(&mut self, turns: &[Turn]) {
        self.projector.reset_from_turns(turns);
        self.frontier.reset_from_turns(turns);
    }

    pub fn clear(&mut self) {
        self.projector.clear();
        self.frontier = ScrollbackFrontier::default();
        self.seed = None;
        self.timeline_version = None;
    }

    pub fn pending_len(&self) -> usize {
        self.pump.len()
    }

    fn ensure_seed(&mut self, seed: &str) {
        if self.seed.as_deref() != Some(seed) {
            self.seed = Some(seed.to_string());
            self.projector.clear();
            self.frontier = ScrollbackFrontier::default();
            self.timeline_version = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::{Block, Round, TimelineModel};
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

    fn model_with_sealed_turns(turns: &[(u64, &str)]) -> TimelineModel {
        let mut model = TimelineModel::default();
        let mut seq = 0;
        for (index, text) in turns {
            let turn_id = format!("turn-{index}");
            seq += 1;
            apply(
                &mut model,
                seq,
                &turn_id,
                TimelineEvent::TurnOpened {
                    user_text: format!("user-{index}"),
                },
            );
            seq += 1;
            apply(
                &mut model,
                seq,
                &turn_id,
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_id: format!("block-{index}"),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: (*text).to_string(),
                        tool: None,
                    },
                },
            );
            model.turns.last_mut().expect("turn just opened").turn_index = Some(*index);
        }
        model
    }

    fn sealed_turn(index: u64) -> Turn {
        Turn {
            turn_id: format!("turn-{index}"),
            turn_index: Some(index),
            user_text: format!("user-{index}"),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            thinking: Default::default(),
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: vec![Block {
                    block_id: format!("block-{index}"),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Sealed,
                    text: format!("answer-{index}"),
                    tool: None,
                    last_fragment: 0,
                    rev: 1,
                }],
            }],
        }
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

    #[test]
    fn prepending_older_turns_does_not_emit_them_into_scrollback() {
        let newer = model_with_sealed_turns(&[(10, "ten"), (11, "eleven")]);
        let mut runtime = V2TranscriptRuntime::new();
        assert_eq!(runtime.sync_timeline("seed", &newer.turns).len(), 4);

        let with_older = model_with_sealed_turns(&[(8, "eight"), (9, "nine")]);
        let mut combined = with_older.turns;
        combined.extend(newer.turns.iter().cloned());

        assert!(
            runtime.sync_timeline("seed", &combined).is_empty(),
            "旧页只能进入内存模型，不能倒灌到 scrollback 尾部"
        );
    }

    #[test]
    fn rebaseline_emits_only_turns_after_the_high_water_mark() {
        let baseline = model_with_sealed_turns(&[(10, "ten"), (11, "eleven")]);
        let mut runtime = V2TranscriptRuntime::new();
        assert_eq!(runtime.sync_timeline("seed", &baseline.turns).len(), 4);

        let expanded = model_with_sealed_turns(&[(10, "ten"), (11, "eleven"), (12, "twelve")]);
        let emitted = runtime.sync_timeline("seed", &expanded.turns);

        assert_eq!(emitted.len(), 2, "只提交 turn-12 的 user + assistant");
        assert!(
            emitted
                .iter()
                .all(|pending| pending.block.turn_id == "turn-12")
        );
    }

    #[test]
    fn replay_from_scratch_resets_only_the_seed_ledger() {
        let a = model_with_sealed_turns(&[(1, "a")]);
        let b = model_with_sealed_turns(&[(1, "b")]);
        let mut runtime = V2TranscriptRuntime::new();

        assert_eq!(runtime.replay_from_scratch("a", &a.turns).len(), 2);
        assert_eq!(runtime.replay_from_scratch("b", &b.turns).len(), 2);
        assert_eq!(
            runtime.replay_from_scratch("a", &a.turns).len(),
            2,
            "切回 a 前会清 scrollback，因此 a 的 ledger 必须允许重放"
        );
        assert!(
            runtime.sync_timeline("b", &b.turns).is_empty(),
            "重置 a 不得让 b 的已提交内容重复"
        );
    }

    #[test]
    fn versioned_sync_uses_timeline_version_gate() {
        let model = model_with_sealed_turns(&[(1, "one")]);
        let mut runtime = V2TranscriptRuntime::new();
        let first = runtime.replay_from_scratch_versioned("seed", &model.turns, model.version);
        assert_eq!(first.len(), 2);
        assert!(
            runtime
                .sync_timeline_versioned("seed", &model.turns, model.version)
                .is_empty(),
            "version 未变时必须零扫描返回"
        );

        let mut expanded = model;
        apply(
            &mut expanded,
            3,
            "turn-2",
            TimelineEvent::TurnOpened {
                user_text: "two".to_string(),
            },
        );
        apply(
            &mut expanded,
            4,
            "turn-2",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "block-2".to_string(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Sealed,
                    text: "two".to_string(),
                    tool: None,
                },
            },
        );
        let emitted = runtime.sync_timeline_versioned("seed", &expanded.turns, expanded.version);
        assert_eq!(emitted.len(), 2);
        assert!(
            emitted
                .iter()
                .all(|pending| pending.block.turn_id == "turn-2")
        );
    }

    #[test]
    fn full_replay_is_not_truncated_by_incremental_queue_cap() {
        let turns: Vec<_> = (0..2500).map(sealed_turn).collect();
        let mut runtime = V2TranscriptRuntime::new();
        let emitted = runtime.replay_from_scratch("seed", &turns);

        assert_eq!(emitted.len(), 5000);
        assert_eq!(emitted.first().unwrap().block.turn_id, "turn-0");
        assert_eq!(emitted.last().unwrap().block.turn_id, "turn-2499");
    }
}
