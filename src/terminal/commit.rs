//! V2 终端提交账本（M1）。
//!
//! 目标：已封口块在重连、re-baseline、resize 重放等路径上**最多提交一次**。
//! 账本是进程内状态；进程退出后终端 scrollback 已保留历史，不要求跨进程重放。

use std::collections::HashMap;

/// 已提交块的稳定身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitId {
    pub seed: String,
    pub turn_id: String,
    pub block_id: String,
    pub revision: u64,
}

impl CommitId {
    pub fn new(
        seed: impl Into<String>,
        turn_id: impl Into<String>,
        block_id: impl Into<String>,
        revision: u64,
    ) -> Self {
        Self {
            seed: seed.into(),
            turn_id: turn_id.into(),
            block_id: block_id.into(),
            revision,
        }
    }
}

/// 一次提交尝试的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    /// 新内容，允许 emit。
    Emit,
    /// 同 id 同内容，跳过 emit。
    Duplicate,
    /// 同 id 不同内容，拒绝 emit 并记录冲突。
    Conflict,
}

#[derive(Debug, Clone)]
struct CommitRecord {
    content_hash: u64,
}

/// 进程内提交账本。
#[derive(Debug, Clone, Default)]
pub struct CommitLedger {
    records: HashMap<CommitId, CommitRecord>,
    order: Vec<CommitId>,
}

#[allow(dead_code)] // M1 scaffold: accessors are consumed by M2/M3 and unit tests.
impl CommitLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试登记一次提交。
    ///
    /// - 首次出现：登记并返回 [`CommitDecision::Emit`]；
    /// - 同 id 同 hash：返回 [`CommitDecision::Duplicate`]；
    /// - 同 id 不同 hash：返回 [`CommitDecision::Conflict`]，不覆盖原记录。
    pub fn commit(&mut self, id: CommitId, content_hash: u64) -> CommitDecision {
        match self.records.get(&id) {
            Some(record) if record.content_hash == content_hash => CommitDecision::Duplicate,
            Some(_) => CommitDecision::Conflict,
            None => {
                self.records
                    .insert(id.clone(), CommitRecord { content_hash });
                self.order.push(id);
                CommitDecision::Emit
            }
        }
    }

    pub fn contains(&self, id: &CommitId) -> bool {
        self.records.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 已提交顺序，用于 resize / 会话切换时的重放。
    pub fn order(&self) -> &[CommitId] {
        &self.order
    }
}

/// 确定性 FNV-1a：仅用于进程内冲突检测，不用于安全哈希。
pub fn content_hash(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for byte in text.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(block: &str, revision: u64) -> CommitId {
        CommitId::new("seed", "turn-1", block, revision)
    }

    #[test]
    fn first_commit_emits_duplicate_skips_conflict_rejects() {
        let mut ledger = CommitLedger::new();
        let id = id("b1", 1);
        let hash = content_hash("hello");

        assert_eq!(ledger.commit(id.clone(), hash), CommitDecision::Emit);
        assert_eq!(ledger.commit(id.clone(), hash), CommitDecision::Duplicate);
        assert_eq!(
            ledger.commit(id.clone(), content_hash("changed")),
            CommitDecision::Conflict
        );
        assert_eq!(ledger.len(), 1);
        assert!(!ledger.is_empty());
        assert!(ledger.contains(&id));
    }

    #[test]
    fn order_is_stable_and_replay_does_not_duplicate() {
        let mut ledger = CommitLedger::new();
        let a = id("a", 1);
        let b = id("b", 1);

        assert_eq!(
            ledger.commit(a.clone(), content_hash("a")),
            CommitDecision::Emit
        );
        assert_eq!(
            ledger.commit(b.clone(), content_hash("b")),
            CommitDecision::Emit
        );
        assert_eq!(
            ledger.commit(a.clone(), content_hash("a")),
            CommitDecision::Duplicate
        );

        assert_eq!(ledger.order(), &[a, b]);
    }

    #[test]
    fn revision_is_part_of_identity() {
        let mut ledger = CommitLedger::new();
        assert_eq!(
            ledger.commit(id("b1", 1), content_hash("v1")),
            CommitDecision::Emit
        );
        assert_eq!(
            ledger.commit(id("b1", 2), content_hash("v2")),
            CommitDecision::Emit
        );
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn content_hash_changes_with_content() {
        assert_ne!(content_hash("hello"), content_hash("hello!"));
        assert_eq!(content_hash("hello"), content_hash("hello"));
    }
}
