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

/// 进程内提交账本。
#[derive(Debug, Clone, Default)]
pub struct CommitLedger {
    /// seed → (128-bit identity fingerprint → content hash)。
    ///
    /// 不保存完整 CommitId，避免每个 block 重复持有 seed/turn/block 字符串。
    /// 指纹只用于进程内冲突检测，不用于安全用途。
    seeds: HashMap<String, HashMap<u128, u64>>,
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
        let fingerprint = identity_fingerprint(&id);
        let records = self.seeds.entry(id.seed).or_default();
        match records.get(&fingerprint) {
            Some(existing) if *existing == content_hash => CommitDecision::Duplicate,
            Some(_) => CommitDecision::Conflict,
            None => {
                records.insert(fingerprint, content_hash);
                CommitDecision::Emit
            }
        }
    }

    pub fn contains(&self, id: &CommitId) -> bool {
        self.seeds
            .get(&id.seed)
            .is_some_and(|records| records.contains_key(&identity_fingerprint(id)))
    }

    pub fn len(&self) -> usize {
        self.seeds.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.seeds.is_empty()
    }

    /// 忘掉某个 seed 的全部提交记录。
    ///
    /// 仅用于**已经清空终端 scrollback** 后的会话重放：此时保留旧 ledger 会让
    /// 重放全部被判为 `Duplicate`，与“清屏后按 ledger 顺序重放”冲突。
    pub fn forget_seed(&mut self, seed: &str) {
        self.seeds.remove(seed);
    }
}

/// 两个独立 FNV-1a 变体组成 128-bit 身份指纹。
///
/// 指纹覆盖 seed/turn/block/revision，并使用 `0` 分隔各字段，避免拼接歧义。
/// 这是进程内去重键，不是安全哈希；碰撞概率按 2^-128 量级处理。
fn identity_fingerprint(id: &CommitId) -> u128 {
    const OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
    const OFFSET_B: u64 = 0x8422_2325_cbf2_9ce4;
    const PRIME_A: u64 = 0x0000_0100_0000_01b3;
    const PRIME_B: u64 = 0x9e37_79b1_85eb_ca87;

    fn update(mut hash: u64, prime: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(prime);
        }
        hash
    }

    let mut a = OFFSET_A;
    let mut b = OFFSET_B;
    for part in [
        id.seed.as_bytes(),
        b"\0",
        id.turn_id.as_bytes(),
        b"\0",
        id.block_id.as_bytes(),
        b"\0",
    ] {
        a = update(a, PRIME_A, part);
        b = update(b, PRIME_B, part);
    }
    let revision = id.revision.to_le_bytes();
    a = update(a, PRIME_A, &revision);
    b = update(b, PRIME_B, &revision);
    (u128::from(a) << 64) | u128::from(b)
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
    fn replay_does_not_duplicate() {
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
        assert!(ledger.contains(&a));
        assert!(ledger.contains(&b));
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

    #[test]
    fn forgetting_a_seed_keeps_other_seed_records() {
        let mut ledger = CommitLedger::new();
        let a = CommitId::new("a", "turn", "block", 1);
        let b = CommitId::new("b", "turn", "block", 1);
        assert_eq!(
            ledger.commit(a.clone(), content_hash("a")),
            CommitDecision::Emit
        );
        assert_eq!(
            ledger.commit(b.clone(), content_hash("b")),
            CommitDecision::Emit
        );

        ledger.forget_seed("a");

        assert!(!ledger.contains(&a));
        assert!(ledger.contains(&b));
    }
}
