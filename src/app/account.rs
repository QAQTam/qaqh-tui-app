//! 账号面（ChatGPT 套餐 / DeepSeek 账号）的 UI 状态与**纯逻辑**。
//!
//! ## 为什么单独一层
//!
//! 登录流程最容易坏的不是界面，而是**轮询**：等一个永远不会来的终态（静默挂死）、
//! 被新一次登录顶掉之后还继续等、超时了却不发 `cancel`（把下一次登录挡在
//! `connection_busy` 上）。这些判定全部写在纯函数里（clock 由调用方注入），
//! 于是可以在没有 daemon、没有浏览器的情况下被测试覆盖。
//!
//! 契约真源：`qaqh-backend/docs/spec-service-surface-siwc-dsaccount.md` §3；
//! 壳层已跑通的两条流程：桌面端 `src/lib/siwc.ts` / `src/dsaccount/flow.ts`。
//!
//! ## 本层不做的事
//!
//! - 不发请求（`account_ops` 负责）；
//! - 不碰浏览器（`browser` 负责）；
//! - 不解析 `Value` 之外的形状（typed DTO 全部来自 `qaqh_client`，本仓不手抄）。

use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_client::{
    DsAccountBalance, DsAccountSignInStatus, DsAccountStatus, DsFlowState, SiwcConnectionState,
    SiwcFlowState, SiwcProfiles, SiwcSignInStatus, SiwcStatus,
};

/// 轮询间隔（毫秒）。与桌面端一致：1s 一次，足以让「浏览器里刚点完授权」
/// 一秒内出现在终端上，又不至于把 daemon 打成轮询风暴。
pub const POLL_MS: u64 = 1_000;

/// 授权窗口失去响应后的宽限。daemon 侧 TTL 到期后状态可能还差最后一拍，
/// 直接判超时会让「刚好卡在边界」的成功登录被显示成失败。
pub const GRACE_MS: u64 = 15_000;

/// 当前时刻（epoch ms）。daemon 的 `expiresAtMs` / `expires_at_ms` 都是这个口径，
/// 用同口径比较，不引入本地单调时钟与墙上时钟的换算。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 两种账号档。取值与配置里的 `credential` 字符串一一对应——**不新造词**：
/// 设置页显示什么、协议里写什么，这里就是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    /// `credential = chatgpt_plan`：Sign in with ChatGPT，套餐额度直连。
    ChatGptPlan,
    /// `credential = deepseek_account`：DeepSeek 账号，余额/赠金计费。
    DeepSeekAccount,
}

/// 鼠标与键盘共用的账号操作；不携带屏幕坐标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAction {
    Login,
    Cancel,
    SignOut,
    Refresh,
    OpenAuthorize,
    CopyAuthorize,
    Usage,
    Balance,
    Verify,
    TopUp,
    SelectProfile(usize),
    Close,
}

impl AccountKind {
    pub fn from_credential(raw: &str) -> Option<Self> {
        match raw.trim() {
            "chatgpt_plan" => Some(Self::ChatGptPlan),
            "deepseek_account" => Some(Self::DeepSeekAccount),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::ChatGptPlan => "ChatGPT 套餐",
            Self::DeepSeekAccount => "DeepSeek 账号",
        }
    }

    /// 服务面方法名（`siwc.status` / `dsaccount.status` …）。
    ///
    /// 用格式串而不是各写一份常量表：方法名的**两段式**是协议事实
    /// （`service.rs` 的白名单就是 `siwc.*` / `dsaccount.*`），多一份手写表只会漂移。
    pub fn method(self, suffix: &str) -> String {
        let prefix = match self {
            Self::ChatGptPlan => "siwc",
            Self::DeepSeekAccount => "dsaccount",
        };
        format!("{prefix}.{suffix}")
    }
}

/// 登录流程的位置（UI 只读这个枚举决定文案与状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// 没有进行中的流程（也没有失败残留）。
    Idle,
    /// `sign_in.start` 在途（浏览器还没拿到 URL）。
    Starting,
    /// 已开工：等浏览器回调。`deadline_ms` 已含 [`GRACE_MS`]。
    Pending { id: String, deadline_ms: u64 },
    /// 本次流程成功（终态，停止轮询）。
    Done,
    /// 本次流程失败（终态，停止轮询）。
    Failed { code: String, message: String },
    /// 被新一次登录顶掉：**不是错误**，停止轮询即可（不要再 cancel 旧 id）。
    Superseded,
}

impl Flow {
    pub fn pending_id(&self) -> Option<&str> {
        match self {
            Flow::Pending { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Flow::Pending { .. })
    }
}

/// 模型目录条目（`siwc.models` / `dsaccount.models`）。
///
/// `value` 写进 `config.model`，`label` 只用于展示：后者可能带显示名，
/// 前者必须是上游 slug/id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub value: String,
    pub label: String,
}

/// 两种账号档的**最近一次快照**（应用级，跨 overlay 开关存活）。
///
/// 设置页的「账号」行、模型目录都从这里读；登录面板关闭不该让状态栏变回
/// 「未知」。
#[derive(Debug, Clone, Default)]
pub struct AccountBook {
    pub siwc: Option<SiwcStatus>,
    pub profiles: Option<SiwcProfiles>,
    pub ds: Option<DsAccountStatus>,
    pub balance: Option<DsAccountBalance>,
    pub models: Vec<ModelOption>,
    pub models_kind: Option<AccountKind>,
}

impl AccountBook {
    /// 设置页「账号」行的一行摘要。**没有快照就说没有**，不猜。
    pub fn summary(&self, kind: AccountKind) -> String {
        match kind {
            AccountKind::ChatGptPlan => match self.siwc.as_ref() {
                None => "读取中…".to_string(),
                Some(status) => siwc_view(status).headline,
            },
            AccountKind::DeepSeekAccount => match self.ds.as_ref() {
                None => "读取中…".to_string(),
                Some(status) => ds_view(status).headline,
            },
        }
    }

    /// 该档的模型目录（**只在该档的目录已取回时**返回，避免把套餐档的目录
    /// 当成账号档的候选）。
    pub fn models_for(&self, kind: AccountKind) -> &[ModelOption] {
        if self.models_kind == Some(kind) {
            &self.models
        } else {
            &[]
        }
    }

    pub fn store_models(&mut self, kind: AccountKind, models: Vec<ModelOption>) {
        self.models = models;
        self.models_kind = Some(kind);
    }
}

/// 账号面板的 UI 状态（随 `Overlay::Account` 持有）。
#[derive(Debug, Clone)]
pub struct AccountState {
    pub kind: AccountKind,
    pub flow: Flow,
    /// 在途动作的可读标签（防止同一动作双发，也给渲染层一个「忙什么」）。
    pub busy: Option<&'static str>,
    /// 最近一次失败（面板顶部红字；成功后清空）。
    pub error: Option<String>,
    /// 最近一次成功提示（如「已登出」）。
    pub notice: Option<String>,
    /// profile 列表选中（仅 ChatGPT 套餐档有意义）。
    pub selected: usize,
    pub scroll: usize,
    /// 上次轮询时刻（epoch ms）。
    pub last_poll_ms: u64,
    /// 本次流程的授权页 URL（用户关掉浏览器标签页后按 `o` 重开）。
    ///
    /// 只在这里留一份**展示用**的 URL：token 与 code 从不经过壳层
    /// （投影里就没有这些字段，spec §3.5）。
    pub authorize_url: Option<String>,
    /// start 请求尚未返回 id 时关闭：保留面板，拿到 id 后取消，不打开浏览器。
    pub close_when_started: bool,
}

impl AccountState {
    /// 绘制禁用态与事件执行使用同一个判据，旧帧点击也必须重新校验。
    pub fn action_enabled(&self, book: &AccountBook, action: AccountAction) -> bool {
        let running = self.flow.is_pending() || self.flow == Flow::Starting;
        let available = self.busy.is_none();
        let signed_in = match self.kind {
            AccountKind::ChatGptPlan => book
                .siwc
                .as_ref()
                .is_some_and(|status| status.status == SiwcConnectionState::Connected),
            AccountKind::DeepSeekAccount => book.ds.as_ref().is_some_and(|status| status.signed_in),
        };
        match action {
            AccountAction::Login => available && !running,
            AccountAction::Cancel => available && self.flow.is_pending(),
            AccountAction::SignOut => available && !running && signed_in,
            AccountAction::Refresh => available,
            AccountAction::OpenAuthorize | AccountAction::CopyAuthorize => {
                self.flow.is_pending()
                    && self
                        .authorize_url
                        .as_ref()
                        .is_some_and(|url| !url.is_empty())
            }
            AccountAction::Usage => match self.kind {
                AccountKind::ChatGptPlan => {
                    book.siwc.as_ref().is_some_and(|s| !s.usage_url.is_empty())
                }
                AccountKind::DeepSeekAccount => {
                    book.ds.as_ref().is_some_and(|s| !s.usage_url.is_empty())
                }
            },
            AccountAction::Balance | AccountAction::Verify => {
                self.kind == AccountKind::DeepSeekAccount && available && !running && signed_in
            }
            AccountAction::TopUp => {
                self.kind == AccountKind::DeepSeekAccount
                    && book.ds.as_ref().is_some_and(|s| !s.top_up_url.is_empty())
            }
            AccountAction::SelectProfile(index) => {
                self.kind == AccountKind::ChatGptPlan
                    && available
                    && !running
                    && book
                        .profiles
                        .as_ref()
                        .and_then(|p| p.profiles.get(index))
                        .is_some_and(|profile| !profile.pending)
            }
            AccountAction::Close => true,
        }
    }

    pub fn new(kind: AccountKind) -> Self {
        Self {
            kind,
            flow: Flow::Idle,
            busy: None,
            error: None,
            notice: None,
            selected: 0,
            scroll: 0,
            last_poll_ms: 0,
            authorize_url: None,
            close_when_started: false,
        }
    }

    /// 流程开工成功：记下 id 与截止时刻（`deadline_ms` 已经是 epoch ms）。
    pub fn started(&mut self, id: String, deadline_ms: u64, now: u64) {
        self.busy = None;
        self.error = None;
        self.notice = None;
        self.flow = Flow::Pending {
            id,
            deadline_ms: deadline_ms.saturating_add(GRACE_MS),
        };
        // 立刻允许下一次 poll：开工响应回来时 daemon 多半已经在等回调，
        // 没必要再白等一个间隔。
        self.last_poll_ms = now.saturating_sub(POLL_MS);
    }
}

/// 一次 tick 该对轮询做什么（纯函数，便于把「静默挂死」钉成测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollAction {
    None,
    /// 该发一次 `*.sign_in.status`（带流程 id）。
    Poll {
        id: String,
    },
    /// 超过 `deadline + GRACE`：**先 cancel**（同一路径），再落失败。
    ///
    /// 「超时也要 cancel」是协议硬要求（spec §3.1）：留一个进行中的流程会把下一次
    /// `sign_in.start` 挡在 `connection_busy` 上（siwc），或让它先收掉旧尝试
    /// （dsaccount，语义不同但同样要清）。
    Timeout {
        id: String,
    },
}

pub fn next_poll(flow: &Flow, last_poll_ms: u64, now: u64) -> PollAction {
    let Flow::Pending { id, deadline_ms } = flow else {
        return PollAction::None;
    };
    if now > *deadline_ms {
        return PollAction::Timeout { id: id.clone() };
    }
    if now.saturating_sub(last_poll_ms) >= POLL_MS {
        return PollAction::Poll { id: id.clone() };
    }
    PollAction::None
}

/// 轮询结果对流程的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowOutcome {
    /// 仍在进行（继续轮询）。
    Continue,
    Completed {
        profile_id: Option<String>,
    },
    Failed {
        code: String,
        message: String,
    },
    /// 被新一次登录顶掉：停止轮询，**不**当作失败展示。
    Superseded,
}

/// `siwc.sign_in.status` 的结论。
///
/// 防御性判据：`flow_id` 与本地不一致时按 superseded 处理（协议说 daemon 会直接
/// 报 `profile_not_found`，但「形状对不上就当成功」是更坏的兜底）。
pub fn apply_siwc_flow(flow: &mut Flow, mine: &str, status: &SiwcSignInStatus) -> FlowOutcome {
    if status.flow_id != mine {
        *flow = Flow::Superseded;
        return FlowOutcome::Superseded;
    }
    match status.state {
        SiwcFlowState::Pending => {
            if let Some(expires_at_ms) = status.expires_at_ms {
                *flow = Flow::Pending {
                    id: mine.to_string(),
                    deadline_ms: expires_at_ms.saturating_add(GRACE_MS),
                };
            }
            FlowOutcome::Continue
        }
        SiwcFlowState::Completed => {
            *flow = Flow::Done;
            FlowOutcome::Completed {
                profile_id: status.profile_id.clone(),
            }
        }
        SiwcFlowState::Failed => {
            let (code, message) = status
                .error
                .as_ref()
                .map(|e| (e.code.clone(), e.message.clone()))
                .unwrap_or_else(|| ("sign_in_failed".into(), "登录失败（未给原因）".into()));
            *flow = Flow::Failed {
                code: code.clone(),
                message: message.clone(),
            };
            FlowOutcome::Failed { code, message }
        }
    }
}

/// `dsaccount.sign_in.status` 的结论。
///
/// 与 siwc 的两处差别（spec §3）：状态查询**不带参数**，所以「被顶掉」靠
/// `attempt_id` 比对识别；终态多一个 `cancelled`。
pub fn apply_ds_flow(flow: &mut Flow, mine: &str, status: &DsAccountSignInStatus) -> FlowOutcome {
    if let Some(attempt_id) = status.attempt_id.as_deref()
        && attempt_id != mine
    {
        *flow = Flow::Superseded;
        return FlowOutcome::Superseded;
    }
    match status.state {
        DsFlowState::Authorizing | DsFlowState::Exchanging => FlowOutcome::Continue,
        DsFlowState::SignedIn => {
            *flow = Flow::Done;
            FlowOutcome::Completed { profile_id: None }
        }
        DsFlowState::Failed => {
            let message = status
                .error
                .clone()
                .unwrap_or_else(|| "登录失败（未给原因）".into());
            *flow = Flow::Failed {
                code: "sign_in_failed".into(),
                message: message.clone(),
            };
            FlowOutcome::Failed {
                code: "sign_in_failed".into(),
                message,
            }
        }
        DsFlowState::Cancelled => {
            *flow = Flow::Failed {
                code: "sign_in_cancelled".into(),
                message: "登录已取消".into(),
            };
            FlowOutcome::Failed {
                code: "sign_in_cancelled".into(),
                message: "登录已取消".into(),
            }
        }
        // `idle` + 没有 attempt id = 本机已经没有任何尝试：我们这一次被顶掉了
        // （daemon 会先收掉旧尝试再开新的）。继续轮询只会等一个不会来的终态。
        DsFlowState::Idle => {
            if status.attempt_id.is_none() {
                *flow = Flow::Superseded;
                FlowOutcome::Superseded
            } else {
                FlowOutcome::Continue
            }
        }
    }
}

/// 服务面报错是否意味着「我不是当前流程了」。
///
/// siwc 的 `sign_in.status` 用 `profile_not_found` 表达这件事（daemon 把
/// `SiwcError` 的 Display 直接放进 `action_failed` 的 message，形如
/// `profile_not_found: 登录流程 id 不匹配…`）。dsaccount 不需要这条：它靠
/// `attempt_id` 比对就能认出 superseded。
pub fn is_superseded_error(kind: AccountKind, message: &str) -> bool {
    match kind {
        AccountKind::ChatGptPlan => message.contains("profile_not_found"),
        AccountKind::DeepSeekAccount => false,
    }
}

/// ChatGPT 套餐的状态视图（品牌口径：`qaqh-desktop-app/docs/siwc-brand-posture.md` §UI 规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiwcView {
    pub headline: String,
    pub detail: Option<String>,
    /// 只有 `connected && sharing` 才是**计费事实陈述**意义上的「在用套餐」。
    pub plan_badge: bool,
    pub action: SiwcAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiwcAction {
    /// 未登录 / 需要重登：主行动是继续登录。
    SignIn,
    /// 流程在进行：主行动是取消。
    Cancel,
    /// 已登录：主行动是「管理用量」（深链来自后端）。
    ManageUsage,
    /// 可重试的暂时故障：稍后重试，**不动凭据**。
    RetryLater,
}

pub fn siwc_view(status: &SiwcStatus) -> SiwcView {
    let retryable = status.error.as_ref().is_some_and(|e| e.retryable);
    let detail = status.error.as_ref().map(|e| {
        if e.retryable {
            format!("{code}（稍后重试，凭据保留）", code = e.code)
        } else {
            format!("{code}: {message}", code = e.code, message = e.message)
        }
    });
    match status.status {
        SiwcConnectionState::Disconnected => SiwcView {
            headline: "未登录".into(),
            detail,
            plan_badge: false,
            action: SiwcAction::SignIn,
        },
        // 「连接中」不得出现套餐标识（brand §UI 规则）。
        SiwcConnectionState::Connecting => SiwcView {
            headline: "浏览器中完成授权…".into(),
            detail,
            plan_badge: false,
            action: SiwcAction::Cancel,
        },
        SiwcConnectionState::Connected if status.sharing => {
            let who = match (status.label.as_deref(), status.email.as_deref()) {
                (Some(label), Some(email)) => format!("{label} ({email})"),
                (Some(label), None) => label.to_string(),
                (None, Some(email)) => email.to_string(),
                (None, None) => "已连接".to_string(),
            };
            SiwcView {
                headline: who,
                detail: Some("套餐用量可用".into()),
                plan_badge: true,
                action: SiwcAction::ManageUsage,
            }
        }
        // 登录了但没授予直连套餐的 scope：**真实状态**，不是「已连接」。
        SiwcConnectionState::Connected => SiwcView {
            headline: "已登录 · 套餐未启用".into(),
            detail: Some("在 ChatGPT 设置里授权套餐用量后即可直连".into()),
            plan_badge: false,
            action: SiwcAction::ManageUsage,
        },
        SiwcConnectionState::ReauthRequired => SiwcView {
            headline: "需要重新登录".into(),
            detail,
            plan_badge: false,
            action: if retryable {
                SiwcAction::RetryLater
            } else {
                SiwcAction::SignIn
            },
        },
    }
}

/// DeepSeek 账号的状态视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsView {
    pub headline: String,
    pub detail: Option<String>,
    pub signed_in: bool,
    /// 本地凭据读取失败：要引导重登，**不能**显示成干净的空态。
    pub storage_broken: bool,
}

pub fn ds_view(status: &DsAccountStatus) -> DsView {
    if let Some(error) = status.storage_error.as_ref() {
        return DsView {
            headline: "本地凭据读取失败".into(),
            detail: Some(format!("{error} · 请重新登录")),
            signed_in: false,
            storage_broken: true,
        };
    }
    match status.account.as_ref() {
        Some(account) => {
            let who = if account.label.is_empty() {
                account.user_id.clone()
            } else {
                format!("{} ({})", account.label, account.user_id)
            };
            let mut bits = Vec::new();
            if let Some(provider) = account.provider.as_deref() {
                bits.push(provider.to_string());
            }
            if let Some(currency) = account.currency.as_deref() {
                bits.push(currency.to_string());
            }
            bits.push(account.inference_origin.clone());
            DsView {
                headline: who,
                detail: Some(bits.join(" · ")),
                signed_in: status.signed_in,
                storage_broken: false,
            }
        }
        None => DsView {
            headline: "未登录".into(),
            detail: Some(format!("推理端点 {}", status.inference_origin)),
            signed_in: false,
            storage_broken: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_action_availability_tracks_flow_and_provider() {
        use AccountAction as A;
        let book = AccountBook::default();
        for kind in [AccountKind::ChatGptPlan, AccountKind::DeepSeekAccount] {
            let mut state = AccountState::new(kind);
            assert!(state.action_enabled(&book, A::Login));
            for action in [
                A::Cancel,
                A::SignOut,
                A::SelectProfile(0),
                A::Balance,
                A::Verify,
                A::OpenAuthorize,
                A::CopyAuthorize,
                A::Usage,
                A::TopUp,
            ] {
                assert!(!state.action_enabled(&book, action), "{kind:?}: {action:?}");
            }
            state.flow = Flow::Pending {
                id: "mine".into(),
                deadline_ms: u64::MAX,
            };
            state.authorize_url = Some("https://example.com/auth".into());
            assert!(!state.action_enabled(&book, A::Login));
            for action in [A::Cancel, A::OpenAuthorize, A::CopyAuthorize, A::Close] {
                assert!(state.action_enabled(&book, action));
            }
            state.busy = Some("取消登录");
            assert!(!state.action_enabled(&book, A::Cancel));
            assert!(state.action_enabled(&book, A::Close));
            state.flow = Flow::Done;
            assert!(
                !state.action_enabled(&book, A::OpenAuthorize),
                "完成后旧授权页不能重开"
            );
        }
    }
    use qaqh_client::{DsAccountFlow, DsAccountProfile, DsRevokeOutcome, ServiceError};
    use serde_json::json;

    fn siwc_status(value: serde_json::Value) -> SiwcStatus {
        serde_json::from_value(value).expect("siwc.status fixture")
    }

    fn ds_status(value: serde_json::Value) -> DsAccountStatus {
        serde_json::from_value(value).expect("dsaccount.status fixture")
    }

    fn ds_flow_value() -> serde_json::Value {
        json!({
            "attemptId": null, "state": "idle",
            "authorizeUrl": null, "expiresAtMs": 0, "error": null
        })
    }

    fn ds_profile() -> DsAccountProfile {
        DsAccountProfile {
            user_id: "u_1".into(),
            label: "工作号".into(),
            provider: Some("WECHAT".into()),
            currency: Some("CNY".into()),
            inference_origin: "https://api.deepseek.com".into(),
            saved_at_ms: 1_728_000_000_000,
            has_credential: true,
        }
    }

    #[test]
    fn credential_names_map_to_kinds_and_back() {
        assert_eq!(
            AccountKind::from_credential("chatgpt_plan"),
            Some(AccountKind::ChatGptPlan)
        );
        assert_eq!(
            AccountKind::from_credential("deepseek_account"),
            Some(AccountKind::DeepSeekAccount)
        );
        assert_eq!(AccountKind::from_credential("api_key"), None);
        assert_eq!(
            AccountKind::ChatGptPlan.method("sign_in.status"),
            "siwc.sign_in.status"
        );
        assert_eq!(
            AccountKind::DeepSeekAccount.method("sign_in.start"),
            "dsaccount.sign_in.start"
        );
    }

    /// 品牌口径的五条：`sharing=false` 不得出套餐标识；`connecting` 不得出；
    /// 可重试错误保留凭据（不引导重登）。
    #[test]
    fn siwc_copy_follows_the_brand_posture() {
        let disconnected = siwc_status(json!({
            "status": "disconnected", "sharing": false, "profiles": 0,
            "usageUrl": "u", "apiBase": "a"
        }));
        let view = siwc_view(&disconnected);
        assert_eq!(view.headline, "未登录");
        assert!(!view.plan_badge);
        assert_eq!(view.action, SiwcAction::SignIn);

        let connecting = siwc_status(json!({
            "status": "connecting", "sharing": false, "profiles": 1,
            "usageUrl": "u", "apiBase": "a"
        }));
        let view = siwc_view(&connecting);
        assert!(!view.plan_badge, "connecting 期间不得显示套餐标识");
        assert_eq!(view.action, SiwcAction::Cancel);
        assert!(view.headline.contains("浏览器"));

        let sharing = siwc_status(json!({
            "status": "connected", "sharing": true, "profiles": 1,
            "usageUrl": "u", "apiBase": "a",
            "profileId": "p_1", "label": "工作", "email": "a@b.c"
        }));
        let view = siwc_view(&sharing);
        assert!(view.plan_badge, "connected+sharing 才是计费事实陈述");
        assert_eq!(view.headline, "工作 (a@b.c)");
        assert_eq!(view.action, SiwcAction::ManageUsage);

        let no_sharing = siwc_status(json!({
            "status": "connected", "sharing": false, "profiles": 1,
            "usageUrl": "u", "apiBase": "a", "profileId": "p_1"
        }));
        let view = siwc_view(&no_sharing);
        assert!(!view.plan_badge, "已登录但没授权套餐时不得冒充「在用套餐」");
        assert_eq!(view.headline, "已登录 · 套餐未启用");

        let reauth = siwc_status(json!({
            "status": "reauth_required", "sharing": false, "profiles": 1,
            "usageUrl": "u", "apiBase": "a",
            "error": {
                "code": "sign_in_required", "message": "需要重新登录",
                "retryable": false, "requiresReauth": true
            }
        }));
        let view = siwc_view(&reauth);
        assert_eq!(view.headline, "需要重新登录");
        assert_eq!(view.action, SiwcAction::SignIn);
        assert!(!view.plan_badge);

        // 可重试：稍后重试 + 保留凭据（不得把用户推去重登）。
        let retryable = siwc_status(json!({
            "status": "reauth_required", "sharing": false, "profiles": 1,
            "usageUrl": "u", "apiBase": "a",
            "error": {
                "code": "transport_error", "message": "网络中断",
                "retryable": true, "requiresReauth": false
            }
        }));
        let view = siwc_view(&retryable);
        assert_eq!(view.action, SiwcAction::RetryLater);
        assert!(
            view.detail
                .as_deref()
                .unwrap_or_default()
                .contains("凭据保留")
        );
    }

    #[test]
    fn ds_copy_distinguishes_broken_storage_from_signed_out() {
        let broken = ds_status(json!({
            "signedIn": false,
            "account": null,
            "storageError": "凭据文件损坏",
            "inferenceOrigin": "https://api.deepseek.com",
            "usageUrl": "u", "topUpUrl": "t",
            "flow": ds_flow_value(),
            "revoke": { "state": "idle", "attempts": 0, "message": null }
        }));
        let view = ds_view(&broken);
        assert!(view.storage_broken);
        assert!(!view.signed_in);
        assert_eq!(view.headline, "本地凭据读取失败");

        let signed = ds_status(json!({
            "signedIn": true,
            "account": {
                "user_id": "u_1", "label": "工作号", "provider": "WECHAT",
                "currency": "CNY", "inference_origin": "https://api.deepseek.com",
                "saved_at_ms": 1728000000000u64, "has_credential": true
            },
            "storageError": null,
            "inferenceOrigin": "https://api.deepseek.com",
            "usageUrl": "u", "topUpUrl": "t",
            "flow": ds_flow_value(),
            "revoke": { "state": "idle", "attempts": 0, "message": null }
        }));
        let view = ds_view(&signed);
        assert!(view.signed_in);
        assert!(view.headline.contains("u_1"));
        assert!(
            view.detail
                .as_deref()
                .unwrap_or_default()
                .contains("WECHAT")
        );
    }

    /// 轮询节奏：间隔未到不重复发；超过 deadline+GRACE 先 cancel。
    #[test]
    fn polling_waits_for_the_interval_and_times_out_into_a_cancel() {
        let mut flow = Flow::Idle;
        assert_eq!(next_poll(&flow, 0, 10_000), PollAction::None);

        flow = Flow::Pending {
            id: "f_1".into(),
            deadline_ms: 60_000,
        };
        assert_eq!(
            next_poll(&flow, 10_000, 10_500),
            PollAction::None,
            "间隔 1s 内不得重复轮询"
        );
        assert_eq!(
            next_poll(&flow, 10_000, 11_000),
            PollAction::Poll { id: "f_1".into() }
        );
        assert_eq!(
            next_poll(&flow, 59_000, 60_000),
            PollAction::Poll { id: "f_1".into() },
            "到点那一拍仍然是轮询（边界不算超时）"
        );
        assert_eq!(
            next_poll(&flow, 59_000, 60_001),
            PollAction::Timeout { id: "f_1".into() },
            "超时必须走取消路径，否则下一次登录撞 connection_busy"
        );
    }

    #[test]
    fn siwc_status_updates_the_deadline_and_terminates_only_on_real_terminal_states() {
        let mut flow = Flow::Pending {
            id: "f_1".into(),
            deadline_ms: 10_000,
        };
        let pending: SiwcSignInStatus = serde_json::from_value(json!({
            "flowId": "f_1", "state": "pending", "expiresAtMs": 99_000u64
        }))
        .expect("pending");
        assert_eq!(
            apply_siwc_flow(&mut flow, "f_1", &pending),
            FlowOutcome::Continue
        );
        assert_eq!(
            flow,
            Flow::Pending {
                id: "f_1".into(),
                deadline_ms: 99_000 + GRACE_MS
            }
        );

        let completed: SiwcSignInStatus = serde_json::from_value(json!({
            "flowId": "f_1", "state": "completed", "profileId": "p_1"
        }))
        .expect("completed");
        assert_eq!(
            apply_siwc_flow(&mut flow, "f_1", &completed),
            FlowOutcome::Completed {
                profile_id: Some("p_1".into())
            }
        );
        assert_eq!(flow, Flow::Done);

        // flow_id 对不上 = 被顶掉，而不是「这次登录失败」。
        let mut flow = Flow::Pending {
            id: "f_1".into(),
            deadline_ms: 10_000,
        };
        let other: SiwcSignInStatus = serde_json::from_value(json!({
            "flowId": "f_2", "state": "pending", "expiresAtMs": 1u64
        }))
        .expect("other");
        assert_eq!(
            apply_siwc_flow(&mut flow, "f_1", &other),
            FlowOutcome::Superseded
        );
        assert_eq!(flow, Flow::Superseded);
    }

    #[test]
    fn ds_status_detects_superseded_by_attempt_id_and_terminal_cancel() {
        let mut flow = Flow::Pending {
            id: "a_1".into(),
            deadline_ms: 10_000,
        };
        let other: DsAccountSignInStatus = serde_json::from_value(json!({
            "attempt_id": "a_2", "state": "authorizing"
        }))
        .expect("other attempt");
        assert_eq!(
            apply_ds_flow(&mut flow, "a_1", &other),
            FlowOutcome::Superseded
        );

        let mut flow = Flow::Pending {
            id: "a_1".into(),
            deadline_ms: 10_000,
        };
        let idle: DsAccountSignInStatus = serde_json::from_value(json!({
            "attempt_id": null, "state": "idle"
        }))
        .expect("idle");
        assert_eq!(
            apply_ds_flow(&mut flow, "a_1", &idle),
            FlowOutcome::Superseded,
            "本机已无尝试 = 被收掉了，继续轮询只会挂死"
        );

        let mut flow = Flow::Pending {
            id: "a_1".into(),
            deadline_ms: 10_000,
        };
        let cancelled: DsAccountSignInStatus = serde_json::from_value(json!({
            "attempt_id": "a_1", "state": "cancelled", "error": null
        }))
        .expect("cancelled");
        assert!(matches!(
            apply_ds_flow(&mut flow, "a_1", &cancelled),
            FlowOutcome::Failed { code, .. } if code == "sign_in_cancelled"
        ));

        let signed: DsAccountSignInStatus = serde_json::from_value(json!({
            "attempt_id": "a_1", "state": "signed-in"
        }))
        .expect("signed-in");
        let mut flow = Flow::Pending {
            id: "a_1".into(),
            deadline_ms: 10_000,
        };
        assert_eq!(
            apply_ds_flow(&mut flow, "a_1", &signed),
            FlowOutcome::Completed { profile_id: None }
        );
        assert_eq!(flow, Flow::Done);
    }

    #[test]
    fn superseded_error_is_only_recognised_for_siwc() {
        assert!(is_superseded_error(
            AccountKind::ChatGptPlan,
            "profile_not_found: 登录流程 id 不匹配（可能已被新的登录取代）"
        ));
        assert!(!is_superseded_error(
            AccountKind::ChatGptPlan,
            "transport_error: 网络中断"
        ));
        assert!(!is_superseded_error(
            AccountKind::DeepSeekAccount,
            "profile_not_found"
        ));
    }

    /// 模型目录必须按档隔离：套餐档的 slug 不得出现在账号档的选择器里。
    #[test]
    fn model_catalogue_is_scoped_to_its_account_kind() {
        let mut book = AccountBook::default();
        assert!(book.models_for(AccountKind::ChatGptPlan).is_empty());
        book.store_models(
            AccountKind::ChatGptPlan,
            vec![ModelOption {
                value: "gpt-5".into(),
                label: "GPT-5".into(),
            }],
        );
        assert_eq!(book.models_for(AccountKind::ChatGptPlan).len(), 1);
        assert!(
            book.models_for(AccountKind::DeepSeekAccount).is_empty(),
            "换档必须重取：不能拿另一档的目录充数"
        );
    }

    #[test]
    fn book_summary_says_unknown_instead_of_guessing() {
        let book = AccountBook::default();
        assert_eq!(
            book.summary(AccountKind::ChatGptPlan),
            "读取中…",
            "没有快照时不许编一个「未登录」"
        );
        let book = AccountBook {
            ds: Some(ds_status(json!({
                "signedIn": false, "account": null, "storageError": null,
                "inferenceOrigin": "https://api.deepseek.com",
                "usageUrl": "u", "topUpUrl": "t",
                "flow": ds_flow_value(),
                "revoke": { "state": "idle", "attempts": 0, "message": null }
            }))),
            ..Default::default()
        };
        assert_eq!(book.summary(AccountKind::DeepSeekAccount), "未登录");
    }

    /// 开工后立刻允许第一次轮询（否则用户要多等一个间隔才看到状态）。
    #[test]
    fn started_flow_polls_immediately_and_carries_the_grace() {
        let mut st = AccountState::new(AccountKind::ChatGptPlan);
        st.started("f_1".into(), 50_000, 10_000);
        assert_eq!(
            st.flow,
            Flow::Pending {
                id: "f_1".into(),
                deadline_ms: 50_000 + GRACE_MS
            }
        );
        assert_eq!(
            next_poll(&st.flow, st.last_poll_ms, 10_000),
            PollAction::Poll { id: "f_1".into() }
        );
    }

    /// fixture 只用一次，避免「未使用」警告同时把形状钉在编译期。
    #[test]
    fn ds_fixtures_match_the_authoritative_dtos() {
        let flow: DsAccountFlow = serde_json::from_value(ds_flow_value()).expect("flow");
        assert_eq!(flow.state, DsFlowState::Idle);
        let revoke: DsRevokeOutcome = serde_json::from_value(json!({
            "state": "idle", "attempts": 0, "message": null
        }))
        .expect("revoke");
        assert_eq!(revoke.state, "idle");
        let error = ServiceError {
            code: "sign_in_required".into(),
            message: "需要重新登录".into(),
            retryable: false,
            requires_reauth: true,
            request_id: None,
        };
        assert!(error.requires_reauth);
        assert_eq!(ds_profile().user_id, "u_1");
    }
}
