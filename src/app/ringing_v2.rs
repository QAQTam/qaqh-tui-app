//! Ringing v2 会话状态机（TUI 侧不变量）。
//!
//! **本模块不再持有任何协议类型**：delivery / reset reason / interaction kind /
//! pending interaction / driver state / bootstrap / 事件信封一律 `use qaqh_client::*`
//! ——wire 视图只有那一份权威实现。这里只剩 TUI 自己的会话级时序规则：
//! epoch / log / cursor 单调性、重复与过期事件丢弃、交互身份「第一答复胜出」、
//! driver seat 按 epoch 单调交接。
//!
//! 历史（2026-09-30 之前）本模块自带六份手抄协议镜像（`Delivery` /
//! `InteractionKind` / `ResetReason` / `PendingInteraction` / `DriverState` /
//! `BootstrapSnapshot`）加三个适配器（`event_meta_from_client` /
//! `reset_from_client` / `bootstrap_from_client`）。漂移**已经真实发生，而且
//! 失败模式是静默的**：
//!
//! - 后端 bootstrap 把 `session_id` 改名 `session_id`（`refactor(identity)`），镜像与
//!   手抄 fixture 仍按旧键读；
//! - 后端信封新增 `ts_ms`（beta-readiness W1/C3），手抄的结构体字面量直接编译
//!   失败；
//! - 镜像里的 `PendingInteraction` 漏了权威类型的 `request` 字段（modal 正文），
//!   零人发现——因为它只有 `#[cfg(test)]` 的读者。
//!
//! 与 `src/protocol/mod.rs` 同一条纪律：宁可倒贴一次迁移成本，也不留第二份协议。
//! 删除纪律（同 T-13）：别指望 `dead_code` 指出残留，逐项 grep 真实调用点。

use std::collections::BTreeSet;

use qaqh_client::{
    ClientV2Bootstrap, ClientV2Delivery, ClientV2DriverState, ClientV2Event, ClientV2Reset,
    ClientV2ResetReason,
};

/// reset 原因是否让会话进入只读。
///
/// **这是 TUI 的展示策略**（没有可写基线就不给写入口），不是 wire 语义；策略
/// 必须挂在权威枚举上。这里刻意写成**穷尽 match 而不是 `matches!`**：后端将来
/// 新增 reset reason 时本函数编译失败，逼一次显式决策，而不是默默归入「可写」。
pub fn reset_is_read_only(reason: ClientV2ResetReason) -> bool {
    match reason {
        ClientV2ResetReason::SnapshotMissing | ClientV2ResetReason::UpgradeRequired => true,
        ClientV2ResetReason::CursorExpired
        | ClientV2ResetReason::LogIdMismatch
        | ClientV2ResetReason::UnknownFact
        | ClientV2ResetReason::ReplayOverflow
        | ClientV2ResetReason::EpochMismatch
        | ClientV2ResetReason::CrossSession
        | ClientV2ResetReason::SnapshotExpired
        | ClientV2ResetReason::SnapshotHashMismatch
        | ClientV2ResetReason::StaleWriter
        | ClientV2ResetReason::ContentQuotaExceeded
        | ClientV2ResetReason::PerConnectionOverflow
        | ClientV2ResetReason::ProgressBufferOverflow
        | ClientV2ResetReason::ActorMailboxOverflow => false,
    }
}

/// 一次 `apply_event` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    ReliableApplied {
        fact_seq: u64,
        projection_index: u16,
    },
    ReplaceableApplied {
        revision: u64,
    },
    Ephemeral,
    Duplicate,
    Stale,
    EpochMismatch,
    LogMismatch,
    NotInitialized,
    ResetPending,
    /// 事件缺了权威信封为该 delivery 声明的必填字段。
    ///
    /// 正常情况下 `qaqh-client` 已在 SSE 解码处调用 `ClientV2Event::validate()`
    /// 拦下；这里只是本状态机**读不到值就没法推进**的自然分支，不是第二套校验。
    Incomplete(&'static str),
}

/// 一次 `apply_bootstrap` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    Applied,
    SessionMismatch,
    Invalid,
    ResetMismatch,
    Stale,
}

/// 交互身份的生命周期结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionOutcome {
    Requested,
    Resolved,
    Expired,
    Duplicate,
    AlreadyTerminal,
    Unknown,
}

/// driver seat 交接结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverOutcome {
    Applied,
    Duplicate,
    Stale,
    Conflict,
}

/// 单会话的 Ringing v2 时序状态。
///
/// **只存 TUI 需要的跨事件不变量**：领域载荷（交互正文、工具卡、transcript）一律
/// 由权威 typed payload 直接进入 UI 层，不在这里落副本——副本必然漏字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingingV2SessionModel {
    session_id: String,
    server_epoch: Option<String>,
    log_id: Option<String>,
    cursor: Option<String>,
    last_fact_seq: u64,
    last_projection_index: u16,
    state_revision: u64,
    /// 未决交互**只记身份**：正文与 kind 属权威 payload，UI 层直接消费
    /// `ClientV2PendingInteraction`，这里再存一份就会漏字段（历史缺陷）。
    pending_interactions: BTreeSet<String>,
    terminal_interactions: BTreeSet<String>,
    driver: Option<ClientV2DriverState>,
    /// 待处理 reset 的**权威原帧**：保留它才能在新 bootstrap 到达时逐字段校验。
    reset: Option<ClientV2Reset>,
}

impl RingingV2SessionModel {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            server_epoch: None,
            log_id: None,
            cursor: None,
            last_fact_seq: 0,
            last_projection_index: 0,
            state_revision: 0,
            pending_interactions: BTreeSet::new(),
            terminal_interactions: BTreeSet::new(),
            driver: None,
            reset: None,
        }
    }

    pub fn server_epoch(&self) -> Option<&str> {
        self.server_epoch.as_deref()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    #[cfg(test)]
    pub fn state_revision(&self) -> u64 {
        self.state_revision
    }

    #[cfg(test)]
    pub fn is_reset_pending(&self) -> bool {
        self.reset.is_some()
    }

    pub fn is_read_only(&self) -> bool {
        self.reset
            .as_ref()
            .is_some_and(|reset| reset_is_read_only(reset.reason))
    }

    pub fn driver(&self) -> Option<&ClientV2DriverState> {
        self.driver.as_ref()
    }

    pub fn is_driver(&self, client_session_id: &str) -> bool {
        self.driver
            .as_ref()
            .and_then(|driver| driver.holder.as_deref())
            == Some(client_session_id)
    }

    #[cfg(test)]
    pub fn pending_interactions(&self) -> impl Iterator<Item = &str> {
        self.pending_interactions.iter().map(String::as_str)
    }

    /// 用权威 typed bootstrap 原子替换旧状态。
    ///
    /// 快照 cursor 是**不透明 token**：这里只解码出 log/fact 基线用于 reset 校验，
    /// 不解码、不推测 replay 位置——replay 由服务端保证严格大于该基线。
    pub fn apply_bootstrap(&mut self, bootstrap: &ClientV2Bootstrap) -> BootstrapOutcome {
        if bootstrap.session_id != self.session_id {
            return BootstrapOutcome::SessionMismatch;
        }
        if bootstrap.server_epoch.trim().is_empty() {
            return BootstrapOutcome::Invalid;
        }
        let Ok(cursor) = bootstrap.snapshot_cursor.decode_snapshot() else {
            return BootstrapOutcome::Invalid;
        };
        let log_id = cursor.log_id;
        let snapshot_fact_seq = cursor.fact_seq;
        let state_revision = bootstrap
            .control
            .state_revision
            .max(bootstrap.conversation.state_revision)
            .max(bootstrap.tool.state_revision);

        // reset 之后的 bootstrap 必须仍指向 reset 宣告的 epoch/log/基线。这样即使
        // 旧请求的响应比新请求晚到，也不能清掉 reset 或覆盖新快照。
        if let Some(reset) = &self.reset {
            if bootstrap.server_epoch != reset.server_epoch {
                return BootstrapOutcome::ResetMismatch;
            }
            if let Some(reset_log_id) = reset.log_id.as_deref()
                && log_id != reset_log_id
            {
                return BootstrapOutcome::ResetMismatch;
            }
            if let Some(reset_fact_seq) = reset
                .snapshot_cursor
                .as_ref()
                .and_then(|token| token.decode_snapshot().ok())
                .map(|cursor| cursor.fact_seq)
                && snapshot_fact_seq < reset_fact_seq
            {
                return BootstrapOutcome::ResetMismatch;
            }
        } else if self.server_epoch.as_deref() == Some(bootstrap.server_epoch.as_str())
            && state_revision < self.state_revision
        {
            // 同一 epoch 内 revision 单调；较旧的并发 bootstrap 响应不得把状态
            // 回滚到更早的 cursor / pending / driver 快照。
            return BootstrapOutcome::Stale;
        }

        self.server_epoch = Some(bootstrap.server_epoch.clone());
        self.log_id = Some(log_id);
        self.cursor = Some(bootstrap.snapshot_cursor.as_str().to_string());
        self.last_fact_seq = 0;
        self.last_projection_index = 0;
        self.state_revision = state_revision;
        // 权威 bootstrap 的 `interactions` 已是 daemon 过滤后的未决集合。
        self.pending_interactions = bootstrap
            .control
            .state
            .interactions
            .iter()
            .map(|interaction| interaction.interaction_id.clone())
            .collect();
        self.terminal_interactions.clear();
        self.driver = bootstrap.control.state.driver.clone();
        self.reset = None;
        BootstrapOutcome::Applied
    }

    /// 标记 reset；保留旧 UI 状态，直到新 bootstrap 通过校验。
    pub fn begin_reset(&mut self, reset: &ClientV2Reset) -> bool {
        if reset.session_id != self.session_id {
            return false;
        }
        self.reset = Some(reset.clone());
        true
    }

    /// 消费一条权威 v2 投影事件。
    pub fn apply_event(&mut self, event: &ClientV2Event) -> ApplyOutcome {
        if self.reset.is_some() {
            return ApplyOutcome::ResetPending;
        }
        let Some(current_epoch) = self.server_epoch.as_deref() else {
            return ApplyOutcome::NotInitialized;
        };
        if event.server_epoch != current_epoch {
            return ApplyOutcome::EpochMismatch;
        }

        match event.delivery {
            ClientV2Delivery::Reliable => self.apply_reliable(event),
            ClientV2Delivery::Replaceable => self.apply_replaceable(event),
            ClientV2Delivery::Ephemeral => ApplyOutcome::Ephemeral,
        }
    }

    /// reliable 事件推进 canonical cursor；`(fact_seq, projection_index)` 全局字典序。
    fn apply_reliable(&mut self, event: &ClientV2Event) -> ApplyOutcome {
        let Some(log_id) = event.log_id.as_deref() else {
            return ApplyOutcome::Incomplete("reliable requires log_id");
        };
        let Some(fact_seq) = event.fact_seq else {
            return ApplyOutcome::Incomplete("reliable requires fact_seq");
        };
        let Some(projection_index) = event.projection_index else {
            return ApplyOutcome::Incomplete("reliable requires projection_index");
        };
        let Some(cursor) = event.cursor.as_ref() else {
            return ApplyOutcome::Incomplete("reliable requires cursor");
        };
        let Some(revision) = event.revision else {
            return ApplyOutcome::Incomplete("reliable requires revision");
        };

        if let Some(current_log) = self.log_id.as_deref() {
            if current_log != log_id {
                return ApplyOutcome::LogMismatch;
            }
        } else {
            self.log_id = Some(log_id.to_string());
        }

        let incoming = (fact_seq, projection_index);
        let current = (self.last_fact_seq, self.last_projection_index);
        if incoming == current {
            return ApplyOutcome::Duplicate;
        }
        if incoming < current {
            return ApplyOutcome::Stale;
        }

        self.cursor = Some(cursor.as_str().to_string());
        self.last_fact_seq = fact_seq;
        self.last_projection_index = projection_index;
        self.state_revision = self.state_revision.max(revision);
        ApplyOutcome::ReliableApplied {
            fact_seq,
            projection_index,
        }
    }

    /// replaceable 事件只推 revision，不碰 canonical cursor。
    fn apply_replaceable(&mut self, event: &ClientV2Event) -> ApplyOutcome {
        let Some(revision) = event.revision else {
            return ApplyOutcome::Incomplete("replaceable requires revision");
        };
        if revision < self.state_revision {
            return ApplyOutcome::Stale;
        }
        if revision == self.state_revision {
            return ApplyOutcome::Duplicate;
        }
        self.state_revision = revision;
        ApplyOutcome::ReplaceableApplied { revision }
    }

    pub fn apply_interaction_requested(&mut self, interaction_id: &str) -> InteractionOutcome {
        if self.terminal_interactions.contains(interaction_id) {
            return InteractionOutcome::AlreadyTerminal;
        }
        if !self.pending_interactions.insert(interaction_id.to_string()) {
            return InteractionOutcome::Duplicate;
        }
        InteractionOutcome::Requested
    }

    pub fn apply_interaction_resolved(&mut self, interaction_id: &str) -> InteractionOutcome {
        self.close_interaction(interaction_id, InteractionOutcome::Resolved)
    }

    pub fn apply_interaction_expired(&mut self, interaction_id: &str) -> InteractionOutcome {
        self.close_interaction(interaction_id, InteractionOutcome::Expired)
    }

    fn close_interaction(
        &mut self,
        interaction_id: &str,
        closed: InteractionOutcome,
    ) -> InteractionOutcome {
        if self.terminal_interactions.contains(interaction_id) {
            return InteractionOutcome::AlreadyTerminal;
        }
        let removed = self.pending_interactions.remove(interaction_id);
        self.terminal_interactions
            .insert(interaction_id.to_string());
        if removed {
            closed
        } else {
            InteractionOutcome::Unknown
        }
    }

    pub fn apply_driver_state(&mut self, driver: ClientV2DriverState) -> DriverOutcome {
        if let Some(current) = &self.driver {
            if driver.driver_epoch < current.driver_epoch {
                return DriverOutcome::Stale;
            }
            if driver.driver_epoch == current.driver_epoch
                && driver.holder == current.holder
                && driver.can_claim == current.can_claim
            {
                return DriverOutcome::Duplicate;
            }
            if driver.driver_epoch == current.driver_epoch {
                return DriverOutcome::Conflict;
            }
        }
        self.driver = Some(driver);
        DriverOutcome::Applied
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 「权威元数据」访问器
//
// 事件信封的跨事件元数据（epoch / log / cursor / fact_seq / projection_index /
// revision / delivery / ts_ms）**就是** `ClientV2Event` 的字段。历史上有过一层
// `EventMeta` 镜像 + `event_meta_from_client` 适配器；它唯一的效果是让后端新增
// 字段（`ts_ms`）时 TUI 静默拿不到。现在消费方直接用 `&ClientV2Event`。
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::v2_fixtures as fx;
    use qaqh_client::{
        Channel, ClientV2DeltaInteractionKind, ClientV2InteractionKind, ClientV2ResetReason,
    };

    const SESSION: &str = "session-1";
    const EPOCH: &str = "epoch-1";
    const LOG: &str = "log-1";

    fn audit_payload() -> qaqh_client::ClientV2Payload {
        fx::audit_payload()
    }

    /// snapshot 基线：`CanonicalCursor::snapshot` 要求 `fact_seq >= 1`。
    fn baseline() -> ClientV2Bootstrap {
        fx::bootstrap(SESSION, EPOCH, LOG, 1, 7)
    }

    /// 权威 snapshot cursor 的字符串形态（用例断言用；不再手写假 cursor）。
    fn baseline_cursor() -> String {
        baseline().snapshot_cursor.as_str().to_string()
    }

    fn model() -> RingingV2SessionModel {
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(
            model.apply_bootstrap(&baseline()),
            BootstrapOutcome::Applied
        );
        model
    }

    fn reliable(fact_seq: u64, projection_index: u16) -> ClientV2Event {
        fx::ReliableEvent::new(
            SESSION,
            EPOCH,
            LOG,
            fact_seq,
            projection_index,
            audit_payload(),
        )
        .build()
    }

    /// 权威 bootstrap 被完整吃下：cursor / revision / 未决身份 / driver。
    #[test]
    fn bootstrap_adopts_authoritative_snapshot() {
        let bootstrap = fx::bootstrap_typed(
            SESSION,
            EPOCH,
            LOG,
            42,
            19,
            &[fx::pending("i1", "c1", "t1", "permission")],
            Some(fx::driver("cs-1", 4, false)),
        );
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(model.apply_bootstrap(&bootstrap), BootstrapOutcome::Applied);
        assert_eq!(model.server_epoch(), Some(EPOCH));
        assert_eq!(model.cursor(), Some(bootstrap.snapshot_cursor.as_str()));
        // 三频道 revision 取最大者（control=conversation=tool=19）。
        assert_eq!(model.state_revision(), 19);
        assert_eq!(model.pending_interactions().collect::<Vec<_>>(), ["i1"]);
        assert_eq!(model.driver().expect("driver").driver_epoch, 4);
        assert!(model.is_driver("cs-1"));
    }

    /// 反闸：TUI 不再自建协议枚举——`kind` 直接是权威类型，新增变体编译期可见。
    #[test]
    fn interaction_kind_is_the_authoritative_enum() {
        let kinds = [
            ClientV2InteractionKind::Permission,
            ClientV2InteractionKind::Ask,
            ClientV2InteractionKind::PlanReview,
        ];
        assert_eq!(kinds.len(), 3);
        assert_eq!(
            fx::interaction_kind_wire(ClientV2InteractionKind::PlanReview),
            "plan",
            "wire 值由权威 serde 承担，TUI 不再自己映射"
        );
    }

    #[test]
    fn c1_snapshot_then_subscribe_is_gap_free_and_duplicate_free() {
        let mut model = model();
        assert_eq!(model.cursor(), Some(baseline_cursor().as_str()));

        let first = reliable(8, 1);
        assert_eq!(
            model.apply_event(&first),
            ApplyOutcome::ReliableApplied {
                fact_seq: 8,
                projection_index: 1
            }
        );
        // cursor 是**权威 token 本身**，不是 TUI 拼出来的字符串。
        assert_eq!(
            model.cursor(),
            first.cursor.as_ref().map(|cursor| cursor.as_str())
        );
        assert_eq!(model.apply_event(&first), ApplyOutcome::Duplicate);
    }

    #[test]
    fn c2_reliable_reconnect_advances_in_global_lexicographic_order() {
        let mut model = model();
        for (fact_seq, projection_index) in [(8, 0), (8, 1), (9, 0)] {
            assert_eq!(
                model.apply_event(&reliable(fact_seq, projection_index)),
                ApplyOutcome::ReliableApplied {
                    fact_seq,
                    projection_index
                }
            );
        }
        assert_eq!(model.apply_event(&reliable(8, 99)), ApplyOutcome::Stale);
    }

    #[test]
    fn c3_replaceable_reconnect_is_latest_current_without_cursor_advance() {
        let mut model = model();
        let cursor = model.cursor().map(str::to_string);
        let event = fx::replaceable_event(SESSION, EPOCH, 8, audit_payload());
        assert_eq!(
            model.apply_event(&event),
            ApplyOutcome::ReplaceableApplied { revision: 8 }
        );
        assert_eq!(model.cursor(), cursor.as_deref());
        assert_eq!(model.state_revision(), 8);
        assert_eq!(model.apply_event(&event), ApplyOutcome::Duplicate);
    }

    #[test]
    fn c4_ephemeral_never_persists_or_replays() {
        let mut model = model();
        let before = model.clone();
        let event = fx::ephemeral_event(SESSION, EPOCH, audit_payload());
        assert_eq!(model.apply_event(&event), ApplyOutcome::Ephemeral);
        assert_eq!(model, before);
    }

    #[test]
    fn c5_epoch_and_log_mismatch_are_rejected() {
        let mut model = model();
        let wrong_epoch =
            fx::ReliableEvent::new(SESSION, "epoch-2", LOG, 8, 1, audit_payload()).build();
        assert_eq!(model.apply_event(&wrong_epoch), ApplyOutcome::EpochMismatch);

        let wrong_log =
            fx::ReliableEvent::new(SESSION, EPOCH, "log-2", 8, 1, audit_payload()).build();
        assert_eq!(model.apply_event(&wrong_log), ApplyOutcome::LogMismatch);
    }

    #[test]
    fn c5_log_id_reset_keeps_state_until_rebaseline() {
        let mut model = model();
        let reset = fx::reset(
            SESSION,
            EPOCH,
            Some("log-2"),
            Some(("log-2", 8)),
            ClientV2ResetReason::LogIdMismatch,
        );
        assert!(model.begin_reset(&reset));
        assert!(model.is_reset_pending());
        assert!(!model.is_read_only());
        assert_eq!(
            model.apply_event(&reliable(8, 1)),
            ApplyOutcome::ResetPending
        );
    }

    #[test]
    fn c6_cursor_expired_keeps_old_state_until_rebaseline() {
        let mut model = model();
        let old_cursor = model.cursor().map(str::to_string);
        let reset = fx::reset(
            SESSION,
            EPOCH,
            Some(LOG),
            Some((LOG, 8)),
            ClientV2ResetReason::CursorExpired,
        );
        assert!(model.begin_reset(&reset));
        assert!(model.is_reset_pending());
        assert_eq!(model.cursor(), old_cursor.as_deref());
        assert_eq!(
            model.apply_event(&reliable(8, 1)),
            ApplyOutcome::ResetPending
        );

        let next = fx::bootstrap(SESSION, EPOCH, LOG, 8, 9);
        assert_eq!(model.apply_bootstrap(&next), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
        assert_eq!(model.cursor(), Some(next.snapshot_cursor.as_str()));
        assert_eq!(model.state_revision(), 9);
    }

    #[test]
    fn c7_snapshot_missing_is_read_only_and_does_not_guess_history() {
        let mut model = model();
        let old_cursor = model.cursor().map(str::to_string);
        let old_revision = model.state_revision();
        let reset = fx::reset(
            SESSION,
            EPOCH,
            None,
            None,
            ClientV2ResetReason::SnapshotMissing,
        );

        assert!(model.begin_reset(&reset));
        assert!(model.is_reset_pending());
        assert!(model.is_read_only());
        assert_eq!(model.cursor(), old_cursor.as_deref());
        assert_eq!(model.state_revision(), old_revision);
        assert_eq!(
            model.apply_event(&reliable(8, 1)),
            ApplyOutcome::ResetPending
        );
    }

    /// reset 只读策略的穷尽映射：每个后端原因都必须有明确归属。
    ///
    /// 证伪方式：往 `ClientV2ResetReason` 加一个变体而不改 `reset_is_read_only`
    /// —— 本仓**编译失败**（穷尽 match），而不是默默当成「可写」。
    #[test]
    fn reset_read_only_policy_covers_every_authoritative_reason() {
        for reason in [
            ClientV2ResetReason::CursorExpired,
            ClientV2ResetReason::LogIdMismatch,
            ClientV2ResetReason::UnknownFact,
            ClientV2ResetReason::UpgradeRequired,
            ClientV2ResetReason::ReplayOverflow,
            ClientV2ResetReason::EpochMismatch,
            ClientV2ResetReason::CrossSession,
            ClientV2ResetReason::SnapshotMissing,
            ClientV2ResetReason::SnapshotExpired,
            ClientV2ResetReason::SnapshotHashMismatch,
            ClientV2ResetReason::StaleWriter,
            ClientV2ResetReason::ContentQuotaExceeded,
            ClientV2ResetReason::PerConnectionOverflow,
            ClientV2ResetReason::ProgressBufferOverflow,
            ClientV2ResetReason::ActorMailboxOverflow,
        ] {
            let expected = matches!(
                reason,
                ClientV2ResetReason::SnapshotMissing | ClientV2ResetReason::UpgradeRequired
            );
            assert_eq!(reset_is_read_only(reason), expected, "{reason:?}");
        }
    }

    #[test]
    fn r4_first_answer_wins_and_terminal_interaction_cannot_reopen() {
        let mut model = model();
        assert_eq!(
            model.apply_interaction_requested("i2"),
            InteractionOutcome::Requested
        );
        assert_eq!(
            model.apply_interaction_requested("i2"),
            InteractionOutcome::Duplicate
        );
        assert_eq!(
            model.apply_interaction_resolved("i2"),
            InteractionOutcome::Resolved
        );
        assert_eq!(
            model.apply_interaction_requested("i2"),
            InteractionOutcome::AlreadyTerminal
        );
        assert_eq!(
            model.apply_interaction_resolved("i2"),
            InteractionOutcome::AlreadyTerminal
        );
    }

    /// 未决集合**只记身份**：正文（`request`）不在这里落副本。
    ///
    /// 历史缺陷：镜像 `PendingInteraction` 漏了权威类型的 `request` 字段，而它
    /// 只有 `#[cfg(test)]` 读者，所以没人发现。现在正文直接由 typed payload 进
    /// UI 层，本状态机连存的地方都没有。
    #[test]
    fn pending_set_tracks_identity_only() {
        let bootstrap = fx::bootstrap_typed(
            SESSION,
            EPOCH,
            LOG,
            7,
            7,
            &[fx::pending_with_text(
                "int-ask",
                "call-ask",
                "turn-ask",
                "ask",
                "继续？",
            )],
            None,
        );
        // 权威类型确实带着正文——本状态机只是不复制它。
        let authoritative = &bootstrap.control.state.interactions[0];
        assert!(authoritative.request.is_some(), "正文在权威 payload 上");

        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(model.apply_bootstrap(&bootstrap), BootstrapOutcome::Applied);
        assert_eq!(
            model.pending_interactions().collect::<Vec<_>>(),
            ["int-ask"]
        );
    }

    /// 三种交互 kind 在 bootstrap 上都被认作未决身份（不再有 TUI 枚举映射层）。
    #[test]
    fn r1_r3_reconnect_restores_permission_ask_and_plan_by_stable_id() {
        for wire_kind in ["permission", "ask", "plan"] {
            let interaction_id = format!("int-{wire_kind}");
            let call_id = format!("call-{wire_kind}");
            let turn_id = format!("turn-{wire_kind}");
            let bootstrap = fx::bootstrap_typed(
                SESSION,
                EPOCH,
                LOG,
                7,
                7,
                &[fx::pending(&interaction_id, &call_id, &turn_id, wire_kind)],
                None,
            );
            let mut model = RingingV2SessionModel::new(SESSION);
            assert_eq!(model.apply_bootstrap(&bootstrap), BootstrapOutcome::Applied);
            assert_eq!(
                model.pending_interactions().collect::<Vec<_>>(),
                [interaction_id.as_str()]
            );
        }
    }

    #[test]
    fn d1_vacant_seat_claim_is_applied_with_monotonic_epoch() {
        let mut model = model();
        assert!(model.driver().is_none(), "基线快照没有 driver 段");
        let claimed = ClientV2DriverState {
            holder: Some("cs-1".into()),
            driver_epoch: 4,
            can_claim: false,
        };
        assert_eq!(
            model.apply_driver_state(claimed.clone()),
            DriverOutcome::Applied
        );
        assert!(model.is_driver("cs-1"));
        assert_eq!(model.apply_driver_state(claimed), DriverOutcome::Duplicate);
    }

    #[test]
    fn d2_busy_driver_rejects_other_claimants_stably() {
        let mut model = model();
        assert_eq!(
            model.apply_driver_state(ClientV2DriverState {
                holder: Some("cs-1".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Applied
        );
        assert_eq!(
            model.apply_driver_state(ClientV2DriverState {
                holder: Some("cs-2".into()),
                driver_epoch: 4,
                can_claim: true,
            }),
            DriverOutcome::Conflict
        );
        assert!(model.is_driver("cs-1"));
        assert!(!model.is_driver("cs-2"));
    }

    #[test]
    fn d3_handover_rejects_old_epoch_and_same_epoch_conflicts() {
        let mut model = model();
        let current = ClientV2DriverState {
            holder: Some("cs-1".into()),
            driver_epoch: 4,
            can_claim: false,
        };
        assert_eq!(
            model.apply_driver_state(current.clone()),
            DriverOutcome::Applied
        );
        assert_eq!(model.apply_driver_state(current), DriverOutcome::Duplicate);
        assert_eq!(
            model.apply_driver_state(ClientV2DriverState {
                holder: Some("cs-2".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Conflict
        );
        assert_eq!(
            model.apply_driver_state(ClientV2DriverState {
                holder: Some("cs-2".into()),
                driver_epoch: 5,
                can_claim: false,
            }),
            DriverOutcome::Applied
        );
        assert!(model.is_driver("cs-2"));
        assert_eq!(
            model.apply_driver_state(ClientV2DriverState {
                holder: Some("cs-1".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Stale
        );
        assert!(model.is_driver("cs-2"));
    }

    #[test]
    fn t2_stale_bootstrap_cannot_rollback_newer_snapshot() {
        let mut model = model();

        let newer = fx::bootstrap(SESSION, EPOCH, "v2.new", 1, 9);
        assert_eq!(model.apply_bootstrap(&newer), BootstrapOutcome::Applied);

        let stale = fx::bootstrap(SESSION, EPOCH, "v2.old", 1, 8);
        assert_eq!(model.apply_bootstrap(&stale), BootstrapOutcome::Stale);
        assert_eq!(model.cursor(), Some(newer.snapshot_cursor.as_str()));
        assert_eq!(model.state_revision(), 9);
    }

    #[test]
    fn t2_old_bootstrap_after_reset_must_match_reset_epoch_and_log() {
        let mut model = model();
        assert!(model.begin_reset(&fx::reset(
            SESSION,
            "epoch-2",
            Some("log-2"),
            None,
            ClientV2ResetReason::CursorExpired,
        )));

        let stale = fx::bootstrap(SESSION, EPOCH, LOG, 1, 7);
        assert_eq!(
            model.apply_bootstrap(&stale),
            BootstrapOutcome::ResetMismatch
        );
        assert!(model.is_reset_pending());
        assert_eq!(model.server_epoch(), Some(EPOCH));

        let wrong_log = fx::bootstrap(SESSION, "epoch-2", LOG, 1, 7);
        assert_eq!(
            model.apply_bootstrap(&wrong_log),
            BootstrapOutcome::ResetMismatch
        );
        assert!(model.is_reset_pending());

        let fresh = fx::bootstrap(SESSION, "epoch-2", "log-2", 1, 1);
        assert_eq!(model.apply_bootstrap(&fresh), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
        assert_eq!(model.server_epoch(), Some("epoch-2"));
    }

    #[test]
    fn t2_old_bootstrap_before_reset_baseline_is_rejected() {
        let mut model = model();
        assert!(model.begin_reset(&fx::reset(
            SESSION,
            EPOCH,
            Some(LOG),
            Some(("v2.reset.9", 9)),
            ClientV2ResetReason::CursorExpired,
        )));

        let old = fx::bootstrap(SESSION, EPOCH, LOG, 8, 8);
        assert_eq!(model.apply_bootstrap(&old), BootstrapOutcome::ResetMismatch);
        assert!(model.is_reset_pending());

        let fresh = fx::bootstrap(SESSION, EPOCH, LOG, 9, 9);
        assert_eq!(model.apply_bootstrap(&fresh), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
    }

    /// 会话身份不匹配的快照必须被拒（`session_id` 是权威键，不是 `session_id` 别名）。
    #[test]
    fn bootstrap_for_another_session_is_rejected() {
        let mut model = model();
        let other = fx::bootstrap("session-2", EPOCH, LOG, 1, 99);
        assert_eq!(
            model.apply_bootstrap(&other),
            BootstrapOutcome::SessionMismatch
        );
        assert_eq!(model.state_revision(), 7);

        let mut other_model = RingingV2SessionModel::new("session-2");
        assert!(!other_model.begin_reset(&fx::reset(
            SESSION,
            EPOCH,
            None,
            None,
            ClientV2ResetReason::CursorExpired
        )));
    }

    /// reset 帧带 session_id；不带它的旧形状根本解不出来（fail-loud）。
    #[test]
    fn reset_frame_requires_authoritative_session_id() {
        assert!(
            fx::reset_from_legacy_session_id_key().is_err(),
            "legacy session_id 键必须解析失败——这正是镜像时代静默漂移的那一类"
        );
    }

    /// `ts_ms`（beta-readiness W1/C3）是权威信封字段，TUI 直接读，不再自建时间戳。
    #[test]
    fn envelope_ts_ms_is_consumed_from_the_authoritative_source() {
        let mut event = fx::ReliableEvent::new(SESSION, EPOCH, LOG, 8, 1, audit_payload());
        event.ts_ms = Some(1_759_000_000_123);
        let built = event.build();
        let mut model = model();
        assert_eq!(
            model.apply_event(&built),
            ApplyOutcome::ReliableApplied {
                fact_seq: 8,
                projection_index: 1
            }
        );
        assert_eq!(fx::delivery_of(&built), ClientV2Delivery::Reliable);
        assert_eq!(
            built.ts_ms,
            Some(1_759_000_000_123),
            "权威信封把源 fact 墙钟带到了 TUI"
        );
        // 缺席时 `#[serde(default)]` 给 None：合成 ephemeral 事件就是这样。
        let synthetic = fx::ephemeral_event(SESSION, EPOCH, audit_payload());
        assert_eq!(synthetic.ts_ms, None);
        assert_eq!(
            fx::stream_key(Channel::Conversation),
            synthetic.stream_key,
            "stream_key 也走权威 serde"
        );
    }

    /// 权威交互 delta 的 kind 名与新状态机无映射层（直接断言枚举可命名即可）。
    #[test]
    fn delta_interaction_kind_is_authoritative() {
        let _ = ClientV2DeltaInteractionKind::Ask;
        // 权威 delta 能被 TUI 直接命名与匹配（不再经 TUI 自己的 kind 枚举）。
        let delta = fx::turn_started_delta("t1");
        assert!(matches!(
            delta,
            qaqh_client::ClientV2ConversationDelta::TurnStarted { .. }
        ));
    }
}
