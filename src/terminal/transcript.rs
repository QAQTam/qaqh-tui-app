//! V2 transcript commit ledger 适配。
//!
//! `CommitLedger` 只认识 `CommitId + content_hash`；本层把 V2 block 的
//! `turn_id / block_id / revision / content_fingerprint` 接上，并在 Emit
//! 成功时推进 block 到 `Committed`。

#![allow(dead_code)] // M3.2 inline 接线前先冻结 API。

use crate::terminal::commit::{CommitDecision, CommitId, CommitLedger, content_hash};
use crate::ui::v2::transcript::{BlockState, TranscriptBlock};

#[derive(Debug, Default)]
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

    pub fn order(&self) -> &[CommitId] {
        self.ledger.order()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::v2::transcript::{BlockKind, TranscriptBlock};

    fn sealed(text: &str) -> TranscriptBlock {
        let mut block = TranscriptBlock::new(
            "block-1",
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
}
