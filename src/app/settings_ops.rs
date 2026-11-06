//! 设置面板动作与配置读写（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    pub(crate) fn load_local_settings(&mut self) {
        match crate::local_config::Store::discover() {
            Ok(store) => {
                match store.load() {
                    Ok(config) => self.local_config = config,
                    Err(error) => self.toast(
                        NoticeLevel::Error,
                        format!("本地配置 {}: {error}", store.path().display()),
                    ),
                }
                self.local_store = Some(store);
            }
            Err(error) => self.toast(NoticeLevel::Error, error),
        }
    }

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
            settings::FieldKind::Port => {
                // 先提交设置状态，再导航；否则会覆盖刚压栈的账号卡片。
                self.replace_overlay(Overlay::Settings(st.clone()));
                self.settings_port_activate(&mut st);
                return;
            }
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
        for overlay in self.overlays.iter_mut() {
            if let Overlay::Settings(st) = overlay {
                st.account_summary = summary.clone();
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
        } else if self.account.models_for(kind).is_empty() {
            self.spawn_account_models(kind);
        }
    }

    pub fn fetch_config(&mut self) {
        self.spawn_api(move |api, tx| async move {
            let result = api.query(QueryRequest::ConfigLoad).await;
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigLoaded(result)));
        });
    }

    /// provider 目录快照：`session.create` 强制 `model_selection`（backend-v3），
    /// 目录是它的唯一权威来源。Ready / 创建失败时刷新。
    pub fn fetch_provider_catalog(&mut self) {
        if self.runtime.client_opt().is_none() {
            return;
        }
        self.spawn_api(move |api, tx| async move {
            let result = api.provider_catalog().await;
            let _ = tx.send(AppMsg::Action(ActionResult::ProviderCatalog(result)));
        });
    }

    /// 新会话的 `model_selection`（backend-v3 强制项）。
    ///
    /// 来源优先级（对齐 electron `model-config.ts`：先目录默认，再回退）：
    /// 1. `QAQH_MODEL_SELECTION=provider/model` 环境变量（显式覆盖）；
    /// 2. 目录的 `default_model_selection`（设置页「默认模型」写入）；
    /// 3. 目录里第一个（字典序）启用模型。
    ///
    /// 目录为空（全新 daemon 未配置任何 provider）时返回 `None`：daemon 会以
    /// 「请选择服务商和模型」拒绝创建，调用方据此提示去设置页配置。
    pub fn default_model_selection(&self) -> Option<qaqh_types::ModelSelection> {
        if let Ok(raw) = std::env::var("QAQH_MODEL_SELECTION") {
            let raw = raw.trim();
            if let Some((provider_id, model_id)) = raw.split_once('/') {
                if !provider_id.is_empty() && !model_id.is_empty() {
                    return Some(qaqh_types::ModelSelection {
                        provider_id: provider_id.to_owned(),
                        model_id: model_id.to_owned(),
                        options: Default::default(),
                    });
                }
            }
            log::warn!("QAQH_MODEL_SELECTION 格式应为 provider/model，收到 {raw:?}（忽略）");
        }
        if let Some(selection) = self.provider_catalog.default_model_selection.clone() {
            return Some(selection);
        }
        let (provider_id, model_id) = self.provider_catalog.first_enabled_selection()?;
        Some(qaqh_types::ModelSelection {
            provider_id,
            model_id,
            options: Default::default(),
        })
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
        if !st.has_changes() {
            self.toast(NoticeLevel::Info, "设置无改动");
            return;
        }
        if let Err(e) = st.draft.validate() {
            self.toast(NoticeLevel::Error, format!("校验失败：{e}"));
            return;
        }
        if let Some(on) = st.performance_draft {
            let local = crate::local_config::LocalConfig {
                performance_mode: on,
            };
            let result = self
                .local_store
                .as_ref()
                .ok_or_else(|| "TUI 本地配置目录不可用".to_string())
                .and_then(|store| store.save(local));
            if let Err(error) = result {
                self.toast(NoticeLevel::Error, format!("本地配置保存失败：{error}"));
                return;
            }
            self.local_config = local;
            st.local_loaded = local;
            st.performance_draft = None;
            // 即时停止在途缓动，保持正文与命中位置一致。
            for session in self.sessions.values_mut() {
                session.scroll.tween = None;
            }
            self.redraw_requested = true;
            self.toast(NoticeLevel::Info, "TUI 本地设置已保存（仅本地文件）");
        }
        // 本地-only 保存到此结束：不 spawn API，不 config.save，不 config.load。
        if st.draft.is_empty() {
            return;
        }
        self.settings_saving = true;
        let payload = serde_json::to_value(&st.draft).unwrap_or(serde_json::Value::Null);
        self.spawn_api(move |api, tx| async move {
            let result = match api
                .config_action(RingingCommand::ConfigSave { patch: payload })
                .await
            {
                Ok(_) => api
                    .query(QueryRequest::ConfigLoad)
                    .await
                    .map_err(|error| format!("已写入，但配置回读失败（保留草稿）：{error}")),
                Err(error) => Err(error),
            };
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "设置",
                result,
            }));
        });
    }

    /// 端口字段回车：即时走各自的单写口（不进草稿）。
    ///
    /// 目录分区（模型/默认模型/新增/删除）在这里发 `provider.*` /
    /// `model.selection.set_default` 命令——`config_revision` CAS 由
    /// `catalog_write` 用 `provider.list` 快照的 revision 统一携带。
    pub(super) fn settings_port_activate(&mut self, st: &mut SettingsState) {
        match st.row().id {
            settings::FieldId::PermissionLevel => {
                self.toast(NoticeLevel::Info, "聚焦权限级别后按 1-4 即时生效");
            }
            settings::FieldId::Account => {
                // 入口对两档账号额度有意义；api_key 档没有登录这件事，说明白。
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
            settings::FieldId::DefaultModel => {
                // 回退链与展示一致：循环状态 → 目录当前默认 → 首个启用模型。
                // 未按过 ←→ 的 Enter 是「把展示值设为默认」（幂等）。
                let sel = st
                    .catalog_draft
                    .default_model_sel
                    .clone()
                    .or_else(|| {
                        st.catalog
                            .default_selection()
                            .map(|(p, m)| format!("{p}/{m}"))
                    })
                    .or_else(|| {
                        st.catalog
                            .first_enabled_selection()
                            .map(|(p, m)| format!("{p}/{m}"))
                    });
                let Some(sel) = sel else {
                    self.toast(NoticeLevel::Warn, "目录为空：先新增 provider 与模型");
                    return;
                };
                let Some((provider_id, model_id)) = sel.split_once('/') else {
                    return;
                };
                self.apply_default_model(provider_id.to_owned(), model_id.to_owned());
            }
            settings::FieldId::AddModelApply => {
                let Some(provider_id) = st.catalog_draft.add_model_target(&st.catalog) else {
                    self.toast(NoticeLevel::Warn, "目录为空：先保存一个 provider");
                    return;
                };
                let model_id = st.catalog_draft.add_model_id.trim().to_string();
                if model_id.is_empty() {
                    self.toast(NoticeLevel::Warn, "模型 ID 不能为空（先在「模型 ID」行回车输入）");
                    return;
                }
                let context = st
                    .catalog_draft
                    .add_model_context
                    .unwrap_or(128_000)
                    .min(u32::MAX as u64) as u32;
                let max_tokens = st
                    .catalog_draft
                    .add_model_max_tokens
                    .unwrap_or(8_192)
                    .min(u32::MAX as u64) as u32;
                self.apply_add_model(provider_id, model_id, context, max_tokens);
            }
            settings::FieldId::NewProviderApply => {
                let id = st.catalog_draft.new_provider_id.trim().to_string();
                if id.is_empty() {
                    self.toast(NoticeLevel::Warn, "provider ID 不能为空");
                    return;
                }
                let name = st
                    .catalog_draft
                    .new_provider_name
                    .trim()
                    .to_string()
                    .replace(&id, &id)
                    .is_empty()
                    .then(|| id.clone())
                    .unwrap_or_else(|| {
                        let n = st.catalog_draft.new_provider_name.trim();
                        if n.is_empty() { id.clone() } else { n.to_string() }
                    });
                let base_url = st.catalog_draft.new_provider_base_url.trim().to_string();
                if base_url.is_empty() {
                    self.toast(NoticeLevel::Warn, "Base URL 不能为空");
                    return;
                }
                let wire = st
                    .catalog_draft
                    .new_provider_wire
                    .clone()
                    .unwrap_or_else(|| "chat".into());
                let credential = st
                    .catalog_draft
                    .new_provider_credential
                    .clone()
                    .unwrap_or_else(|| "api_key".into());
                let api_key = st.catalog_draft.new_provider_api_key.trim().to_string();
                self.apply_new_provider(id, name, base_url, wire, credential, api_key);
            }
            settings::FieldId::DeleteModelApply => {
                let Some(sel) = st
                    .catalog_draft
                    .delete_model_sel
                    .clone()
                    .or_else(|| settings::CatalogDraft::delete_candidates(&st.catalog).first().cloned())
                else {
                    self.toast(NoticeLevel::Warn, "目录为空：没有可删除的模型");
                    return;
                };
                let Some((provider_id, model_id)) = sel.split_once('/') else {
                    return;
                };
                // 默认模型引用预检（electron registry.ts 同款：默认指向的模型不可删）。
                if let Some((dp, dm)) = st.catalog.default_selection()
                    && dp == provider_id
                    && dm == model_id
                {
                    self.toast(
                        NoticeLevel::Warn,
                        format!("{sel} 是全局默认模型：先把默认切到别的模型再删除"),
                    );
                    return;
                }
                self.apply_delete_model(provider_id.to_owned(), model_id.to_owned());
            }
            _ => {}
        }
    }

    /// 端口字段 ←→：候选循环全部在 `SettingsState::cycle` 内消费（目录快照在
    /// state 上）；权限级别的数字快捷键走 App 层按键路径。
    pub(super) fn settings_port_cycle(&mut self, _st: &mut SettingsState, _delta: i32) {}

    // ───────────────────────── 模型目录写口（E26：五种目录命令带 config_revision）─────────

    /// 目录写命令统一出口：携带 `config_revision` CAS，成功后重拉目录。
    pub(super) fn catalog_write(&mut self, command: RingingCommand, label: &'static str) {
        let revision = self.provider_catalog.revision;
        self.spawn_api(move |api, tx| async move {
            let options = qaqh_client::CommandOptions {
                command_id: None,
                preconditions: vec![qaqh_ringing::CommandPrecondition::ConfigRevision { revision }],
            };
            let result = match api.send_command(None, command, options).await {
                Ok(ack) if ack.status == RingingCommandAckStatus::Rejected => Err(
                    ack.message.unwrap_or_else(|| "目录写入被拒".to_string()),
                ),
                Ok(_) => Ok(serde_json::json!({})),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label,
                result,
            }));
            // 目录变了：重拉（ConfigWrite 处理器会 fetch_config；目录单独拉）。
            let catalog = api.provider_catalog().await;
            let _ = tx.send(AppMsg::Action(ActionResult::ProviderCatalog(catalog)));
        });
    }

    /// 全局默认模型：`model.selection.set_default`（不再依赖 default_profile）。
    pub fn apply_default_model(&mut self, provider_id: String, model_id: String) {
        self.catalog_write(
            RingingCommand::ModelSelectionSetDefault {
                selection: qaqh_types::ModelSelection {
                    provider_id,
                    model_id,
                    options: Default::default(),
                },
            },
            "默认模型",
        );
    }

    /// 手动添加/更新模型（v3 已删预置目录，模型定义由用户声明）。
    pub fn apply_add_model(
        &mut self,
        provider_id: String,
        model_id: String,
        context_window: u32,
        max_output_tokens: u32,
    ) {
        self.catalog_write(
            RingingCommand::ProviderModelUpsert {
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                model: qaqh_profile::ModelDef {
                    enabled: true,
                    display_name: Some(model_id.clone()),
                    context_window,
                    max_output_tokens,
                    reasoning_levels: Vec::new(),
                    capabilities: Default::default(),
                    defaults: Default::default(),
                },
            },
            "添加模型",
        );
    }

    /// 新增/更新 provider（BYOK 密钥走 SecretWrite：空 = Keep）。
    pub fn apply_new_provider(
        &mut self,
        id: String,
        name: String,
        base_url: String,
        wire: String,
        credential: String,
        api_key: String,
    ) {
        // wire/credential 走 serde 词表直译（规范名，无 FromStr）。
        let wire = match wire.as_str() {
            "chat" => qaqh_types::Wire::Chat,
            "responses" => qaqh_types::Wire::Responses,
            "messages" => qaqh_types::Wire::Messages,
            "gemini" => qaqh_types::Wire::Gemini,
            other => {
                self.toast(NoticeLevel::Error, format!("未知 wire 拼写: {other:?}"));
                return;
            }
        };
        let credential = match credential.as_str() {
            "api_key" => qaqh_types::CredentialSource::ApiKey,
            "chatgpt_plan" => qaqh_types::CredentialSource::ChatgptPlan,
            "deepseek_account" => qaqh_types::CredentialSource::DeepseekAccount,
            other => {
                self.toast(NoticeLevel::Error, format!("未知凭据来源: {other:?}"));
                return;
            }
        };
        self.catalog_write(
            RingingCommand::ProviderUpsert {
                id: id.clone(),
                provider: qaqh_ringing::ProviderUpsertDto {
                    name,
                    base_url,
                    wire,
                    credential,
                    headers: Default::default(),
                    compat: None,
                    api_key: if api_key.is_empty() {
                        qaqh_ringing::SecretWrite::Keep
                    } else {
                        qaqh_ringing::SecretWrite::Set(api_key)
                    },
                },
            },
            "保存 provider",
        );
    }

    /// 删除模型（默认引用预检在端口激活处完成）。
    pub fn apply_delete_model(&mut self, provider_id: String, model_id: String) {
        self.catalog_write(
            RingingCommand::ProviderModelDelete {
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
            },
            "删除模型",
        );
    }

    pub fn set_permission_level(&mut self, level: u8) {
        // 最新 ConfigPatch 接受 1..=4；旧独立写口仍只接受 1..=3。
        // 使用相同的配置事务与 reload 广播，同时支持 sandbox-run。
        self.spawn_api(move |api, tx| async move {
            let result = api
                // backend-v3：权限档位有专用命令（值域 1..=4）。
                .config_action(RingingCommand::ConfigSetPermissionLevel { level })
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
            // 目录快照进设置页（模型目录分区的候选来源），同时拉一次最新的
            // ——目录分区是即时写，快照越新，CAS 冲突越少。
            self.fetch_provider_catalog();
            self.overlays.push(Overlay::Settings(SettingsState {
                local_loaded: self.local_config,
                catalog: self.provider_catalog.clone(),
                ..SettingsState::default()
            }));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saved_oauth_snapshot_is_applied_before_draft_is_cleared() {
        let (mut app, _rx) = App::new_for_test();
        app.config = Some(ConfigDto {
            credential: "api_key".into(),
            ..Default::default()
        });
        let mut st = SettingsState::default();
        st.draft.credential = Some("chatgpt_plan".into());
        app.overlays.push(Overlay::Settings(st));
        app.settings_saving = true;
        let dto = ConfigDto {
            credential: "chatgpt_plan".into(),
            wire: "responses".into(),
            ..Default::default()
        };
        app.handle(AppMsg::Action(ActionResult::ConfigWrite {
            label: "设置",
            result: Ok(serde_json::to_value(dto).unwrap()),
        }));
        let Some(Overlay::Settings(st)) = app.overlays.last() else {
            panic!("settings")
        };
        assert!(st.draft.is_empty());
        assert_eq!(st.effective_credential(app.config.as_ref()), "chatgpt_plan");
        assert!(!app.settings_saving);
    }

    #[tokio::test]
    async fn failed_settings_readback_keeps_oauth_draft() {
        let (mut app, _rx) = App::new_for_test();
        let mut st = SettingsState::default();
        st.draft.credential = Some("chatgpt_plan".into());
        app.overlays.push(Overlay::Settings(st));
        app.settings_saving = true;
        app.handle(AppMsg::Action(ActionResult::ConfigWrite {
            label: "设置",
            result: Ok(serde_json::Value::Null),
        }));
        let Some(Overlay::Settings(st)) = app.overlays.last() else {
            panic!("settings")
        };
        assert_eq!(st.draft.credential.as_deref(), Some("chatgpt_plan"));
        assert!(!app.settings_saving);
    }

    #[tokio::test]
    async fn account_settings_mouse_and_keyboard_keep_the_new_overlay() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let row = settings::ROWS
            .iter()
            .position(|r| r.id == settings::FieldId::Account)
            .unwrap();
        for mouse in [false, true] {
            let (mut app, _rx) = App::new_for_test();
            // v3：凭据来源来自 loaded 读模型（目录接管后 draft 不再承载 credential）。
            app.config = Some(ConfigDto {
                credential: "chatgpt_plan".into(),
                ..Default::default()
            });
            let mut st = SettingsState::default();
            st.focus = row;
            app.overlays.push(Overlay::Settings(st));
            if mouse {
                app.mouse_settings_row(row);
            } else {
                app.handle(AppMsg::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )));
            }
            assert!(matches!(
                app.overlays.as_slice(),
                [Overlay::Settings(_), Overlay::Account(_)]
            ));
            app.account_action(account::AccountAction::Close);
            assert!(matches!(
                app.overlays.last(),
                Some(Overlay::Settings(_))
            ), "返回必须回到设置");
        }
    }

    #[tokio::test]
    async fn f10_render_choice_can_be_saved_without_control_comma() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let root = std::env::temp_dir().join(format!("qaqh-f10-{}", uuid::Uuid::new_v4()));
        let (mut app, mut rx) = App::new_for_test();
        app.config = Some(ConfigDto::default());
        app.local_store = Some(crate::local_config::Store::for_test(&root));
        let press =
            |app: &mut App, code| app.handle(AppMsg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        press(&mut app, KeyCode::F(10));
        let row = settings::ROWS
            .iter()
            .position(|row| row.id == settings::FieldId::PerformanceMode)
            .unwrap();
        for _ in 0..row {
            press(&mut app, KeyCode::Down);
        }
        let Some(Overlay::Settings(st)) = app.overlays.last() else {
            panic!("F10 must open settings")
        };
        assert_eq!(st.row().label, "渲染");
        assert_eq!(st.display(app.config.as_ref(), st.row().id), "动效模式");
        press(&mut app, KeyCode::Right);
        let Some(Overlay::Settings(st)) = app.overlays.last() else {
            panic!("settings")
        };
        assert_eq!(st.display(app.config.as_ref(), st.row().id), "性能模式");
        assert!(st.draft.is_empty());
        press(&mut app, KeyCode::Char('s'));
        assert!(app.local_config.performance_mode);
        press(&mut app, KeyCode::F(10));
        press(&mut app, KeyCode::F(10));
        let Some(Overlay::Settings(st)) = app.overlays.last() else {
            panic!("settings")
        };
        assert_eq!(
            st.display(app.config.as_ref(), settings::FieldId::PerformanceMode),
            "性能模式"
        );
        for _ in 0..row {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert!(
            app.local_config.performance_mode,
            "Esc discards the unsaved choice"
        );
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "本地渲染选择不得调用 daemon");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn performance_only_save_is_local_and_sends_no_runtime_message() {
        let root = std::env::temp_dir().join(format!("qaqh-settings-{}", uuid::Uuid::new_v4()));
        let (mut app, mut rx) = App::new_for_test();
        app.local_store = Some(crate::local_config::Store::for_test(&root));
        let mut st = SettingsState::default();
        st.focus = settings::ROWS
            .iter()
            .position(|r| r.id == settings::FieldId::PerformanceMode)
            .unwrap();
        st.cycle(None, 1).unwrap();
        assert!(st.draft.is_empty(), "本地配置不能污染 daemon patch");
        assert!(st.has_changes());
        app.save_settings(&mut st);
        assert!(app.local_config.performance_mode);
        assert!(!app.settings_saving);
        assert!(!st.has_changes());
        assert_eq!(
            app.local_store.as_ref().unwrap().load().unwrap(),
            app.local_config
        );
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "不得发送 config.save/config.load 请求"
        );
        // 再次保存可以原子替换既有文件。
        st.cycle(None, 1).unwrap();
        app.save_settings(&mut st);
        assert!(!app.local_config.performance_mode);
        assert!(
            !app.local_store
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .performance_mode
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn local_save_failure_keeps_draft_and_runtime_mode() {
        let (mut app, mut rx) = App::new_for_test();
        let mut st = SettingsState {
            performance_draft: Some(true),
            ..SettingsState::default()
        };
        app.save_settings(&mut st);
        assert!(!app.local_config.performance_mode);
        assert_eq!(st.performance_draft, Some(true));
        assert!(app.toasts.back().unwrap().text.contains("本地配置保存失败"));
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
    }
}
