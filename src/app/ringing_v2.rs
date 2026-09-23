//! Ringing v2 SessionModel 的纯 reducer。
//!
//! 本模块只持有 TUI 自己需要的状态，不解析 wire JSON，也不直接依赖
//! `qaqh-ringing` / `qaqh-session`。后续由 `qaqh-client` 的 v2 适配层把
//! typed envelope 映射成这里的 [`EventMeta`] / [`BootstrapSnapshot`]。
//!
//! 这样做的原因：wire 类型还在后端最小锚点里推进，而 cursor、reset、
//! interaction、driver 的**状态迁移规则**已经冻结。先把不随 wire 字段形状
//! 变化的核心状态机锁住，等适配层落地时只做机械映射。
//!
//! 当前未接线，允许 dead_code；适配层落地后删除本 allow。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use qaqh_client::{
    ClientV2Bootstrap, ClientV2Delivery, ClientV2Event, ClientV2InteractionKind, ClientV2Payload,
    ClientV2Reset, ClientV2ResetReason,
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

/// payload 的顶层 family。这里只做路由分类，不尝试从 `ClientV2Payload`
/// 内部类型中解出 interaction/driver；那部分由 #323 补齐 typed accessor 后接线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFamily {
    Conversation,
    Timeline,
    Control,
    Resource,
    Meta,
    AuditRef,
    Unknown,
}

/// 从 `qaqh-client` 的 typed bootstrap 构造纯 reducer 输入。
pub fn bootstrap_from_client(
    bootstrap: &ClientV2Bootstrap,
) -> Result<BootstrapSnapshot, &'static str> {
    let log_id = bootstrap.log_id()?;
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
        seed: bootstrap.seed.clone(),
        log_id: Some(log_id),
        snapshot_cursor: bootstrap.snapshot_cursor.as_str().to_string(),
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

pub fn payload_family(event: &ClientV2Event) -> PayloadFamily {
    match &event.payload {
        ClientV2Payload::ConversationDelta(_) => PayloadFamily::Conversation,
        ClientV2Payload::TimelineDelta(_) => PayloadFamily::Timeline,
        ClientV2Payload::ControlDelta(_) => PayloadFamily::Control,
        ClientV2Payload::ResourceDelta(_) => PayloadFamily::Resource,
        ClientV2Payload::MetaDelta(_) => PayloadFamily::Meta,
        ClientV2Payload::AuditRef(_) => PayloadFamily::AuditRef,
        ClientV2Payload::Unknown(_) => PayloadFamily::Unknown,
    }
}

/// 把 typed reset 映射为 reducer 的 reset signal。
pub fn reset_from_client(reset: &ClientV2Reset) -> ResetSignal {
    ResetSignal {
        server_epoch: reset.server_epoch.clone(),
        seed: reset.seed.clone(),
        log_id: reset.log_id.clone(),
        snapshot_cursor: reset
            .snapshot_cursor
            .as_ref()
            .map(|cursor| cursor.as_str().to_string()),
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

    pub fn seed(&self) -> &str {
        &self.seed
    }

    pub fn server_epoch(&self) -> Option<&str> {
        self.server_epoch.as_deref()
    }

    pub fn log_id(&self) -> Option<&str> {
        self.log_id.as_deref()
    }

    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    pub fn last_position(&self) -> (u64, u16) {
        (self.last_fact_seq, self.last_projection_index)
    }

    pub fn state_revision(&self) -> u64 {
        self.state_revision
    }

    pub fn reset(&self) -> Option<&ResetSignal> {
        self.reset.as_ref()
    }

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
    use qaqh_client::{ClientV2Cursor, ClientV2CursorToken};

    fn bootstrap() -> BootstrapSnapshot {
        BootstrapSnapshot {
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: "v2.snapshot".into(),
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
    fn reliable_is_strictly_forward_and_deduplicated() {
        let mut model = model();
        let first = EventMeta::reliable("epoch-1", "log-1", 8, 1, "v2.8.1", 8);
        assert_eq!(
            model.apply_event(first.clone()),
            ApplyOutcome::ReliableApplied {
                fact_seq: 8,
                projection_index: 1
            }
        );
        assert_eq!(model.cursor(), Some("v2.8.1"));
        assert_eq!(model.apply_event(first), ApplyOutcome::Duplicate);

        let stale = EventMeta::reliable("epoch-1", "log-1", 7, 9, "v2.7.9", 9);
        assert_eq!(model.apply_event(stale), ApplyOutcome::Stale);
    }

    #[test]
    fn replaceable_does_not_advance_cursor() {
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
    fn ephemeral_never_changes_state() {
        let mut model = model();
        let before = model.clone();
        assert_eq!(
            model.apply_event(EventMeta::ephemeral("epoch-1")),
            ApplyOutcome::Ephemeral
        );
        assert_eq!(model, before);
    }

    #[test]
    fn epoch_and_log_mismatch_are_rejected() {
        let mut model = model();
        let wrong_epoch = EventMeta::reliable("epoch-2", "log-1", 8, 1, "v2.8.1", 8);
        assert_eq!(model.apply_event(wrong_epoch), ApplyOutcome::EpochMismatch);

        let wrong_log = EventMeta::reliable("epoch-1", "log-2", 8, 1, "v2.8.1", 8);
        assert_eq!(model.apply_event(wrong_log), ApplyOutcome::LogMismatch);
    }

    #[test]
    fn reset_preserves_old_state_until_bootstrap() {
        let mut model = model();
        let old_cursor = model.cursor().map(str::to_string);
        assert!(model.begin_reset(ResetSignal {
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
            log_id: Some("log-1".into()),
            snapshot_cursor: Some("v2.new".into()),
            reason: ResetReason::CursorExpired,
        }));
        assert!(model.is_reset_pending());
        assert_eq!(model.cursor(), old_cursor.as_deref());
        assert_eq!(
            model.apply_event(EventMeta::reliable("epoch-1", "log-1", 8, 1, "v2.8.1", 8)),
            ApplyOutcome::ResetPending
        );

        let mut next = bootstrap();
        next.snapshot_cursor = "v2.new".into();
        next.state_revision = 9;
        assert_eq!(model.apply_bootstrap(next), BootstrapOutcome::Applied);
        assert!(!model.is_reset_pending());
        assert_eq!(model.cursor(), Some("v2.new"));
        assert_eq!(model.state_revision(), 9);
    }

    #[test]
    fn interaction_id_is_idempotent_and_terminal_after_resolution() {
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
    fn driver_epoch_is_monotonic_and_duplicate_is_ignored() {
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
                driver_epoch: 3,
                can_claim: true,
            }),
            DriverOutcome::Stale
        );
        assert!(model.is_driver("cs-1"));
    }

    #[test]
    fn client_bootstrap_adapter_maps_pending_driver_and_revision() {
        let token = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-1", 42))
            .expect("cursor");
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
                "state": {
                    "interactions": [{
                        "interaction_id": "i1",
                        "call_id": "c1",
                        "turn_id": "t1",
                        "kind": "permission"
                    }],
                    "driver": {
                        "holder": "cs-1",
                        "driver_epoch": 4,
                        "can_claim": false
                    }
                }
            },
            "conversation": {
                "channel": "conversation",
                "state_revision": 19,
                "snapshot_version": 1,
                "state": {}
            },
            "tool": {
                "channel": "tool",
                "state_revision": 11,
                "snapshot_version": 1,
                "state": {}
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
    fn client_event_adapter_maps_delivery_cursor_and_family() {
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
        assert_eq!(payload_family(&event), PayloadFamily::AuditRef);
    }

    #[test]
    fn client_reset_adapter_preserves_read_only_reason() {
        let cursor = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-1", 42))
            .expect("cursor");
        let reset = ClientV2Reset {
            schema: "qaqh.Ringing".into(),
            version: 2,
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
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
