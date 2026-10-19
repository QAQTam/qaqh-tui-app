//! 设置面板动作与配置读写（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    /// 设置页鼠标点击一行：先聚焦，再执行与 Enter 相同的语义。
    pub fn mouse_settings_row(&mut self, index: usize) {
        let Some(Overlay::Settings(mut st)) = self.overlays.last().cloned() else {
            return;
        };
        st.focus = index.min(settings::ROWS.len().saturating_sub(1));
        st.editing = None;
        let row = st.row();
        match row.kind {
            settings::FieldKind::Text
            | settings::FieldKind::Secret
            | settings::FieldKind::Number
            | settings::FieldKind::Float => {
                st.editing = st.start_edit(self.config.as_ref());
            }
            settings::FieldKind::Enum | settings::FieldKind::Toggle => {
                // 与键盘路径共用同一入口：换档后该重取的东西必须一起重取。
                self.settings_cycle(&mut st, 1);
            }
            settings::FieldKind::Port => self.settings_port_activate(&mut st),
        }
        self.replace_overlay(Overlay::Settings(st));
    }

    /// 设置页字段循环的**唯一入口**（键盘 ←→/Enter 与鼠标共用）。
    ///
    /// 除了 `cycle` 本身，它还负责一件「跨档副作用」：凭据来源换档后，账号面
    /// 状态与模型目录必须重取（品牌口径：切账户后必须重取模型目录；换档同理）。
    /// 散落在各按键分支里就一定会漏一条路径。
    pub(super) fn settings_cycle(&mut self, st: &mut SettingsState, delta: i32) {
        let before = st.effective_credential(self.config.as_ref());
        if let Err(error) = st.cycle(self.config.as_ref(), delta) {
            self.toast(NoticeLevel::Error, error);
            return;
        }
        let after = st.effective_credential(self.config.as_ref());
        if before == after {
            return;
        }
        if let Some(kind) = account::AccountKind::from_credential(&after) {
            self.refresh_account(kind);
        }
        self.refresh_settings_account_summary();
    }

    /// 把账号快照写回设置页的「账号登录」行与模型候选（没有设置页时什么都不做）。
    pub(super) fn refresh_settings_account_summary(&mut self) {
        let credential = self
            .overlays
            .iter()
            .rev()
            .find_map(|overlay| match overlay {
                Overlay::Settings(st) => Some(st.effective_credential(self.config.as_ref())),
                _ => None,
            });
        let Some(credential) = credential else {
            return;
        };
        let kind = account::AccountKind::from_credential(&credential);
        let summary = kind
            .map(|kind| self.account.summary(kind))
            .unwrap_or_default();
        let models = kind
            .map(|kind| self.account.models_for(kind).to_vec())
            .unwrap_or_default();
        for overlay in self.overlays.iter_mut() {
            if let Overlay::Settings(st) = overlay {
                st.account_summary = summary.clone();
                st.model_options = models.clone();
            }
        }
    }

    /// `config.load` 落地后的补课：设置页先打开、配置后到的那条路径。
    ///
    /// 只在这两个面板之一开着时才去取账号状态——否则每次配置刷新都顺手打三个
    /// 网络请求，而用户根本没在看账号。
    pub(super) fn after_config_loaded(&mut self) {
        self.refresh_settings_account_summary();
        let credential = self
            .config
            .as_ref()
            .map(|c| c.credential.clone())
            .unwrap_or_default();
        let Some(kind) = account::AccountKind::from_credential(&credential) else {
            return;
        };
        let watching = matches!(
            self.overlays.last(),
            Some(Overlay::Settings(_)) | Some(Overlay::Account(_))
        );
        if !watching {
            return;
        }
        let missing = match kind {
            account::AccountKind::ChatGptPlan => self.account.siwc.is_none(),
            account::AccountKind::DeepSeekAccount => self.account.ds.is_none(),
        };
        if missing {
            self.refresh_account(kind);
        }
    }

    pub fn fetch_config(&mut self) {
        self.spawn_api(move |api, tx| async move {
            let result = api.query(QueryRequest::ConfigLoad).await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigLoaded(result)));
        });
    }

    /// 上下文窗口分母：`ConfigDto.context_length`（daemon 本地压缩用的同源分母）。
    ///
    /// v2 conversation 投影上的 `context_limit` 已随后端 Info 面板一起删除，这里
    /// 改吃全局配置——一个 daemon 一份，不在 `SessionState` 上留 per-session 副本
    /// （副本会引出「忘了刷某个会话」这类不一致）。
    pub fn context_limit(&self) -> Option<u32> {
        self.config
            .as_ref()
            .map(|c| c.context_length.min(u32::MAX as u64) as u32)
            .filter(|limit| *limit > 0)
    }

    /// `config.save`：把设置页草稿作为 Merge Patch 发送（只发脏字段）。
    pub(super) fn save_settings(&mut self, st: &mut SettingsState) {
        if self.settings_saving {
            return;
        }
        if st.draft.is_empty() {
            self.toast(NoticeLevel::Info, "设置无改动");
            return;
        }
        if let Err(e) = st.draft.validate() {
            self.toast(NoticeLevel::Error, format!("校验失败：{e}"));
            return;
        }
        self.settings_saving = true;
        let payload = serde_json::to_value(&st.draft).unwrap_or(serde_json::Value::Null);
        self.spawn_api(move |api, tx| async move {
            let result = api
                .action(ActionRequest::ConfigSave { fields: payload })
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "设置",
                result,
            }));
        });
    }

    /// 端口字段回车：即时走各自的单写口（不进草稿）。
    pub(super) fn settings_port_activate(&mut self, st: &mut SettingsState) {
        match st.row().id {
            settings::FieldId::PermissionLevel => {
                self.toast(NoticeLevel::Info, "聚焦权限级别后按 1-3 即时生效");
            }
            settings::FieldId::ActiveProfile => {
                let name = st
                    .profile_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.active_profile.clone()));
                if let Some(name) = name {
                    self.apply_profile(name);
                }
            }
            settings::FieldId::Account => {
                // 入口只对账号档有意义：BYOK 档没有登录这件事，说清楚而不是开一个空面板。
                match account::AccountKind::from_credential(
                    &st.effective_credential(self.config.as_ref()),
                ) {
                    Some(kind) => self.open_account(kind),
                    None => self.toast(
                        NoticeLevel::Info,
                        "当前凭据来源是 api_key（BYOK），无需登录".to_string(),
                    ),
                }
            }
            _ => {}
        }
    }

    /// 端口字段 ←→：切换候选（回车才真正应用）。
    pub(super) fn settings_port_cycle(&mut self, st: &mut SettingsState, delta: i32) {
        // 唯一剩下的端口字段是 profile（workspace 模式那个随能力下线一起删了）。
        if !matches!(st.row().id, settings::FieldId::ActiveProfile) {
            return;
        }
        let Some(cfg) = self.config.as_ref() else {
            return;
        };
        if cfg.profiles.is_empty() {
            return;
        }
        let cur = st
            .profile_sel
            .clone()
            .unwrap_or_else(|| cfg.active_profile.clone());
        let idx = cfg.profiles.iter().position(|n| *n == cur).unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(cfg.profiles.len() as i32) as usize;
        st.profile_sel = Some(cfg.profiles[next].clone());
    }

    /// `profile.apply`：切换活跃 profile（服务端单写口，写后广播 reload）。
    pub fn apply_profile(&mut self, name: String) {
        self.spawn_api(move |api, tx| async move {
            let result = api.action(ActionRequest::ProfileApply { name }).await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "应用 Profile",
                result,
            }));
        });
    }

    pub fn set_permission_level(&mut self, level: u8) {
        self.spawn_api(move |api, tx| async move {
            let result = api
                .action(ActionRequest::ConfigSetPermissionLevel {
                    level: level.into(),
                })
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "权限级别",
                result,
            }));
        });
    }

    pub fn toggle_settings(&mut self) {
        let open = self
            .overlays
            .last()
            .is_some_and(|o| matches!(o, Overlay::Settings(_)));
        if open {
            self.overlays.pop();
        } else {
            if self.config.is_none() {
                self.fetch_config();
            }
            self.overlays
                .push(Overlay::Settings(SettingsState::default()));
            self.refresh_settings_account_summary();
            // 账号档：打开设置页就顺手把状态/目录刷一次（本地 daemon，代价可忽略）。
            // 没有这一下，「账号登录」行会一直停在「回车打开账号面板」直到用户点进去。
            let credential = self
                .config
                .as_ref()
                .map(|c| c.credential.clone())
                .unwrap_or_default();
            if let Some(kind) = account::AccountKind::from_credential(&credential) {
                self.refresh_account(kind);
            }
        }
    }
}
