//! 设置面板动作与配置读写（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    pub fn fetch_config(&mut self) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::CONFIG_LOAD, &serde_json::json!({}))
                .await
                .map_err(|e| e.to_string());
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
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        let payload = st.draft.to_json();
        tokio::spawn(async move {
            let result = client
                .service(methods::CONFIG_SAVE, &payload)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite { label: "设置", result }));
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
            settings::FieldId::WorkspaceMode => {
                let mode = st
                    .ws_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.workspace.mode.clone()));
                if let Some(mode) = mode {
                    self.set_workspace_mode(mode);
                }
            }
            _ => {}
        }
    }

    /// 端口字段 ←→：切换候选（回车才真正应用）。
    pub(super) fn settings_port_cycle(&mut self, st: &mut SettingsState, delta: i32) {
        match st.row().id {
            settings::FieldId::ActiveProfile => {
                let Some(cfg) = self.config.as_ref() else { return };
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
            settings::FieldId::WorkspaceMode => {
                // local（全平台）/ wsl（仅 Windows）——与后端 workspace.set_mode 校验一致。
                let modes: &[&str] = if cfg!(windows) { &["local", "wsl"] } else { &["local"] };
                let cur = st
                    .ws_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.workspace.mode.clone()))
                    .unwrap_or_else(|| "local".into());
                let idx = modes.iter().position(|m| *m == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(modes.len() as i32) as usize;
                st.ws_sel = Some(modes[next].to_string());
            }
            _ => {}
        }
    }

    /// `profile.apply`：切换活跃 profile（服务端单写口，写后广播 reload）。
    pub fn apply_profile(&mut self, name: String) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::PROFILE_APPLY, &serde_json::json!({ "name": name }))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "应用 Profile",
                result,
            }));
        });
    }

    /// `workspace.set_mode`：local / wsl（仅 Windows）（服务端单写口）。
    pub fn set_workspace_mode(&mut self, mode: String) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::WORKSPACE_SET_MODE, &serde_json::json!({ "mode": mode }))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "workspace 模式",
                result,
            }));
        });
    }

    pub fn set_permission_level(&mut self, level: u8) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(
                    methods::CONFIG_SET_PERMISSION_LEVEL,
                    &serde_json::json!({ "level": level }),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "权限级别",
                result,
            }));
        });
    }

    pub fn toggle_settings(&mut self) {
        let open = self.overlays.last().is_some_and(|o| matches!(o, Overlay::Settings(_)));
        if open {
            self.overlays.pop();
        } else {
            if self.config.is_none() {
                self.fetch_config();
            }
            self.overlays.push(Overlay::Settings(SettingsState::default()));
        }
    }

}
