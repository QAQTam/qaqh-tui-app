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
                if let Err(error) = st.cycle(self.config.as_ref(), 1) {
                    self.toast(NoticeLevel::Error, error);
                }
            }
            settings::FieldKind::Port => self.settings_port_activate(&mut st),
        }
        self.replace_overlay(Overlay::Settings(st));
    }

    pub fn fetch_config(&mut self) {
        self.spawn_api(move |api, tx| async move {
            let result = api.query(QueryRequest::ConfigLoad).await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigLoaded(result)));
        });
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
                self.toast(NoticeLevel::Info, "聚焦权限级别后按 1-4 即时生效");
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
        }
    }
}
