//! Ringing v3 会话状态机（TUI 侧不变量）。
//!
//! **本模块不持有任何手抄协议类型**：bootstrap / 事件帧 / 交互视图 / driver
//! 全部来自 `qaqh-ringing-contract`（backend-v3 起本仓直接 path 依赖权威契约
//! crate，类型漂移 = 编译错误）。这里只剩 TUI 自己的会话级时序规则：
//!
//! 1. **exclusive 水位**（backend-v3 B01）：`SessionState.seq` 是「下一个将
//!    分配的事件序号」。bootstrap 后 `next_seq = state.seq`；事件帧
//!    `seq < next_seq` 已被快照覆盖（丢弃），`seq >= next_seq` 是新事实。
//!    相等边界必须处理——首帧的 `seq` 恰好等于 `next_seq`。
//! 2. **resync（旧名 reset）**：服务端判定缺口/过期后发 `resync_required`
//!    并断流（B01/B09）。TUI 保留旧 UI 状态，直到新 bootstrap 校验通过后
//!    原子替换；等待期间本地视图已知过期，写入口一律关闭。
//! 3. **交互身份「第一答复胜出」**：`InteractionRequested` 记身份，
//!    `InteractionResolved`（含 Expired/Cancelled）封口；终态不得重开。
//! 4. **driver 席位按 epoch 单调交接**（B02）：`DriverChanged` 的 `None`
//!    表示席位清空；epoch 回退视为陈旧帧。
//!
//! 历史（2026-09-30 之前）本模块曾自带六份手抄协议镜像；漂移真实发生过且
//! 失败模式是静默的（详见 git 历史版本注释）。v3 起类型全部直连权威 crate，
//! 连「等 qaqh-client 补转出口」这一步都不再存在。

use std::collections::BTreeSet;

use qaqh_ringing::SESSION_CONTRACT_REVISION;
use qaqh_ringing_contract::state::DriverState;
use qaqh_ringing_contract::{SessionBootstrap, SessionEventFrame};

/// resync 等待期是否只读。
///
/// **这是 TUI 的展示策略**（本地视图已知过期时不给写入口），不是 wire 语义。
/// v3 的 `resync_required.reason` 是自由字符串（`cursor_gap` / `stream_lagged`，
/// 见 daemon `axum_impl/v2.rs` 的 resync 分支）——没有可穷举的权威枚举，
/// 所以策略是「任何 resync 都只读直到 re-baseline」。这比 v3 之前的
/// 「按原因分类」更保守，也更符合 B09 的裁定：resync 后未提交的暂态
/// 本来就不承诺恢复。
pub fn resync_is_read_only() -> bool {
    true
}

/// 一次 `apply_frame` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 帧被接受；`seq` 是该帧的序号（本帧之后 `next_seq = seq + 1`）。
    Applied { seq: u64 },
    /// 帧序号已落在快照水位之下（bootstrap 已覆盖 / 重放）。
    Duplicate,
    NotInitialized,
    ResyncPending,
}

/// 一次 `apply_bootstrap` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    Applied,
    SessionMismatch,
    Invalid,
    /// 同一 epoch 内水位倒退的旧快照：不得回滚。
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

/// 单会话的 Ringing v3 时序状态。
///
/// **只存 TUI 需要的跨事件不变量**：领域载荷（交互正文、工具卡、transcript）一律
/// 由权威 typed payload 直接进入 UI 层，不在这里落副本——副本必然漏字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingingV2SessionModel {
    session_id: String,
    server_epoch: Option<String>,
    /// exclusive 水位：下一个允许的事件 seq（bootstrap `state.seq` 起）。
    next_seq: u64,
    initialized: bool,
    /// 未决交互**只记身份**：正文与 kind 属权威 `InteractionView`，UI 层直接
    /// 消费，这里再存一份就会漏字段（历史缺陷）。
    pending_interactions: BTreeSet<String>,
    terminal_interactions: BTreeSet<String>,
    driver: Option<DriverState>,
    /// 待处理 resync 的**权威原帧**：新 bootstrap 到达前保留（只读窗口）。
    resync: Option<qaqh_ringing::RingingV2ResyncRequired>,
}

impl RingingV2SessionModel {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            server_epoch: None,
            next_seq: 0,
            initialized: false,
            pending_interactions: BTreeSet::new(),
            terminal_interactions: BTreeSet::new(),
            driver: None,
            resync: None,
        }
    }

    /// 生产路径已不再读取 server epoch（仅测试断言使用）。
    #[cfg(test)]
    pub fn server_epoch(&self) -> Option<&str> {
        self.server_epoch.as_deref()
    }

    /// 当前水位（下一帧应为该 seq）。仅测试断言使用。
    #[cfg(test)]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    #[cfg(test)]
    pub fn is_resync_pending(&self) -> bool {
        self.resync.is_some()
    }

    pub fn is_read_only(&self) -> bool {
        self.resync.is_some() && resync_is_read_only()
    }

    /// driver 席位只读视图（测试断言用；生产 UI 接线时再升级为公开访问器）。
    #[cfg(test)]
    pub fn driver_for_test(&self) -> Option<&DriverState> {
        self.driver.as_ref()
    }

    #[cfg(test)]
    pub fn pending_interactions(&self) -> impl Iterator<Item = &str> {
        self.pending_interactions.iter().map(String::as_str)
    }

    /// 用权威 typed bootstrap 原子替换旧状态。
    ///
    /// `state.seq` 是 exclusive 水位（B01）：快照已覆盖全部 `seq < state.seq`
    /// 的事件帧，之后的帧从 `state.seq` 起接。
    pub fn apply_bootstrap(&mut self, bootstrap: &SessionBootstrap) -> BootstrapOutcome {
        if bootstrap.session_id != self.session_id {
            return BootstrapOutcome::SessionMismatch;
        }
        if bootstrap.server_epoch.trim().is_empty() {
            return BootstrapOutcome::Invalid;
        }
        // 契约代际双保险：qaqh-client 在 bootstrap 响应处已硬校验（426 /
        // ClientError::Protocol），这里再断一次，防未来有人绕过 client 直拉。
        if bootstrap.session_contract_revision != SESSION_CONTRACT_REVISION {
            return BootstrapOutcome::Invalid;
        }

        // 同 epoch 水位倒退：较旧的并发 bootstrap 响应不得把状态回滚——
        // resync 等待期也不豁免（旧 in-flight 响应不能抢先解除 resync）。
        if self.initialized
            && self.server_epoch.as_deref() == Some(bootstrap.server_epoch.as_str())
            && bootstrap.state.seq < self.next_seq
        {
            return BootstrapOutcome::Stale;
        }

        self.server_epoch = Some(bootstrap.server_epoch.clone());
        self.next_seq = bootstrap.state.seq;
        self.initialized = true;
        // 权威 bootstrap 的 `interactions` 已是 daemon 维护的未决集合。
        self.pending_interactions = bootstrap
            .state
            .interactions
            .iter()
            .map(|interaction| interaction.interaction_id.as_ref().to_owned())
            .collect();
        self.terminal_interactions.clear();
        self.driver = bootstrap.state.driver.clone();
        self.resync = None;
        BootstrapOutcome::Applied
    }

    /// 标记 resync；保留旧 UI 状态，直到新 bootstrap 通过校验。
    pub fn begin_resync(&mut self, resync: &qaqh_ringing::RingingV2ResyncRequired) -> bool {
        if resync.session_id != self.session_id {
            return false;
        }
        self.resync = Some(resync.clone());
        true
    }

    /// 消费一条权威 v3 事件帧。
    ///
    /// 帧不携带 epoch / delivery / revision——单流单游标，epoch 与重连都由
    /// `qaqh-client` 的流状态机负责；这里只管水位与身份。
    pub fn apply_frame(&mut self, frame: &SessionEventFrame) -> ApplyOutcome {
        if self.resync.is_some() {
            return ApplyOutcome::ResyncPending;
        }
        if !self.initialized {
            return ApplyOutcome::NotInitialized;
        }
        if frame.seq < self.next_seq {
            return ApplyOutcome::Duplicate;
        }
        self.next_seq = frame.seq + 1;
        ApplyOutcome::Applied { seq: frame.seq }
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

    /// driver 席位交接（B02）。`None` = 席位清空（release 后）。
    pub fn apply_driver(&mut self, driver: Option<&DriverState>) -> DriverOutcome {
        let Some(incoming) = driver else {
            if self.driver.is_none() {
                return DriverOutcome::Duplicate;
            }
            self.driver = None;
            return DriverOutcome::Applied;
        };
        if let Some(current) = &self.driver {
            if incoming.epoch < current.epoch {
                return DriverOutcome::Stale;
            }
            if incoming.epoch == current.epoch && incoming.holder == current.holder {
                return DriverOutcome::Duplicate;
            }
            if incoming.epoch == current.epoch {
                return DriverOutcome::Conflict;
            }
        }
        self.driver = Some(incoming.clone());
        DriverOutcome::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_contract::state::TurnView;
use qaqh_ringing_contract::SessionState;

    const SESSION: &str = "session-1";
    const EPOCH: &str = "epoch-1";

    fn bootstrap(seq: u64) -> SessionBootstrap {
        SessionBootstrap {
            schema: "qaqh.ringing.session/v2".into(),
            version: qaqh_ringing::RINGING_V2_VERSION,
            session_contract_revision: SESSION_CONTRACT_REVISION.into(),
            server_epoch: EPOCH.into(),
            session_id: SESSION.into(),
            state: SessionState {
                seq,
                ..SessionState::default()
            },
        }
    }

    fn frame(seq: u64) -> SessionEventFrame {
        SessionEventFrame {
            seq,
            event: qaqh_ringing_contract::SessionEvent::SessionMetaChanged { title: None },
        }
    }

    fn model() -> RingingV2SessionModel {
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(
            model.apply_bootstrap(&bootstrap(7)),
            BootstrapOutcome::Applied
        );
        model
    }

    /// B01：bootstrap 后首帧恰好是 `state.seq`（相等边界必须处理）。
    #[test]
    fn first_frame_after_bootstrap_is_exactly_the_watermark() {
        let mut model = model();
        assert_eq!(model.next_seq(), 7);
        assert_eq!(model.apply_frame(&frame(7)), ApplyOutcome::Applied { seq: 7 });
        assert_eq!(model.apply_frame(&frame(8)), ApplyOutcome::Applied { seq: 8 });
    }

    /// B01：`seq < state.seq` 的帧已被快照覆盖，必须丢弃（重放不双计）。
    #[test]
    fn frames_below_the_watermark_are_duplicates() {
        let mut model = model();
        assert_eq!(model.apply_frame(&frame(3)), ApplyOutcome::Duplicate);
        assert_eq!(model.next_seq(), 7, "丢弃的帧不得推进水位");
        assert_eq!(model.apply_frame(&frame(7)), ApplyOutcome::Applied { seq: 7 });
        assert_eq!(model.apply_frame(&frame(7)), ApplyOutcome::Duplicate);
    }

    #[test]
    fn bootstrap_for_another_session_is_rejected() {
        let mut model = RingingV2SessionModel::new("session-2");
        assert_eq!(
            model.apply_bootstrap(&bootstrap(1)),
            BootstrapOutcome::SessionMismatch
        );
    }

    #[test]
    fn stale_same_epoch_bootstrap_cannot_rollback_the_watermark() {
        let mut model = model();
        assert_eq!(
            model.apply_bootstrap(&bootstrap(42)),
            BootstrapOutcome::Applied
        );
        assert_eq!(model.apply_bootstrap(&bootstrap(9)), BootstrapOutcome::Stale);
        assert_eq!(model.next_seq(), 42);
        // 新 epoch（daemon 重启）不算 stale：水位随新代际走。
        let mut fresh = bootstrap(1);
        fresh.server_epoch = "epoch-2".into();
        assert_eq!(model.apply_bootstrap(&fresh), BootstrapOutcome::Applied);
        assert_eq!(model.next_seq(), 1);
    }

    #[test]
    fn wrong_revision_is_invalid() {
        let mut model = RingingV2SessionModel::new(SESSION);
        let mut wrong = bootstrap(1);
        wrong.session_contract_revision = "ringing-session-1".into();
        assert_eq!(model.apply_bootstrap(&wrong), BootstrapOutcome::Invalid);
    }

    /// resync → 只读、事件被拒；新 bootstrap（同会话）解除并接上新水位。
    #[test]
    fn resync_freezes_writes_until_rebaseline() {
        let mut model = model();
        let resync = qaqh_ringing::RingingV2ResyncRequired {
            session_id: SESSION.into(),
            reason: "cursor_gap".into(),
        };
        assert!(model.begin_resync(&resync));
        assert!(model.is_read_only());
        assert_eq!(model.apply_frame(&frame(7)), ApplyOutcome::ResyncPending);
        assert_eq!(
            model.apply_bootstrap(&bootstrap(50)),
            BootstrapOutcome::Applied
        );
        assert!(!model.is_read_only());
        assert_eq!(model.next_seq(), 50);
        assert_eq!(model.apply_frame(&frame(50)), ApplyOutcome::Applied { seq: 50 });
    }

    #[test]
    fn resync_for_another_session_is_ignored() {
        let mut model = model();
        let resync = qaqh_ringing::RingingV2ResyncRequired {
            session_id: "other".into(),
            reason: "cursor_gap".into(),
        };
        assert!(!model.begin_resync(&resync));
        assert!(!model.is_read_only());
    }

    /// 交互身份：第一答复胜出、终态不得重开（R4 语义延续到 v3）。
    #[test]
    fn first_answer_wins_and_terminal_interaction_cannot_reopen() {
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
        // 未知 id 的 resolved 也进入终态集合（服务端权威），返回 Unknown。
        assert_eq!(
            model.apply_interaction_expired("i9"),
            InteractionOutcome::Unknown
        );
    }

    /// bootstrap 的未决交互按身份收编（含 contract `InteractionId` 新类型）。
    #[test]
    fn bootstrap_adopts_pending_interaction_identities() {
        let mut state = SessionState {
            seq: 7,
            ..SessionState::default()
        };
        state.interactions.push(qaqh_ringing_contract::state::InteractionView {
            interaction_id: qaqh_ringing_contract::fact::InteractionId::new(
                "int-1".to_string(),
            ),
            call_id: None,
            turn_id: None,
            kind: qaqh_ringing_contract::fact::InteractionKind::Ask,
            request: qaqh_ringing_contract::fact::ContentValue::Inline {
                text: "{\"input\":\"ASK v1\"}".into(),
            },
            choices: vec![],
            expires_at_ms: None,
            resolution: None,
        });
        let bootstrap = SessionBootstrap {
            schema: "qaqh.ringing.session/v2".into(),
            version: qaqh_ringing::RINGING_V2_VERSION,
            session_contract_revision: SESSION_CONTRACT_REVISION.into(),
            server_epoch: EPOCH.into(),
            session_id: SESSION.into(),
            state,
        };
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(model.apply_bootstrap(&bootstrap), BootstrapOutcome::Applied);
        assert_eq!(
            model.pending_interactions().collect::<Vec<_>>(),
            ["int-1"]
        );
    }

    /// driver 席位：epoch 单调、同 epoch 冲突、None 清席（B02）。
    #[test]
    fn driver_seat_handover_is_epoch_monotonic() {
        let mut model = model();
        assert!(model.driver.is_none());
        assert_eq!(
            model.apply_driver(Some(&DriverState {
                holder: Some("cs-1".into()),
                epoch: 4,
            })),
            DriverOutcome::Applied
        );
        assert_eq!(model.driver.as_ref().and_then(|d| d.holder.as_deref()), Some("cs-1"));
        // 同 epoch 同 holder = Duplicate。
        assert_eq!(
            model.apply_driver(Some(&DriverState {
                holder: Some("cs-1".into()),
                epoch: 4,
            })),
            DriverOutcome::Duplicate
        );
        // 同 epoch 换 holder = Conflict。
        assert_eq!(
            model.apply_driver(Some(&DriverState {
                holder: Some("cs-2".into()),
                epoch: 4,
            })),
            DriverOutcome::Conflict
        );
        // epoch 前进 = Applied。
        assert_eq!(
            model.apply_driver(Some(&DriverState {
                holder: Some("cs-2".into()),
                epoch: 5,
            })),
            DriverOutcome::Applied
        );
        // epoch 回退 = Stale。
        assert_eq!(
            model.apply_driver(Some(&DriverState {
                holder: Some("cs-1".into()),
                epoch: 4,
            })),
            DriverOutcome::Stale
        );
        assert_eq!(model.driver.as_ref().and_then(|d| d.holder.as_deref()), Some("cs-2"));
        // None 清席；重复 None = Duplicate。
        assert_eq!(model.apply_driver(None), DriverOutcome::Applied);
        assert!(model.driver.is_none());
        assert_eq!(model.apply_driver(None), DriverOutcome::Duplicate);
    }

    /// v3 的空 turn 列表可以零值往返（回归：bootstrap 消费不得要求多余字段）。
    #[test]
    fn bootstrap_with_empty_state_applies() {
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(
            model.apply_bootstrap(&bootstrap(0)),
            BootstrapOutcome::Applied
        );
        // 空快照水位 0：首帧 seq 0 也要被接受（相等边界）。
        assert_eq!(model.apply_frame(&frame(0)), ApplyOutcome::Applied { seq: 0 });
    }

    /// 未初始化（无 bootstrap）时事件帧必须被拒。
    #[test]
    fn frames_before_bootstrap_are_rejected() {
        let mut model = RingingV2SessionModel::new(SESSION);
        assert_eq!(model.apply_frame(&frame(0)), ApplyOutcome::NotInitialized);
    }

    /// `TurnView` 形状不被本模块依赖——只要编译过就说明契约字段兼容。
    #[test]
    fn contract_types_are_nameable() {
        let _ = TurnView {
            turn_id: qaqh_ringing_contract::fact::TurnId::new("t1".to_string()),
            input_id: None,
            mode: None,
            phase: None,
            outcome: None,
        };
    }
}
