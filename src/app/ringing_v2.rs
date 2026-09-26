//! Ringing v2 SessionModel 的纯 reducer。
//!
//! 本模块只持有 TUI 自己需要的状态，不解析 wire JSON，也不直接依赖
//! `qaqh-ringing` / `qaqh-session`。生产入口由 `qaqh-client` 的 v2 typed
//! surface 经本模块的适配函数映射为 [`EventMeta`] / [`BootstrapSnapshot`]。
//!
//! wire 类型由后端冻结，cursor、reset、interaction、driver 的状态迁移规则
//! 在这里独立锁定，便于 fixture 与跨平台回归复用。
use std::collections::{BTreeMap, BTreeSet};

use qaqh_client::{
    ClientV2Bootstrap, ClientV2Delivery, ClientV2Event, ClientV2InteractionKind, ClientV2Reset,
    ClientV2ResetReason,
};

/// v2 delivery 语义。只有 reliable 推进 canonical cursor。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Reliable,
    Replaceable,
    Ephemeral,
}

/// 一个 v2 事件的 canonical 元数据。
///
/// 这里刻意不保存 payload：payload 进入现有 transcript reducer，本模块只负责
/// epoch / log / cursor / revision / interaction / driver 这些跨事件不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMeta {
    pub server_epoch: String,
    pub log_id: Option<String>,
    pub fact_seq: Option<u64>,
    pub projection_index: Option<u16>,
    pub cursor: Option<String>,
    pub delivery: Delivery,
    pub revision: Option<u64>,
}

#[cfg(test)]
impl EventMeta {
    pub fn reliable(
        server_epoch: impl Into<String>,
        log_id: impl Into<String>,
        fact_seq: u64,
        projection_index: u16,
        cursor: impl Into<String>,
        revision: u64,
    ) -> Self {
        Self {
            server_epoch: server_epoch.into(),
            log_id: Some(log_id.into()),
            fact_seq: Some(fact_seq),
            projection_index: Some(projection_index),
            cursor: Some(cursor.into()),
            delivery: Delivery::Reliable,
            revision: Some(revision),
        }
    }

    pub fn replaceable(
        server_epoch: impl Into<String>,
        log_id: Option<String>,
        revision: u64,
    ) -> Self {
        Self {
            server_epoch: server_epoch.into(),
            log_id,
            fact_seq: None,
            projection_index: None,
            cursor: None,
            delivery: Delivery::Replaceable,
            revision: Some(revision),
        }
    }

    pub fn ephemeral(server_epoch: impl Into<String>) -> Self {
        Self {
            server_epoch: server_epoch.into(),
            log_id: None,
            fact_seq: None,
            projection_index: None,
            cursor: None,
            delivery: Delivery::Ephemeral,
            revision: None,
        }
    }
}

/// 从 `qaqh-client` 的 typed bootstrap 构造纯 reducer 输入。
pub fn bootstrap_from_client(
    bootstrap: &ClientV2Bootstrap,
) -> Result<BootstrapSnapshot, &'static str> {
    let cursor = bootstrap
        .snapshot_cursor
        .decode_snapshot()
        .map_err(|_| "invalid_snapshot_cursor")?;
    let log_id = cursor.log_id.clone();
    let state_revision = bootstrap
        .control
        .state_revision
        .max(bootstrap.conversation.state_revision)
        .max(bootstrap.tool.state_revision);
    let pending_interactions = bootstrap
        .control
        .state
        .interactions
        .iter()
        .map(|interaction| {
            Ok(PendingInteraction {
                interaction_id: interaction.interaction_id.clone(),
                call_id: interaction.call_id.clone(),
                turn_id: interaction.turn_id.clone(),
                kind: interaction_kind_from_client(interaction.kind)?,
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    let driver = bootstrap
        .control
        .state
        .driver
        .as_ref()
        .map(|driver| DriverState {
            holder: driver.holder.clone(),
            driver_epoch: driver.driver_epoch,
            can_claim: driver.can_claim,
        });

    Ok(BootstrapSnapshot {
        server_epoch: bootstrap.server_epoch.clone(),
        seed: bootstrap.session_id.clone(),
        log_id: Some(log_id),
        snapshot_cursor: bootstrap.snapshot_cursor.as_str().to_string(),
        snapshot_fact_seq: cursor.fact_seq,
        state_revision,
        pending_interactions,
        driver,
    })
}

/// 从 `qaqh-client` 的 typed envelope 提取 reducer 需要的 canonical 元数据。
pub fn event_meta_from_client(event: &ClientV2Event) -> EventMeta {
    EventMeta {
        server_epoch: event.server_epoch.clone(),
        log_id: event.log_id.clone(),
        fact_seq: event.fact_seq,
        projection_index: event.projection_index,
        cursor: event
            .cursor
            .as_ref()
            .map(|cursor| cursor.as_str().to_string()),
        delivery: match event.delivery {
            ClientV2Delivery::Reliable => Delivery::Reliable,
            ClientV2Delivery::Replaceable => Delivery::Replaceable,
            ClientV2Delivery::Ephemeral => Delivery::Ephemeral,
        },
        revision: event.revision,
    }
}

/// 把 typed reset 映射为 reducer 的 reset signal。
pub fn reset_from_client(reset: &ClientV2Reset) -> ResetSignal {
    ResetSignal {
        server_epoch: reset.server_epoch.clone(),
        seed: reset.session_id.clone(),
        log_id: reset.log_id.clone(),
        snapshot_cursor: reset
            .snapshot_cursor
            .as_ref()
            .map(|cursor| cursor.as_str().to_string()),
        snapshot_fact_seq: reset
            .snapshot_cursor
            .as_ref()
            .and_then(|cursor| cursor.decode_snapshot().ok())
            .map(|cursor| cursor.fact_seq),
        reason: match reset.reason {
            ClientV2ResetReason::CursorExpired => ResetReason::CursorExpired,
            ClientV2ResetReason::LogIdMismatch => ResetReason::LogIdMismatch,
            ClientV2ResetReason::UnknownFact => ResetReason::UnknownFact,
            ClientV2ResetReason::UpgradeRequired => ResetReason::UpgradeRequired,
            ClientV2ResetReason::ReplayOverflow => ResetReason::ReplayOverflow,
            ClientV2ResetReason::V1EpochMismatch => ResetReason::V1EpochMismatch,
            ClientV2ResetReason::CrossSession => ResetReason::CrossSession,
            ClientV2ResetReason::SnapshotMissing => ResetReason::SnapshotMissing,
            ClientV2ResetReason::SnapshotExpired => ResetReason::SnapshotExpired,
            ClientV2ResetReason::SnapshotHashMismatch => ResetReason::SnapshotHashMismatch,
            ClientV2ResetReason::StaleWriter => ResetReason::StaleWriter,
            ClientV2ResetReason::ContentQuotaExceeded => ResetReason::ContentQuotaExceeded,
            ClientV2ResetReason::PerConnectionOverflow => ResetReason::PerConnectionOverflow,
            ClientV2ResetReason::ProgressBufferOverflow => ResetReason::ProgressBufferOverflow,
            ClientV2ResetReason::ActorMailboxOverflow => ResetReason::ActorMailboxOverflow,
        },
    }
}

fn interaction_kind_from_client(
    kind: ClientV2InteractionKind,
) -> Result<InteractionKind, &'static str> {
    match kind {
        ClientV2InteractionKind::Permission => Ok(InteractionKind::Permission),
        ClientV2InteractionKind::Ask => Ok(InteractionKind::Ask),
        ClientV2InteractionKind::PlanReview => Ok(InteractionKind::PlanReview),
    }
}

/// bootstrap 的三频道状态中，本模块只消费 control 的 pending/driver，
/// 以及三频道中最大的 `state_revision`。领域 payload 由上层保存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSnapshot {
    pub server_epoch: String,
    pub seed: String,
    pub log_id: Option<String>,
    pub snapshot_cursor: String,
    pub snapshot_fact_seq: u64,
    pub state_revision: u64,
    pub pending_interactions: Vec<PendingInteraction>,
    pub driver: Option<DriverState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionKind {
    Permission,
    Ask,
    PlanReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInteraction {
    pub interaction_id: String,
    pub call_id: String,
    pub turn_id: String,
    pub kind: InteractionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverState {
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub can_claim: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    CursorExpired,
    LogIdMismatch,
    UnknownFact,
    UpgradeRequired,
    ReplayOverflow,
    V1EpochMismatch,
    CrossSession,
    SnapshotMissing,
    SnapshotExpired,
    SnapshotHashMismatch,
    StaleWriter,
    ContentQuotaExceeded,
    PerConnectionOverflow,
    ProgressBufferOverflow,
    ActorMailboxOverflow,
}

impl ResetReason {
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::SnapshotMissing | Self::UpgradeRequired)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetSignal {
    pub server_epoch: String,
    pub seed: String,
    pub log_id: Option<String>,
    pub snapshot_cursor: Option<String>,
    pub snapshot_fact_seq: Option<u64>,
    pub reason: ResetReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Malformed(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    Applied,
    SeedMismatch,
    Invalid,
    ResetMismatch,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionOutcome {
    Requested,
    Resolved,
    Expired,
    Duplicate,
    AlreadyTerminal,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverOutcome {
    Applied,
    Duplicate,
    Stale,
    Conflict,
}

/// 单 seed 的 Ringing v2 会话状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingingV2SessionModel {
    seed: String,
    server_epoch: Option<String>,
    log_id: Option<String>,
    cursor: Option<String>,
    last_fact_seq: u64,
    last_projection_index: u16,
    state_revision: u64,
    pending_interactions: BTreeMap<String, PendingInteraction>,
    terminal_interactions: BTreeSet<String>,
    driver: Option<DriverState>,
    reset: Option<ResetSignal>,
}

impl RingingV2SessionModel {
    pub fn new(seed: impl Into<String>) -> Self {
        Self {
            seed: seed.into(),
            server_epoch: None,
            log_id: None,
            cursor: None,
            last_fact_seq: 0,
            last_projection_index: 0,
            state_revision: 0,
            pending_interactions: BTreeMap::new(),
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
            .is_some_and(|reset| reset.reason.is_read_only())
    }

    pub fn driver(&self) -> Option<&DriverState> {
        self.driver.as_ref()
    }

    pub fn is_driver(&self, client_session_id: &str) -> bool {
        self.driver
            .as_ref()
            .and_then(|driver| driver.holder.as_deref())
            == Some(client_session_id)
    }

    #[cfg(test)]
    pub fn pending_interactions(&self) -> impl Iterator<Item = &PendingInteraction> {
        self.pending_interactions.values()
    }

    /// 在已经完成新 bootstrap 校验后，原子替换旧模型。
    pub fn apply_bootstrap(&mut self, bootstrap: BootstrapSnapshot) -> BootstrapOutcome {
        if bootstrap.seed != self.seed {
            return BootstrapOutcome::SeedMismatch;
        }
        if bootstrap.server_epoch.trim().is_empty() || bootstrap.snapshot_cursor.trim().is_empty() {
            return BootstrapOutcome::Invalid;
        }

        // reset 之后的 bootstrap 必须仍指向 reset 宣告的 epoch/log。这样即使
        // 旧请求的响应比新请求晚到，也不能清掉 reset 或覆盖新快照。
        if let Some(reset) = &self.reset {
            if bootstrap.server_epoch != reset.server_epoch {
                return BootstrapOutcome::ResetMismatch;
            }
            if let Some(reset_log_id) = reset.log_id.as_deref()
                && bootstrap.log_id.as_deref() != Some(reset_log_id)
            {
                return BootstrapOutcome::ResetMismatch;
            }
            if let Some(reset_fact_seq) = reset.snapshot_fact_seq
                && bootstrap.snapshot_fact_seq < reset_fact_seq
            {
                return BootstrapOutcome::ResetMismatch;
            }
        } else if self.server_epoch.as_deref() == Some(bootstrap.server_epoch.as_str())
            && bootstrap.state_revision < self.state_revision
        {
            // 同一 epoch 内 revision 单调；较旧的并发 bootstrap 响应不得把
            // SessionModel 回滚到更早的 cursor / pending / driver 快照。
            return BootstrapOutcome::Stale;
        }

        self.server_epoch = Some(bootstrap.server_epoch);
        self.log_id = bootstrap.log_id;
        self.cursor = Some(bootstrap.snapshot_cursor);
        // snapshot cursor 对客户端是不透明 token，不在这里解码；后续 replay
        // 由服务端保证严格大于 snapshot baseline。
        self.last_fact_seq = 0;
        self.last_projection_index = 0;
        self.state_revision = bootstrap.state_revision;
        self.pending_interactions = bootstrap
            .pending_interactions
            .into_iter()
            .map(|interaction| (interaction.interaction_id.clone(), interaction))
            .collect();
        self.terminal_interactions.clear();
        self.driver = bootstrap.driver;
        self.reset = None;
        BootstrapOutcome::Applied
    }

    /// 标记 reset；保留旧 UI 状态，直到新 bootstrap 通过校验。
    pub fn begin_reset(&mut self, reset: ResetSignal) -> bool {
        if reset.seed != self.seed {
            return false;
        }
        self.reset = Some(reset);
        true
    }

    pub fn apply_event(&mut self, event: EventMeta) -> ApplyOutcome {
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
            Delivery::Reliable => self.apply_reliable(event),
            Delivery::Replaceable => self.apply_replaceable(event),
            Delivery::Ephemeral => ApplyOutcome::Ephemeral,
        }
    }

    fn apply_reliable(&mut self, event: EventMeta) -> ApplyOutcome {
        let Some(log_id) = event.log_id else {
            return ApplyOutcome::Malformed("reliable requires log_id");
        };
        let Some(fact_seq) = event.fact_seq else {
            return ApplyOutcome::Malformed("reliable requires fact_seq");
        };
        let Some(projection_index) = event.projection_index else {
            return ApplyOutcome::Malformed("reliable requires projection_index");
        };
        let Some(cursor) = event.cursor else {
            return ApplyOutcome::Malformed("reliable requires cursor");
        };
        let Some(revision) = event.revision else {
            return ApplyOutcome::Malformed("reliable requires revision");
        };

        if let Some(current_log) = self.log_id.as_deref() {
            if current_log != log_id {
                return ApplyOutcome::LogMismatch;
            }
        } else {
            self.log_id = Some(log_id);
        }

        let incoming = (fact_seq, projection_index);
        let current = (self.last_fact_seq, self.last_projection_index);
        if incoming == current {
            return ApplyOutcome::Duplicate;
        }
        if incoming < current {
            return ApplyOutcome::Stale;
        }

        self.cursor = Some(cursor);
        self.last_fact_seq = fact_seq;
        self.last_projection_index = projection_index;
        self.state_revision = self.state_revision.max(revision);
        ApplyOutcome::ReliableApplied {
            fact_seq,
            projection_index,
        }
    }

    fn apply_replaceable(&mut self, event: EventMeta) -> ApplyOutcome {
        let Some(revision) = event.revision else {
            return ApplyOutcome::Malformed("replaceable requires revision");
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

    pub fn apply_interaction_requested(
        &mut self,
        interaction: PendingInteraction,
    ) -> InteractionOutcome {
        if self
            .terminal_interactions
            .contains(&interaction.interaction_id)
        {
            return InteractionOutcome::AlreadyTerminal;
        }
        if self
            .pending_interactions
            .contains_key(&interaction.interaction_id)
        {
            return InteractionOutcome::Duplicate;
        }
        self.pending_interactions
            .insert(interaction.interaction_id.clone(), interaction);
        InteractionOutcome::Requested
    }

    pub fn apply_interaction_resolved(&mut self, interaction_id: &str) -> InteractionOutcome {
        if self.terminal_interactions.contains(interaction_id) {
            return InteractionOutcome::AlreadyTerminal;
        }
        let removed = self.pending_interactions.remove(interaction_id);
        self.terminal_interactions
            .insert(interaction_id.to_string());
        if removed.is_some() {
            InteractionOutcome::Resolved
        } else {
            InteractionOutcome::Unknown
        }
    }

    pub fn apply_interaction_expired(&mut self, interaction_id: &str) -> InteractionOutcome {
        if self.terminal_interactions.contains(interaction_id) {
            return InteractionOutcome::AlreadyTerminal;
        }
        let removed = self.pending_interactions.remove(interaction_id);
        self.terminal_interactions
            .insert(interaction_id.to_string());
        if removed.is_some() {
            InteractionOutcome::Expired
        } else {
            InteractionOutcome::Unknown
        }
    }

    pub fn apply_driver_state(&mut self, driver: DriverState) -> DriverOutcome {
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

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{
        ClientV2ControlState, ClientV2ConversationState, ClientV2Cursor, ClientV2CursorToken,
        ClientV2ToolState,
    };

    fn bootstrap() -> BootstrapSnapshot {
        BootstrapSnapshot {
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: "v2.snapshot".into(),
            snapshot_fact_seq: 0,
            state_revision: 7,
            pending_interactions: vec![PendingInteraction {
                interaction_id: "i1".into(),
                call_id: "c1".into(),
                turn_id: "t1".into(),
                kind: InteractionKind::Permission,
            }],
            driver: Some(DriverState {
                holder: None,
                driver_epoch: 3,
                can_claim: true,
            }),
        }
    }

    fn model() -> RingingV2SessionModel {
        let mut model = RingingV2SessionModel::new("seed-1");
        assert_eq!(
            model.apply_bootstrap(bootstrap()),
            BootstrapOutcome::Applied
        );
        model
    }

    /// 从 typed client bootstrap 的 wire 形态构造 fixture。三个频道基线统一
    /// 由客户端 `Default` 生成，只覆写本矩阵关心的 pending/driver 字段。
    fn client_bootstrap(
        seed: &str,
        server_epoch: &str,
        log_id: &str,
        snapshot_fact_seq: u64,
        interactions: impl serde::Serialize,
        driver: impl serde::Serialize,
    ) -> ClientV2Bootstrap {
        let token = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot(
            log_id,
            snapshot_fact_seq,
        ))
        .expect("snapshot cursor");
        let mut control =
            serde_json::to_value(ClientV2ControlState::default()).expect("control baseline");
        control["interactions"] =
            serde_json::to_value(interactions).expect("interaction fixture JSON");
        control["driver"] = serde_json::to_value(driver).expect("driver fixture JSON");
        let conversation = serde_json::to_value(ClientV2ConversationState::default())
            .expect("conversation baseline");
        let tool = serde_json::to_value(ClientV2ToolState::default()).expect("tool baseline");
        serde_json::from_value(serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 2,
            "server_epoch": server_epoch,
            "seed": seed,
            "snapshot_cursor": token.as_str(),
            "control": {
                "channel": "control",
                "state_revision": 7,
                "snapshot_version": 1,
                "state": control
            },
            "conversation": {
                "channel": "conversation",
                "state_revision": 7,
                "snapshot_version": 1,
                "state": conversation
            },
            "tool": {
                "channel": "tool",
                "state_revision": 7,
                "snapshot_version": 1,
                "state": tool
            }
        }))
        .expect("client bootstrap")
    }

    #[test]
    fn bootstrap_sets_pending_and_driver() {
        let model = model();
        assert_eq!(model.server_epoch(), Some("epoch-1"));
        assert_eq!(model.cursor(), Some("v2.snapshot"));
        assert_eq!(model.state_revision(), 7);
        assert_eq!(model.pending_interactions().count(), 1);
        assert_eq!(model.driver().expect("driver").driver_epoch, 3);
        assert!(!model.is_driver("cs-1"));
    }

    #[test]
    fn v2_c1_snapshot_then_subscribe_is_gap_free_and_duplicate_free() {
        let mut model = model();
        assert_eq!((model.last_fact_seq, model.last_projection_index), (0, 0));
        assert_eq!(model.cursor(), Some("v2.snapshot"));

        let first = EventMeta::reliable("epoch-1", "log-1", 8, 1, "v2.8.1", 8);
        assert_eq!(
            model.apply_event(first.clone()),
            ApplyOutcome::ReliableApplied {
                fact_seq: 8,
                projection_index: 1
            }
        );
        assert_eq!((model.last_fact_seq, model.last_projection_index), (8, 1));
        assert_eq!(model.cursor(), Some("v2.8.1"));
        assert_eq!(model.apply_event(first), ApplyOutcome::Duplicate);
    }

    #[test]
    fn v2_c2_reliable_reconnect_advances_in_global_lexicographic_order() {
        let mut model = model();
        for (fact_seq, projection_index) in [(8, 0), (8, 1), (9, 0)] {
            assert_eq!(
                model.apply_event(EventMeta::reliable(
                    "epoch-1",
                    "log-1",
                    fact_seq,
                    projection_index,
                    format!("v2.{fact_seq}.{projection_index}"),
                    fact_seq,
                )),
                ApplyOutcome::ReliableApplied {
                    fact_seq,
                    projection_index
                }
            );
        }
        assert_eq!((model.last_fact_seq, model.last_projection_index), (9, 0));
        assert_eq!(
            model.apply_event(EventMeta::reliable(
                "epoch-1", "log-1", 8, 99, "v2.stale", 99
            )),
            ApplyOutcome::Stale
        );
    }

    #[test]
    fn v2_c3_replaceable_reconnect_is_latest_current_without_cursor_advance() {
        let mut model = model();
        let cursor = model.cursor().map(str::to_string);
        let event = EventMeta::replaceable("epoch-1", Some("log-1".into()), 8);
        assert_eq!(
            model.apply_event(event.clone()),
            ApplyOutcome::ReplaceableApplied { revision: 8 }
        );
        assert_eq!(model.cursor(), cursor.as_deref());
        assert_eq!(model.state_revision(), 8);
        assert_eq!(model.apply_event(event), ApplyOutcome::Duplicate);
    }

    #[test]
    fn v2_c4_ephemeral_never_persists_or_replays() {
        let mut model = model();
        let before = model.clone();
        assert_eq!(
            model.apply_event(EventMeta::ephemeral("epoch-1")),
            ApplyOutcome::Ephemeral
        );
        assert_eq!(model, before);
    }

    #[test]
    fn v2_c5_epoch_log_mismatch_is_rejected_and_log_reset_maps_typed_reason() {
        let mut model = model();
        let wrong_epoch = EventMeta::reliable("epoch-2", "log-1", 8, 1, "v2.8.1", 8);
        assert_eq!(model.apply_event(wrong_epoch), ApplyOutcome::EpochMismatch);

        let wrong_log = EventMeta::reliable("epoch-1", "log-2", 8, 1, "v2.8.1", 8);
        assert_eq!(model.apply_event(wrong_log), ApplyOutcome::LogMismatch);

        let reset = reset_from_client(&ClientV2Reset {
            schema: "qaqh.Ringing".into(),
            version: 2,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            log_id: Some("log-2".into()),
            snapshot_cursor: Some(
                ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-2", 8))
                    .expect("snapshot cursor"),
            ),
            reason: ClientV2ResetReason::LogIdMismatch,
        });
        assert_eq!(reset.reason, ResetReason::LogIdMismatch);
        assert!(model.begin_reset(reset));
        assert!(model.is_reset_pending());
        assert!(!model.is_read_only());
    }

    #[test]
    fn v2_c6_cursor_expired_keeps_old_state_until_rebaseline() {
        let mut model = model();
        let old_cursor = model.cursor().map(str::to_string);
        let signal = ResetSignal {
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: Some("v2.new".into()),
            snapshot_fact_seq: Some(8),
            reason: ResetReason::CursorExpired,
        };
        assert!(model.begin_reset(signal));
        assert!(model.is_reset_pending());
        assert_eq!(model.cursor(), old_cursor.as_deref());
        assert_eq!(
            model.apply_event(EventMeta::reliable("epoch-1", "log-1", 8, 1, "v2.8.1", 8)),
            ApplyOutcome::ResetPending
        );

        let mut next = bootstrap();
        next.snapshot_cursor = "v2.new".into();
        next.snapshot_fact_seq = 8;
        next.state_revision = 9;
        assert_eq!(model.apply_bootstrap(next), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
        assert_eq!(model.cursor(), Some("v2.new"));
        assert_eq!(model.state_revision(), 9);
    }

    #[test]
    fn v2_c7_snapshot_missing_is_read_only_and_does_not_guess_history() {
        let mut model = model();
        let old_cursor = model.cursor().map(str::to_string);
        let old_revision = model.state_revision();
        let signal = reset_from_client(&ClientV2Reset {
            schema: "qaqh.Ringing".into(),
            version: 2,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            log_id: None,
            snapshot_cursor: None,
            reason: ClientV2ResetReason::SnapshotMissing,
        });

        assert!(model.begin_reset(signal));
        assert!(model.is_reset_pending());
        assert!(model.is_read_only());
        assert_eq!(model.cursor(), old_cursor.as_deref());
        assert_eq!(model.state_revision(), old_revision);
        assert_eq!(
            model.apply_event(EventMeta::reliable("epoch-1", "log-1", 8, 1, "v2.8.1", 8)),
            ApplyOutcome::ResetPending
        );
    }

    #[test]
    fn v2_r4_first_answer_wins_and_terminal_interaction_cannot_reopen() {
        let mut model = model();
        let request = PendingInteraction {
            interaction_id: "i2".into(),
            call_id: "c2".into(),
            turn_id: "t2".into(),
            kind: InteractionKind::Ask,
        };
        assert_eq!(
            model.apply_interaction_requested(request.clone()),
            InteractionOutcome::Requested
        );
        assert_eq!(
            model.apply_interaction_requested(request),
            InteractionOutcome::Duplicate
        );
        assert_eq!(
            model.apply_interaction_resolved("i2"),
            InteractionOutcome::Resolved
        );
        assert_eq!(
            model.apply_interaction_requested(PendingInteraction {
                interaction_id: "i2".into(),
                call_id: "c2".into(),
                turn_id: "t2".into(),
                kind: InteractionKind::Ask,
            }),
            InteractionOutcome::AlreadyTerminal
        );
        assert_eq!(
            model.apply_interaction_resolved("i2"),
            InteractionOutcome::AlreadyTerminal
        );
    }

    #[test]
    fn v2_r1_r3_reconnect_restores_permission_ask_and_plan_by_stable_id() {
        for (wire_kind, expected_kind) in [
            ("permission", InteractionKind::Permission),
            ("ask", InteractionKind::Ask),
            ("plan", InteractionKind::PlanReview),
        ] {
            let interaction_id = format!("int-{wire_kind}");
            let call_id = format!("call-{wire_kind}");
            let turn_id = format!("turn-{wire_kind}");
            let bootstrap = client_bootstrap(
                "seed-1",
                "epoch-1",
                "log-1",
                7,
                serde_json::json!([{
                    "interaction_id": interaction_id,
                    "call_id": call_id,
                    "turn_id": turn_id,
                    "kind": wire_kind,
                    "request": null
                }]),
                (),
            );
            let snapshot = bootstrap_from_client(&bootstrap).expect("typed bootstrap adapter");
            let mut model = RingingV2SessionModel::new("seed-1");
            assert_eq!(model.apply_bootstrap(snapshot), BootstrapOutcome::Applied);

            let pending: Vec<_> = model.pending_interactions().collect();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].interaction_id, interaction_id);
            assert_eq!(pending[0].call_id, call_id);
            assert_eq!(pending[0].turn_id, turn_id);
            assert_eq!(pending[0].kind, expected_kind);
        }
    }

    #[test]
    fn v2_d1_vacant_seat_claim_is_applied_with_monotonic_epoch() {
        let mut model = model();
        let vacant = model.driver().expect("bootstrap driver");
        assert_eq!(vacant.holder, None);
        assert!(vacant.can_claim);

        let claimed = DriverState {
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
    fn v2_d2_busy_driver_rejects_other_claimants_stably() {
        let mut model = model();
        assert_eq!(
            model.apply_driver_state(DriverState {
                holder: Some("cs-1".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Applied
        );
        assert_eq!(
            model.apply_driver_state(DriverState {
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
    fn v2_d3_handover_rejects_old_epoch_and_same_epoch_conflicts() {
        let mut model = model();
        let current = DriverState {
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
            model.apply_driver_state(DriverState {
                holder: Some("cs-2".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Conflict
        );
        assert_eq!(
            model.apply_driver_state(DriverState {
                holder: Some("cs-2".into()),
                driver_epoch: 5,
                can_claim: false,
            }),
            DriverOutcome::Applied
        );
        assert!(model.is_driver("cs-2"));
        assert_eq!(
            model.apply_driver_state(DriverState {
                holder: Some("cs-1".into()),
                driver_epoch: 4,
                can_claim: false,
            }),
            DriverOutcome::Stale
        );
        assert!(model.is_driver("cs-2"));
    }

    #[test]
    fn v2_t2_stale_bootstrap_cannot_rollback_newer_snapshot() {
        let mut model = model();

        let mut newer = bootstrap();
        newer.snapshot_cursor = "v2.new".into();
        newer.state_revision = 9;
        assert_eq!(model.apply_bootstrap(newer), BootstrapOutcome::Applied);

        let mut stale = bootstrap();
        stale.snapshot_cursor = "v2.old".into();
        stale.state_revision = 8;
        assert_eq!(model.apply_bootstrap(stale), BootstrapOutcome::Stale);
        assert_eq!(model.cursor(), Some("v2.new"));
        assert_eq!(model.state_revision(), 9);
    }

    #[test]
    fn v2_t2_old_bootstrap_after_reset_must_match_reset_epoch_and_log() {
        let mut model = model();
        assert!(model.begin_reset(ResetSignal {
            server_epoch: "epoch-2".into(),
            seed: "seed-1".into(),
            log_id: Some("log-2".into()),
            snapshot_cursor: None,
            snapshot_fact_seq: None,
            reason: ResetReason::CursorExpired,
        }));

        let mut stale = bootstrap();
        stale.server_epoch = "epoch-1".into();
        stale.log_id = Some("log-1".into());
        assert_eq!(
            model.apply_bootstrap(stale),
            BootstrapOutcome::ResetMismatch
        );
        assert!(model.is_reset_pending());
        assert_eq!(model.server_epoch(), Some("epoch-1"));

        let mut wrong_log = bootstrap();
        wrong_log.server_epoch = "epoch-2".into();
        wrong_log.log_id = Some("log-1".into());
        assert_eq!(
            model.apply_bootstrap(wrong_log),
            BootstrapOutcome::ResetMismatch
        );
        assert!(model.is_reset_pending());

        let mut fresh = bootstrap();
        fresh.server_epoch = "epoch-2".into();
        fresh.log_id = Some("log-2".into());
        fresh.snapshot_cursor = "v2.fresh".into();
        fresh.state_revision = 1;
        assert_eq!(model.apply_bootstrap(fresh), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
        assert_eq!(model.server_epoch(), Some("epoch-2"));
        assert_eq!(model.cursor(), Some("v2.fresh"));
    }

    #[test]
    fn v2_t2_old_bootstrap_before_reset_baseline_is_rejected() {
        let mut model = model();
        assert!(model.begin_reset(ResetSignal {
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: Some("v2.reset.9".into()),
            snapshot_fact_seq: Some(9),
            reason: ResetReason::CursorExpired,
        }));

        let mut old = bootstrap();
        old.snapshot_fact_seq = 8;
        old.state_revision = 8;
        assert_eq!(model.apply_bootstrap(old), BootstrapOutcome::ResetMismatch);
        assert!(model.is_reset_pending());

        let mut fresh = bootstrap();
        fresh.snapshot_fact_seq = 9;
        fresh.state_revision = 9;
        assert_eq!(model.apply_bootstrap(fresh), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
    }

    #[test]
    fn client_bootstrap_adapter_maps_pending_driver_and_revision() {
        let token = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-1", 42))
            .expect("cursor");
        // 三个频道快照在 `qaqh-client` 里是手写/领域结构体：非 `Option` 字段在
        // wire 上都是**必填**（daemon 逐字段发全量，见 `axum_impl/v2.rs` 的
        // `V2ControlState`），所以 `"state": {}` 会在反序列化时红。
        // 基线统一用客户端自己的 `Default` 生成，本用例只覆写真正关心的
        // `interactions` / `driver` —— 后端再往快照里加必填字段时不用回来补
        // fixture，但整条 `from_value` 反序列化路径仍然被走一遍。
        let mut control_state =
            serde_json::to_value(ClientV2ControlState::default()).expect("control baseline");
        control_state["interactions"] = serde_json::json!([{
            "interaction_id": "i1",
            "call_id": "c1",
            "turn_id": "t1",
            "kind": "permission"
        }]);
        control_state["driver"] = serde_json::json!({
            "holder": "cs-1",
            "driver_epoch": 4,
            "can_claim": false
        });
        let conversation_state = serde_json::to_value(ClientV2ConversationState::default())
            .expect("conversation baseline");
        let tool_state = serde_json::to_value(ClientV2ToolState::default()).expect("tool baseline");
        let value = serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 2,
            "server_epoch": "epoch-1",
            "seed": "seed-1",
            "snapshot_cursor": token.as_str(),
            "control": {
                "channel": "control",
                "state_revision": 7,
                "snapshot_version": 1,
                "state": control_state
            },
            "conversation": {
                "channel": "conversation",
                "state_revision": 19,
                "snapshot_version": 1,
                "state": conversation_state
            },
            "tool": {
                "channel": "tool",
                "state_revision": 11,
                "snapshot_version": 1,
                "state": tool_state
            }
        });
        let bootstrap: ClientV2Bootstrap = serde_json::from_value(value).expect("client bootstrap");
        let mapped = bootstrap_from_client(&bootstrap).expect("adapter");
        assert_eq!(mapped.log_id.as_deref(), Some("log-1"));
        assert_eq!(mapped.state_revision, 19);
        assert_eq!(mapped.pending_interactions.len(), 1);
        assert_eq!(
            mapped.pending_interactions[0].kind,
            InteractionKind::Permission
        );
        assert_eq!(mapped.driver.expect("driver").driver_epoch, 4);
    }

    #[test]
    fn client_plan_review_variant_maps_to_internal_plan_review() {
        assert_eq!(
            interaction_kind_from_client(ClientV2InteractionKind::PlanReview).expect("kind"),
            InteractionKind::PlanReview,
            "TUI 只依赖 typed variant；wire rename(plan) 由 qaqh-client serde 承担"
        );
    }

    #[test]
    fn client_event_adapter_maps_delivery_and_cursor() {
        let cursor = ClientV2CursorToken::encode_reliable(&ClientV2Cursor::new("log-1", 8, 1))
            .expect("cursor");
        let value = serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 2,
            "server_epoch": "epoch-1",
            "seed": "seed-1",
            "event_id": "e1",
            "stream_key": {"kind": "channel", "data": "control"},
            "delivery": "reliable",
            "cursor": cursor.as_str(),
            "log_id": "log-1",
            "fact_seq": 8,
            "projection_index": 1,
            "revision": 8,
            "payload": {
                "kind": "audit_ref",
                "data": {
                    "audit_seq": 1,
                    "audit_hash": "abc"
                }
            }
        });
        let event: ClientV2Event = serde_json::from_value(value).expect("client event");
        let meta = event_meta_from_client(&event);
        assert_eq!(meta.delivery, Delivery::Reliable);
        assert_eq!(meta.log_id.as_deref(), Some("log-1"));
        assert_eq!(meta.fact_seq, Some(8));
        assert_eq!(meta.projection_index, Some(1));
        assert_eq!(meta.cursor.as_deref(), Some(cursor.as_str()));
    }

    #[test]
    fn client_reset_adapter_preserves_read_only_reason() {
        let cursor = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-1", 42))
            .expect("cursor");
        let reset = ClientV2Reset {
            schema: "qaqh.Ringing".into(),
            version: 2,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: Some(cursor),
            reason: ClientV2ResetReason::SnapshotMissing,
        };
        let signal = reset_from_client(&reset);
        assert_eq!(signal.reason, ResetReason::SnapshotMissing);
        assert!(signal.reason.is_read_only());
        assert_eq!(
            signal.snapshot_cursor.as_deref(),
            Some(reset.snapshot_cursor.as_ref().expect("cursor").as_str())
        );
    }
}
