//! 账号面的服务调用（`Client::service_call`）与结果落地。
//!
//! 这一层只做三件事：发请求、把 `Value` 反序列化成**权威 DTO**、把结论写回
//! [`account::AccountState`] / [`account::AccountBook`]。所有判定（何时轮询、
//! 何时算被顶掉、何时该 cancel）都在 `account` 模块的纯函数里。
//!
//! ## 不手抄响应形状
//!
//! `qaqh_client::service_types::*` 是 daemon 投影的 typed 镜像（后端 2026-10-19
//! 交付）。本仓直接 `serde_json::from_value::<SiwcStatus>(..)`——解析失败就报错，
//! 绝不「猜字段名」。手抄一份结构正是桌面端已经付过一次的代价。

use serde_json::{Value, json};

use super::account::{self, AccountKind, AccountState, Flow, FlowOutcome, ModelOption, PollAction};
use super::*;

/// 一次服务调用的用途（结果回来时的分派键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountOp {
    Status,
    Profiles,
    Models,
    SignInStart,
    SignInPoll { id: String },
    SignInCancel,
    SignOut,
    SelectProfile,
    Balance,
    Verify,
}

/// 账号面后台结果。
#[derive(Debug)]
pub struct AccountResult {
    pub kind: AccountKind,
    pub op: AccountOp,
    pub result: Result<Value, String>,
}

impl App {
    /// 账号卡片的共享动作入口；鼠标和快捷键都在这里重新检查可用性。
    pub fn account_action(&mut self, action: account::AccountAction) {
        use account::AccountAction as A;
        let Some(Overlay::Account(st)) = self.overlays.last() else {
            return;
        };
        if !st.action_enabled(&self.account, action) {
            return;
        }
        let kind = st.kind;
        let authorize_url = st.authorize_url.clone();
        match action {
            A::Login => self.start_account_sign_in(kind),
            A::Cancel => self.cancel_account_sign_in(kind),
            A::SignOut => self.sign_out_account(kind),
            A::Refresh => self.refresh_account(kind),
            A::OpenAuthorize => {
                if let Some(url) = authorize_url {
                    self.open_account_link(url);
                }
            }
            A::CopyAuthorize => {
                if let Some(url) = authorize_url {
                    match crate::terminal::clipboard::copy_osc52(&url) {
                        Ok(()) => self.toast(
                            NoticeLevel::Info,
                            "已发送授权链接到终端剪贴板（需终端支持 OSC 52）",
                        ),
                        Err(error) => self.toast(NoticeLevel::Error, format!("复制失败：{error}")),
                    }
                }
            }
            A::Usage => {
                let url = match kind {
                    AccountKind::ChatGptPlan => {
                        self.account.siwc.as_ref().map(|s| s.usage_url.clone())
                    }
                    AccountKind::DeepSeekAccount => {
                        self.account.ds.as_ref().map(|s| s.usage_url.clone())
                    }
                };
                if let Some(url) = url {
                    self.open_account_link(url);
                }
            }
            A::Balance => self.refresh_account_balance(),
            A::Verify => self.verify_account_credential(),
            A::TopUp => {
                if let Some(status) = &self.account.ds {
                    self.open_account_link(status.top_up_url.clone());
                }
            }
            A::SelectProfile(index) => {
                if let Some(profile) = self
                    .account
                    .profiles
                    .as_ref()
                    .and_then(|p| p.profiles.get(index))
                {
                    let id = profile.id.clone();
                    if let Some(st) = self.account_overlay_mut(kind) {
                        st.selected = index;
                    }
                    self.select_account_profile(id);
                }
            }
            A::Close => {
                if st.flow == Flow::Starting {
                    if let Some(st) = self.account_overlay_mut(kind) {
                        st.close_when_started = true;
                        st.notice = Some("正在等待流程 id，收到后将取消并返回".into());
                    }
                    self.redraw_requested = true;
                    return;
                }
                if st.flow.is_pending() {
                    self.cancel_account_sign_in(kind);
                }
                self.overlays.pop();
            }
        }
        self.redraw_requested = true;
    }

    /// 打开账号面板（`kind` 由设置页按**当前生效的 credential** 给出）。
    pub fn open_account(&mut self, kind: AccountKind) {
        match self.overlays.last() {
            Some(Overlay::Account(st)) if st.kind == kind => {}
            Some(Overlay::Account(_)) => {
                self.replace_overlay(Overlay::Account(AccountState::new(kind)));
            }
            _ => self
                .overlays
                .push(Overlay::Account(AccountState::new(kind))),
        }
        self.refresh_account(kind);
    }

    /// 拉一次状态（siwc 另拉 profile 列表与模型目录，dsaccount 登录后拉余额）。
    pub fn refresh_account(&mut self, kind: AccountKind) {
        self.spawn_account_status(kind);
        match kind {
            AccountKind::ChatGptPlan => {
                self.spawn_account_profiles(kind);
                self.spawn_account_models(kind);
            }
            AccountKind::DeepSeekAccount => {
                // 余额只在已登录时才有意义；状态还没回来时先不猜。
                if self
                    .account
                    .ds
                    .as_ref()
                    .is_some_and(|status| status.signed_in)
                {
                    self.spawn_account_balance(kind);
                }
            }
        }
    }

    /// 开工登录：`**`.sign_in.start` → 记下流程 id → 用系统浏览器打开授权页。
    pub fn start_account_sign_in(&mut self, kind: AccountKind) {
        if let Some(st) = self.account_overlay_mut(kind)
            && (st.flow.is_pending() || st.flow == Flow::Starting)
        {
            self.toast(
                NoticeLevel::Info,
                "已有一次登录在进行中（先按 c 取消）".to_string(),
            );
            return;
        }
        if let Some(st) = self.account_overlay_mut(kind) {
            st.busy = Some("发起登录");
            st.error = None;
            st.notice = None;
            st.flow = Flow::Starting;
            st.close_when_started = false;
            st.authorize_url = None;
        }
        self.spawn_account(kind, AccountOp::SignInStart, "sign_in.start", json!({}));
    }

    /// 取消进行中的登录。**超时路径也走这里**（协议硬要求，见 `account::PollAction`）。
    pub fn cancel_account_sign_in(&mut self, kind: AccountKind) {
        let pending = self
            .account_overlay_mut(kind)
            .and_then(|st| st.flow.pending_id().map(str::to_string));
        let Some(id) = pending else {
            // 没有本地流程 id 时**不要**瞎发 cancel：daemon 的 power 是「取消当前
            // 尝试」，而那个尝试可能不是本终端发起的（例如桌面端刚点了登录）。
            if let Some(st) = self.account_overlay_mut(kind) {
                st.notice = Some("没有本终端发起的登录流程（按 l 发起）".into());
            }
            return;
        };
        self.spawn_account_cancel(kind, id);
    }

    pub fn sign_out_account(&mut self, kind: AccountKind) {
        if let Some(st) = self.account_overlay_mut(kind) {
            st.busy = Some("登出");
            st.error = None;
        }
        self.spawn_account(kind, AccountOp::SignOut, "sign_out", json!({}));
    }

    pub fn select_account_profile(&mut self, profile_id: String) {
        if let Some(st) = self.account_overlay_mut(AccountKind::ChatGptPlan) {
            st.busy = Some("切换账户");
            st.error = None;
        }
        self.spawn_account(
            AccountKind::ChatGptPlan,
            AccountOp::SelectProfile,
            "profile.select",
            json!({ "profile_id": profile_id }),
        );
    }

    pub fn refresh_account_balance(&mut self) {
        self.spawn_account_balance(AccountKind::DeepSeekAccount);
    }

    pub fn verify_account_credential(&mut self) {
        if let Some(st) = self.account_overlay_mut(AccountKind::DeepSeekAccount) {
            st.busy = Some("校验凭据");
            st.error = None;
        }
        self.spawn_account(
            AccountKind::DeepSeekAccount,
            AccountOp::Verify,
            "verify",
            json!({}),
        );
    }

    /// 「管理用量」/「充值」深链：一律用后端给的 URL，客户端不自己拼。
    pub fn open_account_link(&mut self, url: String) {
        if let Err(error) = browser::open(&url) {
            self.toast(NoticeLevel::Error, error);
        }
    }

    // ───────────────────────── 发送 ─────────────────────────

    fn spawn_account(
        &self,
        kind: AccountKind,
        op: AccountOp,
        method_suffix: &'static str,
        params: Value,
    ) {
        let method = kind.method(method_suffix);
        self.spawn_api(move |api, tx| async move {
            let result = api.service_call(&method, params).await;
            let _ = tx.send(AppMsg::Action(ActionResult::Account(AccountResult {
                kind,
                op,
                result,
            })));
        });
    }

    fn spawn_account_status(&self, kind: AccountKind) {
        self.spawn_account(kind, AccountOp::Status, "status", json!({}));
    }

    fn spawn_account_profiles(&self, kind: AccountKind) {
        self.spawn_account(kind, AccountOp::Profiles, "profiles", json!({}));
    }

    fn spawn_account_models(&self, kind: AccountKind) {
        self.spawn_account(kind, AccountOp::Models, "models", json!({}));
    }

    fn spawn_account_balance(&self, kind: AccountKind) {
        self.spawn_account(kind, AccountOp::Balance, "balance", json!({}));
    }

    fn spawn_account_cancel(&self, kind: AccountKind, id: String) {
        // 两个档的参数名不同（协议事实）：siwc 用 `flow_id`，dsaccount 用
        // `attempt_id`，且 dsaccount 的 id 必须是**当前**尝试。
        let params = match kind {
            AccountKind::ChatGptPlan => json!({ "flow_id": id }),
            AccountKind::DeepSeekAccount => json!({ "attempt_id": id }),
        };
        self.spawn_account(kind, AccountOp::SignInCancel, "sign_in.cancel", params);
    }

    fn spawn_account_poll(&self, kind: AccountKind, id: String) {
        // dsaccount 的状态查询**不带参数**（本机只有一个当前尝试），
        // siwc 必须带 `flow_id`。
        let params = match kind {
            AccountKind::ChatGptPlan => json!({ "flow_id": id.clone() }),
            AccountKind::DeepSeekAccount => json!({}),
        };
        self.spawn_account(kind, AccountOp::SignInPoll { id }, "sign_in.status", params);
    }

    // ───────────────────────── tick 轮询 ─────────────────────────

    /// 每拍 tick 调用：按纯函数判定是否该轮询/超时。
    pub(super) fn tick_account(&mut self) {
        let Some(Overlay::Account(st)) = self.overlays.last().cloned() else {
            return;
        };
        let now = account::now_ms();
        match account::next_poll(&st.flow, st.last_poll_ms, now) {
            PollAction::None => {}
            PollAction::Poll { id } => {
                if let Some(state) = self.account_overlay_mut(st.kind) {
                    state.last_poll_ms = now;
                }
                self.spawn_account_poll(st.kind, id);
            }
            PollAction::Timeout { id } => {
                // 超时也要 cancel：留一个进行中的流程会把下一次登录挡在
                // `connection_busy` 上（spec §3.1）。
                self.spawn_account_cancel(st.kind, id);
                if let Some(state) = self.account_overlay_mut(st.kind) {
                    state.flow = Flow::Failed {
                        code: "expired".into(),
                        message: "登录超时未完成（浏览器里没有完成授权）".into(),
                    };
                    state.error = Some("登录超时未完成（已自动取消本次流程）".into());
                    state.busy = None;
                }
                self.toast(NoticeLevel::Warn, "登录超时，已取消本次流程".to_string());
                self.redraw_requested = true;
            }
        }
    }

    // ───────────────────────── 结果 ─────────────────────────

    pub(super) fn handle_account_result(&mut self, result: AccountResult) {
        let AccountResult { kind, op, result } = result;
        // 轮询响应只属于它发起时的流程，不能覆盖取消/重试后的新状态。
        if let AccountOp::SignInPoll { id } = &op
            && !self
                .account_overlay_mut(kind)
                .is_some_and(|st| st.flow.pending_id() == Some(id.as_str()))
        {
            return;
        }
        // 状态/目录的后台读取可能晚于动作返回，不能清掉在途动作的忙碌标记。
        if matches!(
            op,
            AccountOp::SignInStart
                | AccountOp::SignInCancel
                | AccountOp::SignOut
                | AccountOp::SelectProfile
                | AccountOp::Verify
        ) && let Some(st) = self.account_overlay_mut(kind)
        {
            st.busy = None;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if op == AccountOp::SignInStart
                    && self
                        .account_overlay_mut(kind)
                        .is_some_and(|st| st.close_when_started)
                {
                    self.overlays.pop();
                    self.toast(NoticeLevel::Error, format!("{}: {error}", kind.title()));
                    self.redraw_requested = true;
                    return;
                }
                let polling = matches!(op, AccountOp::SignInPoll { .. });
                self.account_failed(kind, polling, error);
                return;
            }
        };
        match op {
            AccountOp::Status => self.apply_account_status(kind, value),
            AccountOp::Profiles => match serde_json::from_value(value) {
                Ok(profiles) => {
                    self.account.profiles = Some(profiles);
                    self.redraw_requested = true;
                }
                Err(error) => self.account_parse_error(kind, "siwc.profiles", &error),
            },
            AccountOp::Models => self.apply_account_models(kind, value),
            AccountOp::SignInStart => self.apply_sign_in_start(kind, value),
            AccountOp::SignInPoll { id } => self.apply_sign_in_poll(kind, &id, value),
            AccountOp::SignInCancel => {
                if let Some(st) = self.account_overlay_mut(kind) {
                    if st.flow.is_pending() || st.flow == Flow::Starting {
                        st.flow = Flow::Idle;
                    }
                    st.notice = Some("已取消本次登录".into());
                    st.error = None;
                }
                self.spawn_account_status(kind);
                self.redraw_requested = true;
            }
            AccountOp::SignOut => self.apply_sign_out(kind, value),
            AccountOp::SelectProfile => {
                self.apply_account_status(kind, value);
                // 切了账户必须重取目录（品牌口径：`siwc.models` 随活动 profile 变）。
                self.spawn_account_models(kind);
            }
            AccountOp::Balance => match serde_json::from_value(value) {
                Ok(balance) => {
                    self.account.balance = Some(balance);
                    self.redraw_requested = true;
                }
                Err(error) => self.account_parse_error(kind, "dsaccount.balance", &error),
            },
            AccountOp::Verify => self.apply_verify(value),
        }
        // 设置页的「账号登录」行与模型候选跟着快照走（设置页可能正压在下面）。
        self.refresh_settings_account_summary();
    }

    fn apply_account_status(&mut self, kind: AccountKind, value: Value) {
        match kind {
            AccountKind::ChatGptPlan => {
                match serde_json::from_value::<qaqh_client::SiwcStatus>(value) {
                    Ok(status) => self.account.siwc = Some(status),
                    Err(error) => self.account_parse_error(kind, "siwc.status", &error),
                }
            }
            AccountKind::DeepSeekAccount => {
                match serde_json::from_value::<qaqh_client::DsAccountStatus>(value) {
                    Ok(status) => {
                        let signed_in = status.signed_in;
                        self.account.ds = Some(status);
                        if signed_in {
                            self.spawn_account_balance(kind);
                        }
                    }
                    Err(error) => self.account_parse_error(kind, "dsaccount.status", &error),
                }
            }
        }
        self.redraw_requested = true;
    }

    fn apply_account_models(&mut self, kind: AccountKind, value: Value) {
        let models: Result<Vec<ModelOption>, String> = match kind {
            AccountKind::ChatGptPlan => {
                serde_json::from_value::<Vec<qaqh_client::SiwcModel>>(value)
                    .map(|items| {
                        items
                            .into_iter()
                            .map(|model| ModelOption {
                                value: model.slug.clone(),
                                label: format!("{} ({})", model.display_name, model.slug),
                            })
                            .collect()
                    })
                    .map_err(|e| format!("siwc.models 解析失败：{e}"))
            }
            AccountKind::DeepSeekAccount => {
                serde_json::from_value::<Vec<qaqh_client::DsAccountModel>>(value)
                    .map(|items| {
                        items
                            .into_iter()
                            .filter_map(|model| {
                                let id = model.id?;
                                Some(ModelOption {
                                    label: id.clone(),
                                    value: id,
                                })
                            })
                            .collect()
                    })
                    .map_err(|e| format!("dsaccount.models 解析失败：{e}"))
            }
        };
        match models {
            Ok(models) => {
                self.account.store_models(kind, models.clone());
                // 设置页正开着就把候选同步进去（切档后必须重取，重取后必须能选）。
                if let Some(Overlay::Settings(st)) = self.overlays.last_mut() {
                    let current = st.effective_credential(self.config.as_ref());
                    if AccountKind::from_credential(&current) == Some(kind) {
                        st.model_options = models;
                    }
                }
                self.redraw_requested = true;
            }
            Err(message) => {
                if let Some(st) = self.account_overlay_mut(kind) {
                    st.error = Some(message.clone());
                }
                self.toast(NoticeLevel::Error, message);
            }
        }
    }

    fn apply_sign_in_start(&mut self, kind: AccountKind, value: Value) {
        let now = account::now_ms();
        match kind {
            AccountKind::ChatGptPlan => {
                match serde_json::from_value::<qaqh_client::SiwcSignInStart>(value) {
                    Ok(start) => {
                        if let Some(st) = self.account_overlay_mut(kind) {
                            st.started(start.flow_id.clone(), start.expires_at_ms, now);
                            st.authorize_url = Some(start.authorize_url.clone());
                        }
                        self.finish_sign_in_start(kind, &start.authorize_url);
                    }
                    Err(error) => self.account_parse_error(kind, "siwc.sign_in.start", &error),
                }
            }
            AccountKind::DeepSeekAccount => {
                match serde_json::from_value::<qaqh_client::DsAccountSignInStart>(value) {
                    Ok(start) => {
                        // `expiresIn` 是**秒**（siwc 是毫秒，别混）。
                        let deadline = now.saturating_add(start.expires_in.saturating_mul(1_000));
                        if let Some(st) = self.account_overlay_mut(kind) {
                            st.started(start.attempt_id.clone(), deadline, now);
                            st.authorize_url = Some(start.authorize_url.clone());
                        }
                        self.finish_sign_in_start(kind, &start.authorize_url);
                    }
                    Err(error) => self.account_parse_error(kind, "dsaccount.sign_in.start", &error),
                }
            }
        }
        self.redraw_requested = true;
    }

    fn finish_sign_in_start(&mut self, kind: AccountKind, url: &str) {
        if self
            .account_overlay_mut(kind)
            .is_some_and(|st| st.close_when_started)
        {
            self.cancel_account_sign_in(kind);
            self.overlays.pop();
        } else {
            self.open_authorize_url(url);
        }
    }

    fn open_authorize_url(&mut self, url: &str) {
        match browser::open(url) {
            Ok(()) => self.toast(
                NoticeLevel::Info,
                "已在浏览器打开授权页，完成后回到终端等待".to_string(),
            ),
            Err(error) => {
                // 浏览器打不开**不**等于登录失败：daemon 的流程仍在等回调，
                // 用户可以手动复制 URL。失败必须可见（B1）。
                if let Some(st) = self.overlays.last_mut().and_then(|o| match o {
                    Overlay::Account(st) => Some(st),
                    _ => None,
                }) {
                    st.error = Some(format!("{error}（可复制授权链接手动打开：{url}）"));
                }
                self.redraw_requested = true;
            }
        }
    }

    fn apply_sign_in_poll(&mut self, kind: AccountKind, id: &str, value: Value) {
        match kind {
            AccountKind::ChatGptPlan => {
                match serde_json::from_value::<qaqh_client::SiwcSignInStatus>(value) {
                    Ok(status) => {
                        let Some(st) = self.account_overlay_mut(kind) else {
                            return;
                        };
                        let outcome = account::apply_siwc_flow(&mut st.flow, id, &status);
                        self.finish_flow(kind, outcome);
                    }
                    Err(error) => self.account_parse_error(kind, "siwc.sign_in.status", &error),
                }
            }
            AccountKind::DeepSeekAccount => {
                match serde_json::from_value::<qaqh_client::DsAccountSignInStatus>(value) {
                    Ok(status) => {
                        let Some(st) = self.account_overlay_mut(kind) else {
                            return;
                        };
                        let outcome = account::apply_ds_flow(&mut st.flow, id, &status);
                        self.finish_flow(kind, outcome);
                    }
                    Err(error) => {
                        self.account_parse_error(kind, "dsaccount.sign_in.status", &error)
                    }
                }
            }
        }
    }

    fn finish_flow(&mut self, kind: AccountKind, outcome: FlowOutcome) {
        match outcome {
            FlowOutcome::Continue => {
                self.redraw_requested = true;
            }
            FlowOutcome::Completed { .. } => {
                let who = kind.title();
                if let Some(st) = self.account_overlay_mut(kind) {
                    st.notice = Some(format!("{who} 登录成功"));
                    st.error = None;
                }
                self.toast(NoticeLevel::Info, format!("{who} 登录成功"));
                // 登录成功后状态/目录/余额全部可能变了：整组重取。
                self.account.siwc = None;
                self.account.profiles = None;
                self.account.ds = None;
                self.refresh_account(kind);
            }
            FlowOutcome::Failed { code, message } => {
                if let Some(st) = self.account_overlay_mut(kind) {
                    st.error = Some(format!("{code}: {message}"));
                }
                self.toast(NoticeLevel::Error, format!("登录失败：{message}"));
                self.spawn_account_status(kind);
            }
            FlowOutcome::Superseded => {
                // 「被新登录顶掉」不是错误：停止轮询、不要 cancel 旧 id。
                if let Some(st) = self.account_overlay_mut(kind) {
                    st.notice = Some("本次登录已被新的登录流程取代（已停止等待）".into());
                }
                self.redraw_requested = true;
            }
        }
    }

    fn apply_sign_out(&mut self, kind: AccountKind, value: Value) {
        match kind {
            AccountKind::ChatGptPlan => {
                match serde_json::from_value::<qaqh_client::SiwcSignOutResult>(value) {
                    Ok(result) => {
                        let message = if result.revoked {
                            "已登出".to_string()
                        } else {
                            // 远端撤销未确认：本地已清，不要吓唬用户「登出失败」。
                            "已登出（远端撤销未确认，可在 ChatGPT 设置里断开）".to_string()
                        };
                        if let Some(st) = self.account_overlay_mut(kind) {
                            st.notice = Some(message.clone());
                        }
                        self.toast(NoticeLevel::Info, message);
                    }
                    Err(error) => self.account_parse_error(kind, "siwc.sign_out", &error),
                }
            }
            AccountKind::DeepSeekAccount => {
                if let Some(st) = self.account_overlay_mut(kind) {
                    st.notice = Some("已登出".into());
                }
                self.toast(NoticeLevel::Info, "DeepSeek 账号已登出".to_string());
            }
        }
        self.account.siwc = None;
        self.account.profiles = None;
        self.account.ds = None;
        self.account.balance = None;
        self.refresh_account(kind);
    }

    fn apply_verify(&mut self, value: Value) {
        match serde_json::from_value::<qaqh_client::DsAccountVerify>(value) {
            Ok(verify) if verify.valid => {
                if let Some(st) = self.account_overlay_mut(AccountKind::DeepSeekAccount) {
                    st.notice = Some("凭据有效".into());
                    st.error = None;
                }
            }
            Ok(verify) => {
                let reason = verify.reason.unwrap_or_else(|| "凭据无效".into());
                if let Some(st) = self.account_overlay_mut(AccountKind::DeepSeekAccount) {
                    st.error = Some(reason.clone());
                }
                self.toast(
                    NoticeLevel::Error,
                    if verify.cleared {
                        format!("{reason}（本地凭据已清除，请重新登录）")
                    } else {
                        reason
                    },
                );
                self.account.ds = None;
                self.account.balance = None;
                self.spawn_account_status(AccountKind::DeepSeekAccount);
            }
            Err(error) => self.account_parse_error(AccountKind::DeepSeekAccount, "verify", &error),
        }
        self.redraw_requested = true;
    }

    fn account_failed(&mut self, kind: AccountKind, polling: bool, error: String) {
        // 轮询期的错误分两类：被顶掉（停止）与暂时故障（保留流程继续等）。
        if polling && account::is_superseded_error(kind, &error) {
            if let Some(st) = self.account_overlay_mut(kind) {
                st.flow = Flow::Superseded;
                st.notice = Some("本次登录已被新的登录流程取代（已停止等待）".into());
                st.error = None;
            }
            self.redraw_requested = true;
            return;
        }
        // 轮询每秒一拍：同一条错误不该每拍弹一次 toast（把用户的提示栏刷成瀑布）。
        let repeated = self
            .account_overlay_mut(kind)
            .and_then(|st| st.error.clone())
            .is_some_and(|previous| previous == error);
        if let Some(st) = self.account_overlay_mut(kind) {
            st.error = Some(error.clone());
            // 「开工」失败没有 id 可轮询：退回 Idle，用户按 l 可重试。
            if st.flow == Flow::Starting {
                st.flow = Flow::Idle;
            }
        }
        if !repeated {
            self.toast(NoticeLevel::Error, format!("{}: {error}", kind.title()));
        }
        self.redraw_requested = true;
    }

    fn account_parse_error(&mut self, kind: AccountKind, method: &str, error: &serde_json::Error) {
        let message = format!("{method} 响应解析失败：{error}");
        if let Some(st) = self.account_overlay_mut(kind) {
            st.error = Some(message.clone());
        }
        self.toast(NoticeLevel::Error, message);
        self.redraw_requested = true;
    }

    /// 账号卡片自由滚动（滚轮/PgUp/PgDn；焦点跟随由绘制层收口）。
    pub fn account_scroll(&mut self, up: bool, lines: usize) {
        if let Some(Overlay::Account(st)) = self.overlays.last_mut() {
            st.scroll = if up {
                st.scroll.saturating_sub(lines)
            } else {
                st.scroll.saturating_add(lines)
            };
            self.redraw_requested = true;
        }
    }

    fn account_overlay_mut(&mut self, kind: AccountKind) -> Option<&mut AccountState> {
        match self.overlays.last_mut() {
            Some(Overlay::Account(st)) if st.kind == kind => Some(st),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use account::AccountAction as A;

    #[tokio::test]
    async fn account_close_during_start_cancels_after_id_without_opening_browser() {
        for kind in [AccountKind::ChatGptPlan, AccountKind::DeepSeekAccount] {
            let (mut app, _rx) = App::new_for_test();
            let mut state = AccountState::new(kind);
            state.flow = Flow::Starting;
            app.overlays.push(Overlay::Account(state));
            app.account_action(A::Close);
            assert!(
                matches!(app.overlays.last(), Some(Overlay::Account(st)) if st.close_when_started)
            );
            let start = match kind {
                AccountKind::ChatGptPlan => json!({
                    "flowId": "mine", "authorizeUrl": "https://example.com/auth",
                    "redirectUri": "http://127.0.0.1:1234/callback", "expiresAtMs": u64::MAX,
                    "usageUrl": "https://example.com/usage"
                }),
                AccountKind::DeepSeekAccount => json!({
                    "attemptId": "mine", "authorizeUrl": "https://example.com/auth", "expiresIn": 60,
                    "redirectUri": "http://127.0.0.1:1234/callback"
                }),
            };
            app.apply_sign_in_start(kind, start);
            assert!(app.overlays.is_empty());
            assert!(
                !app.toasts
                    .iter()
                    .any(|toast| toast.text.contains("已在浏览器"))
            );
        }
    }

    #[test]
    fn account_actions_reject_stale_or_disabled_targets() {
        let (mut app, _rx) = App::new_for_test();
        let mut state = AccountState::new(AccountKind::ChatGptPlan);
        state.flow = Flow::Starting;
        app.overlays.push(Overlay::Account(state));
        // 不需 runtime；若禁用动作漏过判据，就会尝试 spawn 并 panic。
        for action in [A::Login, A::Cancel, A::SignOut, A::SelectProfile(999)] {
            app.account_action(action);
        }
        assert!(
            matches!(app.overlays.last(), Some(Overlay::Account(st)) if st.flow == Flow::Starting)
        );
        app.overlays.clear();
        app.account_action(A::Login);
        assert!(app.overlays.is_empty());
    }

    #[test]
    fn account_background_responses_do_not_clear_busy_or_finish_a_new_flow() {
        let (mut app, _rx) = App::new_for_test();
        let kind = AccountKind::ChatGptPlan;
        let mut state = AccountState::new(kind);
        state.flow = Flow::Starting;
        state.busy = Some("发起登录");
        app.overlays.push(Overlay::Account(state));
        app.handle_account_result(AccountResult {
            kind,
            op: AccountOp::Profiles,
            result: Ok(json!({"active": null, "profiles": [], "pending": []})),
        });
        assert!(
            matches!(app.overlays.last(), Some(Overlay::Account(st)) if st.busy == Some("发起登录"))
        );
        app.handle_account_result(AccountResult {
            kind,
            op: AccountOp::SignInPoll { id: "old".into() },
            result: Ok(json!({"flowId": "old", "state": "completed", "profileId": "old-profile"})),
        });
        assert!(
            matches!(app.overlays.last(), Some(Overlay::Account(st)) if st.flow == Flow::Starting)
        );
    }

    #[test]
    fn account_close_during_start_failure_returns_to_previous_page() {
        let (mut app, _rx) = App::new_for_test();
        let kind = AccountKind::ChatGptPlan;
        let mut state = AccountState::new(kind);
        state.flow = Flow::Starting;
        app.overlays.push(Overlay::Settings(Default::default()));
        app.overlays.push(Overlay::Account(state));
        app.account_action(A::Close);
        app.handle_account_result(AccountResult {
            kind,
            op: AccountOp::SignInStart,
            result: Err("connection failed".into()),
        });
        assert!(matches!(app.overlays.as_slice(), [Overlay::Settings(_)]));
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.text.contains("connection failed"))
        );
    }
}
