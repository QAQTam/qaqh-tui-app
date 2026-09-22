//! V2 transcript commit ledger 适配。
//!
//! `CommitLedger` 只认识 `CommitId + content_hash`；本层把 V2 block 的
//! `turn_id / block_id / revision / content_fingerprint` 接上，并在 Emit
//! 成功时推进 block 到 `Committed`。

#![allow(dead_code)] // M3.2 inline 接线前先冻结 API。

use std::collections::VecDeque;

use crate::terminal::commit::{CommitDecision, CommitId, CommitLedger, content_hash};
use crate::ui::v2::transcript::{BlockState, TranscriptBlock};

const DEFAULT_PENDING_CAP: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct TranscriptCommitLedger {
    ledger: CommitLedger,
}

impl TranscriptCommitLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只允许封口块进入提交账本。
    ///
    /// - `Emit`：登记并推进 block 到 `Committed`；
    /// - `Duplicate`：同 id 同内容，不重复输出；
    /// - `Conflict`：同 id 不同内容，拒绝输出并保留 block 原状态。
    pub fn commit_block(
        &mut self,
        seed: &str,
        block: &mut TranscriptBlock,
    ) -> Option<CommitDecision> {
        if block.state != BlockState::Sealed {
            return None;
        }
        let id = CommitId::new(seed, &block.turn_id, block.id.as_str(), block.revision);
        let hash = content_hash(&block.content_fingerprint());
        let decision = self.ledger.commit(id, hash);
        if decision == CommitDecision::Emit {
            block.state = BlockState::Committed;
        }
        Some(decision)
    }

    pub fn contains(&self, id: &CommitId) -> bool {
        self.ledger.contains(id)
    }

    pub fn len(&self) -> usize {
        self.ledger.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ledger.is_empty()
    }

    pub fn forget_seed(&mut self, seed: &str) {
        self.ledger.forget_seed(seed);
    }
}

/// 等待写入 terminal scrollback 的已决策提交。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommit {
    pub seed: String,
    pub block: TranscriptBlock,
}

/// projector 与 terminal 之间的提交泵。
///
/// - 只接收 `Sealed` block；
/// - 队列有界，避免异常事件率把内存拉爆；
/// - `drain_emittable` 通过 ledger 做最终幂等裁决；
/// - 返回的条目已经被标记为 `Committed`，调用方只负责渲染和 `insert_before`。
#[derive(Debug, Clone)]
pub struct TranscriptCommitPump {
    ledger: TranscriptCommitLedger,
    pending: VecDeque<PendingCommit>,
    cap: usize,
    dropped: u64,
}

impl Default for TranscriptCommitPump {
    fn default() -> Self {
        Self::new(DEFAULT_PENDING_CAP)
    }
}

impl TranscriptCommitPump {
    pub fn new(cap: usize) -> Self {
        Self {
            ledger: TranscriptCommitLedger::new(),
            pending: VecDeque::new(),
            cap: cap.max(1),
            dropped: 0,
        }
    }

    pub fn enqueue(
        &mut self,
        seed: &str,
        blocks: impl IntoIterator<Item = TranscriptBlock>,
    ) -> usize {
        let mut enqueued = 0;
        for block in blocks {
            if block.state != BlockState::Sealed {
                continue;
            }
            if self.pending.len() == self.cap {
                self.pending.pop_front();
                self.dropped = self.dropped.saturating_add(1);
            }
            self.pending.push_back(PendingCommit {
                seed: seed.to_string(),
                block,
            });
            enqueued += 1;
        }
        enqueued
    }

    /// 完整 replay 专用：不做容量裁剪。
    ///
    /// 会话切换/resize 必须按 ledger 顺序重放全部已提交内容；若复用有界
    /// `enqueue`，长会话会从头部静默丢掉超过 cap 的块。
    pub fn enqueue_replay(
        &mut self,
        seed: &str,
        blocks: impl IntoIterator<Item = TranscriptBlock>,
    ) -> usize {
        let mut enqueued = 0;
        for block in blocks {
            if block.state != BlockState::Sealed {
                continue;
            }
            self.pending.push_back(PendingCommit {
                seed: seed.to_string(),
                block,
            });
            enqueued += 1;
        }
        enqueued
    }

    pub fn drain_emittable(&mut self) -> Vec<PendingCommit> {
        let mut emitted = Vec::new();
        while let Some(mut pending) = self.pending.pop_front() {
            if self.ledger.commit_block(&pending.seed, &mut pending.block)
                == Some(CommitDecision::Emit)
            {
                emitted.push(pending);
            }
        }
        // 完整 replay 会把队列扩到上万条；drain 后空队列仍持有旧容量。
        // 小增量队列不 shrink，避免每帧重新分配。
        if self.pending.is_empty() && self.pending.capacity() > 256 {
            self.pending.shrink_to_fit();
        }
        emitted
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn ledger(&self) -> &TranscriptCommitLedger {
        &self.ledger
    }

    /// 丢弃某 seed 的待提交项与提交记录，准备清屏后的完整重放。
    pub fn reset_seed(&mut self, seed: &str) {
        self.pending.retain(|pending| pending.seed != seed);
        self.ledger.forget_seed(seed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::v2::transcript::{BlockKind, TranscriptBlock};

    fn sealed(text: &str) -> TranscriptBlock {
        sealed_with_id("block-1", text)
    }

    fn sealed_with_id(id: &str, text: &str) -> TranscriptBlock {
        let mut block = TranscriptBlock::new(
            id,
            BlockKind::Assistant {
                text: text.to_string(),
            },
        )
        .with_turn_id("turn-1");
        assert!(block.seal());
        block
    }

    #[test]
    fn emits_once_and_marks_block_committed() {
        let mut ledger = TranscriptCommitLedger::new();
        let mut block = sealed("hello");
        assert_eq!(
            ledger.commit_block("seed", &mut block),
            Some(CommitDecision::Emit)
        );
        assert_eq!(block.state, BlockState::Committed);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn replay_is_duplicate_and_does_not_emit_again() {
        let mut ledger = TranscriptCommitLedger::new();
        let mut first = sealed("hello");
        assert_eq!(
            ledger.commit_block("seed", &mut first),
            Some(CommitDecision::Emit)
        );
        let mut replay = sealed("hello");
        assert_eq!(
            ledger.commit_block("seed", &mut replay),
            Some(CommitDecision::Duplicate)
        );
        assert_eq!(replay.state, BlockState::Sealed);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn same_identity_with_changed_content_is_conflict() {
        let mut ledger = TranscriptCommitLedger::new();
        let mut first = sealed("hello");
        assert_eq!(
            ledger.commit_block("seed", &mut first),
            Some(CommitDecision::Emit)
        );
        let mut changed = sealed("changed");
        assert_eq!(
            ledger.commit_block("seed", &mut changed),
            Some(CommitDecision::Conflict)
        );
        assert_eq!(changed.state, BlockState::Sealed);
    }

    #[test]
    fn live_block_is_not_committable() {
        let mut ledger = TranscriptCommitLedger::new();
        let mut block = TranscriptBlock::new(
            "block-1",
            BlockKind::Assistant {
                text: "live".to_string(),
            },
        )
        .with_turn_id("turn-1");
        assert_eq!(ledger.commit_block("seed", &mut block), None);
        assert!(ledger.is_empty());
    }

    #[test]
    fn pump_emits_in_order_and_deduplicates_replay() {
        let mut pump = TranscriptCommitPump::new(8);
        assert_eq!(
            pump.enqueue("seed", [sealed_with_id("a", "a"), sealed_with_id("b", "b")]),
            2
        );
        let emitted = pump.drain_emittable();
        assert_eq!(emitted.len(), 2);
        assert!(matches!(
            emitted[0].block.kind,
            BlockKind::Assistant { ref text } if text == "a"
        ));
        assert!(matches!(
            emitted[1].block.kind,
            BlockKind::Assistant { ref text } if text == "b"
        ));

        assert_eq!(pump.enqueue("seed", [sealed_with_id("a", "a")]), 1);
        assert!(pump.drain_emittable().is_empty());
    }

    #[test]
    fn pump_ignores_live_blocks_and_bounds_queue() {
        let mut pump = TranscriptCommitPump::new(1);
        let live = TranscriptBlock::new(
            "live",
            BlockKind::Assistant {
                text: "live".to_string(),
            },
        )
        .with_turn_id("turn-1");
        assert_eq!(pump.enqueue("seed", [live]), 0);
        assert!(pump.is_empty());

        assert_eq!(pump.enqueue("seed", [sealed_with_id("first", "first")]), 1);
        assert_eq!(
            pump.enqueue("seed", [sealed_with_id("second", "second")]),
            1
        );
        assert_eq!(pump.len(), 1);
        assert_eq!(pump.dropped(), 1);
    }

    #[test]
    fn reset_seed_allows_replay_after_scrollback_purge() {
        let mut pump = TranscriptCommitPump::new(8);
        let blocks = [sealed_with_id("a", "a"), sealed_with_id("b", "b")];
        pump.enqueue("seed", blocks.clone());
        assert_eq!(pump.drain_emittable().len(), 2);

        pump.reset_seed("seed");
        pump.enqueue("seed", blocks);
        assert_eq!(pump.drain_emittable().len(), 2);
    }

    #[test]
    fn reset_seed_keeps_other_seed_ledger() {
        let mut pump = TranscriptCommitPump::new(8);
        pump.enqueue("a", [sealed_with_id("a", "a")]);
        pump.enqueue("b", [sealed_with_id("b", "b")]);
        assert_eq!(pump.drain_emittable().len(), 2);

        pump.reset_seed("a");

        pump.enqueue("b", [sealed_with_id("b", "b")]);
        assert!(pump.drain_emittable().is_empty());
        pump.enqueue("a", [sealed_with_id("a", "a")]);
        assert_eq!(pump.drain_emittable().len(), 1);
    }

    #[test]
    fn replay_enqueue_does_not_drop_past_capacity() {
        let mut pump = TranscriptCommitPump::new(2);
        let blocks: Vec<_> = (0..10)
            .map(|index| sealed_with_id(&format!("b{index}"), &format!("block {index}")))
            .collect();

        assert_eq!(pump.enqueue_replay("seed", blocks), 10);
        assert_eq!(pump.drain_emittable().len(), 10);
        assert_eq!(pump.dropped(), 0);
    }
}
