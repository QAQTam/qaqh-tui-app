//! 设置页状态（`Overlay::Settings`）：行模型 + 草稿脏字段 + 编辑缓冲。
//!
//! 写纪律（镜像后端 `docs/config-revamp-plan.md` 硬性约束）：
//! - 文本/数值/枚举/开关字段：编辑只累积 [`ConfigPatch`] 脏字段（K3 Merge
//!   Patch），`s` 一次性 `config.save`——**禁止整包写回**；
//! - permissionLevel / activeProfile / workspace.mode 是服务端独立写端口，
//!   不在 Patch 内：回车/数字即时生效（App 层发起 service 调用）；
//! - apiKey 只进不出：掩码/空 = 保持现值，用户显式输入才写；
//! - ConfigChanged 重拉只替换 loaded 快照，脏字段草稿值优先（B5 回声拉回教训）。

use crate::protocol::config::{ConfigDto, ConfigPatch, SubagentPatch};

/// 后端 `validate` 允许的思考强度枚举。
pub const REASONING_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// 可聚焦字段的稳定标识（行序即 UI 顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    Model,
    Provider,
    Endpoint,
    BaseUrl,
    ApiKey,
    MaxTokens,
    ContextLimit,
    ReasoningEffort,
    AutoCompactThreshold,
    PermissionLevel,
    ActiveProfile,
    WorkspaceMode,
    SubModel,
    SubBaseUrl,
    SubMaxTokens,
    SubTimeoutSecs,
    SubApiKey,
    SubDefaultTools,
    Lang,
    Theme,
    FontFamily,
    NotificationsEnabled,
    ComplianceEnabled,
    TokenizerPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 自由文本（Enter 编辑）。
    Text,
    /// 密钥类：输入即替换，空提交 = 保持现值。
    Secret,
    /// 非零整数。
    Number,
    /// [0,1] 浮点，0 = 关闭。
    Float,
    /// ←→/Enter 循环切换（写入草稿）。
    Enum,
    /// 布尔开关（写入草稿）。
    Toggle,
    /// 服务端独立写端口（回车/数字即时生效，不进草稿）。
    Port,
}

#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub id: FieldId,
    pub label: &'static str,
    pub kind: FieldKind,
    pub section: &'static str,
}

pub const ROWS: &[Row] = &[
    Row { id: FieldId::Model, label: "模型", kind: FieldKind::Text, section: "模型与提供商" },
    Row { id: FieldId::Provider, label: "提供商", kind: FieldKind::Enum, section: "模型与提供商" },
    Row { id: FieldId::Endpoint, label: "端点", kind: FieldKind::Enum, section: "模型与提供商" },
    Row { id: FieldId::BaseUrl, label: "Base URL", kind: FieldKind::Text, section: "模型与提供商" },
    Row { id: FieldId::ApiKey, label: "API Key", kind: FieldKind::Secret, section: "模型与提供商" },
    Row { id: FieldId::MaxTokens, label: "maxTokens", kind: FieldKind::Number, section: "生成参数" },
    Row { id: FieldId::ContextLimit, label: "contextLimit", kind: FieldKind::Number, section: "生成参数" },
    Row { id: FieldId::ReasoningEffort, label: "思考强度", kind: FieldKind::Enum, section: "生成参数" },
    Row { id: FieldId::AutoCompactThreshold, label: "自动压缩阈值", kind: FieldKind::Float, section: "生成参数" },
    Row { id: FieldId::PermissionLevel, label: "权限级别", kind: FieldKind::Port, section: "运行时" },
    Row { id: FieldId::ActiveProfile, label: "Profile", kind: FieldKind::Port, section: "运行时" },
    Row { id: FieldId::WorkspaceMode, label: "workspace 模式", kind: FieldKind::Port, section: "运行时" },
    Row { id: FieldId::SubModel, label: "子代理模型", kind: FieldKind::Text, section: "子代理" },
    Row { id: FieldId::SubBaseUrl, label: "子代理 URL", kind: FieldKind::Text, section: "子代理" },
    Row { id: FieldId::SubMaxTokens, label: "子代理 maxTokens", kind: FieldKind::Number, section: "子代理" },
    Row { id: FieldId::SubTimeoutSecs, label: "子代理超时(s)", kind: FieldKind::Number, section: "子代理" },
    Row { id: FieldId::SubApiKey, label: "子代理 Key", kind: FieldKind::Secret, section: "子代理" },
    Row { id: FieldId::SubDefaultTools, label: "子代理工具", kind: FieldKind::Text, section: "子代理" },
    Row { id: FieldId::Lang, label: "语言", kind: FieldKind::Text, section: "通用" },
    Row { id: FieldId::Theme, label: "主题", kind: FieldKind::Text, section: "通用" },
    Row { id: FieldId::FontFamily, label: "字体", kind: FieldKind::Text, section: "通用" },
    Row { id: FieldId::NotificationsEnabled, label: "桌面通知", kind: FieldKind::Toggle, section: "通用" },
    Row { id: FieldId::ComplianceEnabled, label: "合规模式", kind: FieldKind::Toggle, section: "通用" },
    Row { id: FieldId::TokenizerPath, label: "tokenizer 路径", kind: FieldKind::Text, section: "通用" },
];

/// 单行文本编辑缓冲（沿用 AttachPath 的 Vec<char> + cursor 模式）。
#[derive(Debug, Clone, Default)]
pub struct EditBuffer {
    pub buf: Vec<char>,
    pub cursor: usize,
}

/// 设置页 UI 状态。随 `Overlay::Settings` 持有，Esc 关闭即丢弃草稿（取消）。
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub focus: usize,
    pub editing: Option<EditBuffer>,
    /// 脏字段累积器（K3：只发改过的字段）。
    pub draft: ConfigPatch,
    /// Profile 端口的候选名（None = 展示服务端现值）。
    pub profile_sel: Option<String>,
    /// workspace 模式端口的候选值。
    pub ws_sel: Option<String>,
}

impl SettingsState {
    pub fn row(&self) -> &'static Row {
        &ROWS[self.focus.min(ROWS.len() - 1)]
    }

    pub fn move_focus(&mut self, delta: i32) {
        let n = ROWS.len() as i32;
        let next = (self.focus as i32 + delta).rem_euclid(n);
        self.focus = next as usize;
    }

    /// 当前字段是否已有未保存草稿值。
    pub fn dirty(&self, id: FieldId) -> bool {
        match id {
            FieldId::Model => self.draft.model.is_some(),
            FieldId::Provider => self.draft.provider_id.is_some(),
            FieldId::Endpoint => self.draft.endpoint.is_some(),
            FieldId::BaseUrl => self.draft.base_url.is_some(),
            FieldId::ApiKey => self.draft.api_key.is_some(),
            FieldId::MaxTokens => self.draft.max_tokens.is_some(),
            FieldId::ContextLimit => self.draft.context_limit.is_some(),
            FieldId::ReasoningEffort => self.draft.reasoning_effort.is_some(),
            FieldId::AutoCompactThreshold => self.draft.auto_compact_threshold.is_some(),
            FieldId::Lang => self.draft.lang.is_some(),
            FieldId::Theme => self.draft.theme.is_some(),
            FieldId::FontFamily => self.draft.font_family.is_some(),
            FieldId::NotificationsEnabled => self.draft.notifications_enabled.is_some(),
            FieldId::ComplianceEnabled => self.draft.compliance_enabled.is_some(),
            FieldId::TokenizerPath => self.draft.tokenizer_path.is_some(),
            FieldId::SubModel | FieldId::SubBaseUrl | FieldId::SubApiKey | FieldId::SubMaxTokens | FieldId::SubTimeoutSecs | FieldId::SubDefaultTools => {
                let Some(sub) = &self.draft.subagent else { return false };
                match id {
                    FieldId::SubModel => sub.model.is_some(),
                    FieldId::SubBaseUrl => sub.base_url.is_some(),
                    FieldId::SubApiKey => sub.api_key.is_some(),
                    FieldId::SubMaxTokens => sub.max_tokens.is_some(),
                    FieldId::SubTimeoutSecs => sub.timeout_secs.is_some(),
                    FieldId::SubDefaultTools => sub.default_tools.is_some(),
                    _ => false,
                }
            }
            // 端口字段即时生效，无草稿。
            FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::WorkspaceMode => false,
        }
    }

    /// 展示值：草稿值优先于 loaded 快照。
    pub fn display(&self, loaded: Option<&ConfigDto>, id: FieldId) -> String {
        let d = &self.draft;
        match id {
            FieldId::Model => owned_or(d.model.clone(), loaded.map(|c| c.model.as_str()), "—"),
            FieldId::Provider => owned_or(d.provider_id.clone(), loaded.map(|c| c.provider_id.as_str()), "—"),
            FieldId::Endpoint => owned_or(d.endpoint.clone(), loaded.map(|c| c.endpoint.as_str()), "—"),
            FieldId::BaseUrl => owned_or(d.base_url.clone(), loaded.map(|c| c.base_url.as_str()), "—"),
            FieldId::ApiKey => match (&d.api_key, loaded) {
                (Some(_), _) => "●●●●（待保存）".into(),
                (None, Some(c)) if c.api_key == "****" => "(已配置 ****)".into(),
                (None, _) => "(未配置)".into(),
            },
            FieldId::MaxTokens => num_or(d.max_tokens, loaded.map(|c| c.max_tokens)),
            FieldId::ContextLimit => num_or(d.context_limit, loaded.map(|c| c.context_limit)),
            FieldId::ReasoningEffort => owned_or(
                d.reasoning_effort.clone(),
                loaded.map(|c| c.reasoning_effort.as_str()).filter(|s| !s.is_empty()),
                "—",
            ),
            FieldId::AutoCompactThreshold => {
                let v = d.auto_compact_threshold.or(loaded.map(|c| c.auto_compact_threshold));
                match v {
                    None => "—".into(),
                    Some(0.0) => "0（关闭）".into(),
                    Some(v) => format!("{v:.2}"),
                }
            }
            FieldId::PermissionLevel => loaded
                .map(|c| format!("L{}（按 1-4 即时生效）", c.permission_level))
                .unwrap_or_else(|| "…".into()),
            FieldId::ActiveProfile => {
                let cur = self
                    .profile_sel
                    .clone()
                    .or_else(|| loaded.map(|c| c.active_profile.clone()));
                match cur {
                    Some(name) => {
                        let active = loaded.map(|c| c.active_profile.as_str() == name.as_str()).unwrap_or(true);
                        if active { name } else { format!("{name}（回车应用）") }
                    }
                    None => "…".into(),
                }
            }
            FieldId::WorkspaceMode => {
                let cur = self.ws_sel.clone().or_else(|| loaded.map(|c| c.workspace.mode.clone()));
                match cur {
                    Some(mode) => {
                        let active = loaded.map(|c| c.workspace.mode == mode).unwrap_or(true);
                        if active { mode } else { format!("{mode}（回车应用）") }
                    }
                    None => "…".into(),
                }
            }
            FieldId::SubModel => sub_or(d, loaded, |s, c| (s.model.clone(), c.model.clone()), "—"),
            FieldId::SubBaseUrl => sub_or(d, loaded, |s, c| (s.base_url.clone(), c.base_url.clone()), "—"),
            FieldId::SubMaxTokens => {
                let v = d.subagent.as_ref().and_then(|s| s.max_tokens).or(loaded.map(|c| c.subagent.max_tokens));
                num_or(v, v)
            }
            FieldId::SubTimeoutSecs => {
                let v = d.subagent.as_ref().and_then(|s| s.timeout_secs).or(loaded.map(|c| c.subagent.timeout_secs));
                num_or(v, v)
            }
            FieldId::SubApiKey => match (d.subagent.as_ref().and_then(|s| s.api_key.clone()), loaded) {
                (Some(_), _) => "●●●●（待保存）".into(),
                (None, Some(c)) if c.subagent.api_key_set => "(已配置 ****)".into(),
                (None, _) => "(未配置)".into(),
            },
            FieldId::SubDefaultTools => {
                let draft_val = d.subagent.as_ref().and_then(|s| s.default_tools.clone());
                let loaded_val = loaded.map(|c| c.subagent.default_tools.clone());
                let tools = draft_val.or(loaded_val);
                match tools {
                    None => "…".into(),
                    Some(v) if v.is_empty() => "(全部工具)".into(),
                    Some(v) => v.join(", "),
                }
            }
            FieldId::Lang => opt_str(d.lang.clone(), loaded.and_then(|c| c.lang.clone())),
            FieldId::Theme => opt_str(d.theme.clone(), loaded.and_then(|c| c.theme.clone())),
            FieldId::FontFamily => owned_or(d.font_family.clone(), loaded.map(|c| c.font_family.as_str()).filter(|s| !s.is_empty()), "—"),
            FieldId::NotificationsEnabled => toggle_str(d.notifications_enabled, loaded.map(|c| c.notifications_enabled)),
            FieldId::ComplianceEnabled => toggle_str(d.compliance_enabled, loaded.map(|c| c.compliance_enabled)),
            FieldId::TokenizerPath => opt_str(d.tokenizer_path.clone(), loaded.and_then(|c| c.tokenizer_path.clone())),
        }
    }

    /// 开始编辑：Text/Secret/Number/Float 返回预填缓冲（Secret 恒空），
    /// Enum/Toggle/Port 返回 None（由 cycle/端口逻辑处理）。
    pub fn start_edit(&self, loaded: Option<&ConfigDto>) -> Option<EditBuffer> {
        let seed = match self.row().id {
            FieldId::Model => self.effective(loaded, |d, c| d.model.clone().unwrap_or_else(|| c.model.clone())),
            FieldId::BaseUrl => self.effective(loaded, |d, c| d.base_url.clone().unwrap_or_else(|| c.base_url.clone())),
            FieldId::SubModel => self.effective(loaded, |d, c| {
                d.subagent.as_ref().and_then(|s| s.model.clone()).unwrap_or_else(|| c.subagent.model.clone())
            }),
            FieldId::SubBaseUrl => self.effective(loaded, |d, c| {
                d.subagent.as_ref().and_then(|s| s.base_url.clone()).unwrap_or_else(|| c.subagent.base_url.clone())
            }),
            FieldId::Lang => self.effective(loaded, |d, c| d.lang.clone().unwrap_or_else(|| c.lang.clone().unwrap_or_default())),
            FieldId::Theme => self.effective(loaded, |d, c| d.theme.clone().unwrap_or_else(|| c.theme.clone().unwrap_or_default())),
            FieldId::FontFamily => self.effective(loaded, |d, c| d.font_family.clone().unwrap_or_else(|| c.font_family.clone())),
            FieldId::TokenizerPath => self.effective(loaded, |d, c| {
                d.tokenizer_path.clone().unwrap_or_else(|| c.tokenizer_path.clone().unwrap_or_default())
            }),
            FieldId::MaxTokens => self.effective(loaded, |d, c| d.max_tokens.unwrap_or(c.max_tokens).to_string()),
            FieldId::ContextLimit => self.effective(loaded, |d, c| d.context_limit.unwrap_or(c.context_limit).to_string()),
            FieldId::SubMaxTokens => self.effective(loaded, |d, c| {
                d.subagent.as_ref().and_then(|s| s.max_tokens).unwrap_or(c.subagent.max_tokens).to_string()
            }),
            FieldId::SubTimeoutSecs => self.effective(loaded, |d, c| {
                d.subagent.as_ref().and_then(|s| s.timeout_secs).unwrap_or(c.subagent.timeout_secs).to_string()
            }),
            FieldId::AutoCompactThreshold => {
                self.effective(loaded, |d, c| d.auto_compact_threshold.unwrap_or(c.auto_compact_threshold).to_string())
            }
            FieldId::SubDefaultTools => self.effective(loaded, |d, c| {
                let v = d.subagent.as_ref().and_then(|s| s.default_tools.clone()).unwrap_or_else(|| c.subagent.default_tools.clone());
                if v.is_empty() { String::new() } else { v.join(", ") }
            }),
            FieldId::ApiKey | FieldId::SubApiKey => String::new(),
            FieldId::Provider | FieldId::Endpoint | FieldId::ReasoningEffort
            | FieldId::NotificationsEnabled | FieldId::ComplianceEnabled
            | FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::WorkspaceMode => return None,
        };
        let cursor = seed.chars().count();
        Some(EditBuffer { buf: seed.chars().collect(), cursor })
    }

    /// 提交编辑缓冲到草稿（逐字段校验；失败返回 Err 且不落地）。
    pub fn commit_edit(&mut self, loaded: Option<&ConfigDto>, buf: EditBuffer) -> Result<(), String> {
        let id = self.row().id;
        let raw: String = buf.buf.iter().collect();
        let text = raw.trim().to_string();
        match id {
            FieldId::MaxTokens | FieldId::ContextLimit | FieldId::SubMaxTokens | FieldId::SubTimeoutSecs => {
                let v: u64 = text.parse().map_err(|_| format!("{text:?} 不是有效整数"))?;
                if v == 0 {
                    return Err("数值必须大于 0".into());
                }
                self.set_sub_or_top(id, v);
            }
            FieldId::AutoCompactThreshold => {
                let v: f64 = text.parse().map_err(|_| format!("{text:?} 不是有效数字"))?;
                if v.is_nan() || !(0.0..=1.0).contains(&v) {
                    return Err("自动压缩阈值必须在 [0, 1]（0 = 关闭）".into());
                }
                self.draft.auto_compact_threshold = Some(v);
            }
            FieldId::ApiKey => {
                // 空提交 = 保持现值（守卫语义）；只有显式输入才写。
                if !text.is_empty() {
                    self.draft.api_key = Some(text);
                }
            }
            FieldId::SubApiKey => {
                if !text.is_empty() {
                    self.draft.subagent.get_or_insert_with(SubagentPatch::default).api_key = Some(text);
                }
            }
            FieldId::Lang | FieldId::Theme => {
                // Some("") = 清除（跟随系统）。
                let v = Some(text);
                if id == FieldId::Lang {
                    self.draft.lang = v;
                } else {
                    self.draft.theme = v;
                }
            }
            FieldId::Model => self.draft.model = Some(text),
            FieldId::BaseUrl => self.draft.base_url = Some(text),
            FieldId::FontFamily => self.draft.font_family = Some(text),
            FieldId::TokenizerPath => self.draft.tokenizer_path = Some(text),
            FieldId::SubModel => self.draft.subagent.get_or_insert_with(SubagentPatch::default).model = Some(text),
            FieldId::SubBaseUrl => self.draft.subagent.get_or_insert_with(SubagentPatch::default).base_url = Some(text),
            FieldId::SubDefaultTools => {
                // 空输入 = 全部工具（Some([])），逗号分隔，非空则按逗号切分去空白。
                let tools = if text.is_empty() {
                    Vec::new()
                } else {
                    text.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                self.draft.subagent.get_or_insert_with(SubagentPatch::default).default_tools = Some(tools);
            }
            // 不可编辑字段：静默忽略（理论上不会到达）。
            FieldId::Provider | FieldId::Endpoint | FieldId::ReasoningEffort
            | FieldId::NotificationsEnabled | FieldId::ComplianceEnabled
            | FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::WorkspaceMode => {
                let _ = loaded;
            }
        }
        Ok(())
    }

    /// 循环切换（←→ / Enter）。返回 Ok(true) = 已消费；Ok(false) = 端口字段，
    /// 由 App 层处理；Err = 提示性失败（如目录为空）。
    pub fn cycle(&mut self, loaded: Option<&ConfigDto>, delta: i32) -> Result<bool, String> {
        let id = self.row().id;
        match id {
            FieldId::ReasoningEffort => {
                let cur = self
                    .draft
                    .reasoning_effort
                    .clone()
                    .or_else(|| loaded.map(|c| c.reasoning_effort.clone()))
                    .unwrap_or_else(|| "medium".into());
                let idx = REASONING_EFFORTS.iter().position(|e| *e == cur).unwrap_or(1);
                let next = (idx as i32 + delta).rem_euclid(REASONING_EFFORTS.len() as i32) as usize;
                self.draft.reasoning_effort = Some(REASONING_EFFORTS[next].to_string());
                Ok(true)
            }
            FieldId::NotificationsEnabled => {
                let cur = self.draft.notifications_enabled.or(loaded.map(|c| c.notifications_enabled)).unwrap_or(true);
                self.draft.notifications_enabled = Some(!cur);
                Ok(true)
            }
            FieldId::ComplianceEnabled => {
                let cur = self.draft.compliance_enabled.or(loaded.map(|c| c.compliance_enabled)).unwrap_or(false);
                self.draft.compliance_enabled = Some(!cur);
                Ok(true)
            }
            FieldId::Provider => {
                let cfg = loaded.ok_or_else(|| "配置未加载".to_string())?;
                if cfg.providers.is_empty() {
                    return Err("daemon 未提供 provider 目录".into());
                }
                let cur = self.draft.provider_id.as_deref().unwrap_or(cfg.provider_id.as_str());
                let idx = cfg.providers.iter().position(|p| p.id == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(cfg.providers.len() as i32) as usize;
                let p = &cfg.providers[next];
                self.draft.provider_id = Some(p.id.clone());
                // 跟随 provider 预设：端点与 Base URL 一并落入草稿（用户仍可改）。
                if let Some(ep) = p.endpoints.first() {
                    self.draft.endpoint = Some(ep.id.clone());
                    if !ep.base_url.is_empty() {
                        self.draft.base_url = Some(ep.base_url.clone());
                    }
                }
                Ok(true)
            }
            FieldId::Endpoint => {
                let cfg = loaded.ok_or_else(|| "配置未加载".to_string())?;
                let Some(p) = self.effective_provider(cfg) else {
                    return Err("当前 provider 不在目录中".into());
                };
                if p.endpoints.is_empty() {
                    return Err("该 provider 无端点预设".into());
                }
                let cur = self.draft.endpoint.as_deref().unwrap_or(cfg.endpoint.as_str());
                let idx = p.endpoints.iter().position(|e| e.id == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(p.endpoints.len() as i32) as usize;
                let ep = &p.endpoints[next];
                self.draft.endpoint = Some(ep.id.clone());
                if !ep.base_url.is_empty() {
                    self.draft.base_url = Some(ep.base_url.clone());
                }
                Ok(true)
            }
            FieldId::Model => {
                // ←→ 在当前端点的 models 列表里循环；Enter 自由输入。
                let cfg = loaded.ok_or_else(|| "配置未加载".to_string())?;
                let models = self.effective_provider(cfg).and_then(|p| {
                    let eid = self.draft.endpoint.as_deref().unwrap_or(cfg.endpoint.as_str());
                    p.endpoints.iter().find(|e| e.id == eid).or_else(|| p.endpoints.first()).map(|e| &e.models)
                });
                match models.filter(|m| !m.is_empty()) {
                    Some(models) => {
                        let cur = self.draft.model.as_deref().unwrap_or(cfg.model.as_str());
                        let idx = models.iter().position(|m| m == cur).unwrap_or(0);
                        let next = (idx as i32 + delta).rem_euclid(models.len() as i32) as usize;
                        self.draft.model = Some(models[next].clone());
                        Ok(true)
                    }
                    None => Err("端点无模型列表——回车直接输入".into()),
                }
            }
            FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::WorkspaceMode => Ok(false),
            _ => Ok(false),
        }
    }

    fn effective_provider<'a>(&self, cfg: &'a ConfigDto) -> Option<&'a crate::protocol::config::ProviderDto> {
        let pid = self.draft.provider_id.as_deref().unwrap_or(cfg.provider_id.as_str());
        cfg.providers
            .iter()
            .find(|p| p.id == pid)
            .or_else(|| cfg.providers.first())
    }

    fn effective<T>(&self, loaded: Option<&ConfigDto>, f: impl Fn(&ConfigPatch, &ConfigDto) -> T) -> T {
        match loaded {
            Some(c) => f(&self.draft, c),
            None => {
                // 未加载：草稿有值用草稿，否则空。借一个空 DTO 复用 f。
                let empty = ConfigDto::default();
                f(&self.draft, &empty)
            }
        }
    }

    fn set_sub_or_top(&mut self, id: FieldId, v: u64) {
        match id {
            FieldId::MaxTokens => self.draft.max_tokens = Some(v),
            FieldId::ContextLimit => self.draft.context_limit = Some(v),
            FieldId::SubMaxTokens => self.draft.subagent.get_or_insert_with(SubagentPatch::default).max_tokens = Some(v),
            FieldId::SubTimeoutSecs => self.draft.subagent.get_or_insert_with(SubagentPatch::default).timeout_secs = Some(v),
            _ => {}
        }
    }
}

fn owned_or(draft: Option<String>, loaded: Option<&str>, empty: &str) -> String {
    if let Some(v) = draft {
        return v;
    }
    match loaded {
        Some(v) if !v.is_empty() => v.to_owned(),
        _ => empty.to_owned(),
    }
}

fn num_or(draft: Option<u64>, loaded: Option<u64>) -> String {
    match draft.or(loaded) {
        Some(v) => v.to_string(),
        None => "…".into(),
    }
}

/// lang/theme/tokenizer：None 或 Some("") 一律显示「跟随系统」语义。
fn opt_str(draft: Option<String>, loaded: Option<String>) -> String {
    let v = draft.or(loaded);
    match v {
        Some(s) if !s.is_empty() => s,
        _ => "(跟随系统)".into(),
    }
}

fn toggle_str(draft: Option<bool>, loaded: Option<bool>) -> String {
    let on = draft.or(loaded).unwrap_or(false);
    if on { "[x] 开".into() } else { "[ ] 关".into() }
}

fn sub_or(
    d: &ConfigPatch,
    loaded: Option<&ConfigDto>,
    pick: impl Fn(&SubagentPatch, &crate::protocol::config::SubagentDto) -> (Option<String>, String),
    empty: &str,
) -> String {
    let (draft, loaded) = match (d.subagent.as_ref(), loaded) {
        (Some(s), Some(c)) => pick(s, &c.subagent),
        (Some(s), None) => (pick(s, &Default::default()).0, String::new()),
        (None, Some(c)) => (None, pick(&SubagentPatch::default(), &c.subagent).1),
        (None, None) => return empty.to_owned(),
    };
    match draft {
        Some(v) => v,
        None if !loaded.is_empty() => loaded,
        None => empty.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ConfigDto {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5",
            "baseUrl": "https://api.example.com/v1",
            "providerId": "prov-a",
            "endpoint": "ep-a1",
            "maxTokens": 96000,
            "contextLimit": 1000000,
            "reasoningEffort": "high",
            "autoCompactThreshold": 0.95,
            "permissionLevel": 3,
            "apiKey": "****",
            "activeProfile": "default",
            "profiles": ["default", "fast"],
            "notificationsEnabled": true,
            "complianceEnabled": false,
            "providers": [
                { "id": "prov-a", "display": "A", "endpoints": [
                    { "id": "ep-a1", "display": "A1", "protocol": "openai", "baseUrl": "https://api.example.com/v1", "defaultModel": "", "models": ["gpt-5", "gpt-5-mini"], "stateful": false, "beta": false }
                ]},
                { "id": "prov-b", "display": "B", "endpoints": [
                    { "id": "ep-b1", "display": "B1", "protocol": "openai", "baseUrl": "https://b.example.com/v1", "defaultModel": "", "models": ["b1"], "stateful": false, "beta": false }
                ]}
            ],
            "subagent": { "model": "", "baseUrl": "", "api_key": "", "apiKeySet": false, "maxTokens": 4096, "timeoutSecs": 120, "defaultTools": [] },
            "workspace": { "mode": "local" }
        }))
        .unwrap()
    }

    fn row_index(id: FieldId) -> usize {
        ROWS.iter().position(|r| r.id == id).unwrap()
    }

    #[test]
    fn focus_moves_and_wraps() {
        let mut st = SettingsState::default();
        st.focus = row_index(FieldId::Model);
        st.move_focus(1);
        assert_eq!(st.row().id, FieldId::Provider);
        st.move_focus(-1);
        assert_eq!(st.row().id, FieldId::Model);
        st.focus = ROWS.len() - 1;
        st.move_focus(1);
        assert_eq!(st.focus, 0);
    }

    #[test]
    fn display_prefers_draft_over_loaded() {
        let c = cfg();
        let mut st = SettingsState::default();
        assert_eq!(st.display(Some(&c), FieldId::Model), "gpt-5");
        st.draft.model = Some("other".into());
        assert_eq!(st.display(Some(&c), FieldId::Model), "other");
        assert!(st.dirty(FieldId::Model));
        assert_eq!(st.display(Some(&c), FieldId::ApiKey), "(已配置 ****)");
        st.draft.api_key = Some("sk-new".into());
        assert_eq!(st.display(Some(&c), FieldId::ApiKey), "●●●●（待保存）");
        assert_eq!(st.display(Some(&c), FieldId::AutoCompactThreshold), "0.95");
        st.draft.auto_compact_threshold = Some(0.0);
        assert_eq!(st.display(Some(&c), FieldId::AutoCompactThreshold), "0（关闭）");
    }

    #[test]
    fn commit_edit_validates_ranges() {
        let c = cfg();
        let mut st = SettingsState::default();
        st.focus = row_index(FieldId::MaxTokens);
        let buf = |s: &str| EditBuffer { buf: s.chars().collect(), cursor: s.len() };
        assert!(st.commit_edit(Some(&c), buf("0")).is_err());
        assert!(st.commit_edit(Some(&c), buf("abc")).is_err());
        st.commit_edit(Some(&c), buf("128000")).unwrap();
        assert_eq!(st.draft.max_tokens, Some(128000));

        st.focus = row_index(FieldId::AutoCompactThreshold);
        assert!(st.commit_edit(Some(&c), buf("1.5")).is_err());
        st.commit_edit(Some(&c), buf("0")).unwrap();
        assert_eq!(st.draft.auto_compact_threshold, Some(0.0));

        // apiKey：空 = 保持，非空 = 写草稿。
        st.focus = row_index(FieldId::ApiKey);
        st.commit_edit(Some(&c), buf("")).unwrap();
        assert!(st.draft.api_key.is_none());
        st.commit_edit(Some(&c), buf("sk-new")).unwrap();
        assert_eq!(st.draft.api_key.as_deref(), Some("sk-new"));

        // lang：Some("") = 清除（跟随系统）。
        st.focus = row_index(FieldId::Lang);
        st.commit_edit(Some(&c), buf("")).unwrap();
        assert_eq!(st.draft.lang, Some(String::new()));
    }

    #[test]
    fn cycle_effort_toggles_and_providers() {
        let c = cfg();
        let mut st = SettingsState::default();

        st.focus = row_index(FieldId::ReasoningEffort);
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.reasoning_effort.as_deref(), Some("xhigh"));
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(st.draft.reasoning_effort.as_deref(), Some("high"));

        st.focus = row_index(FieldId::NotificationsEnabled);
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.notifications_enabled, Some(false));

        st.focus = row_index(FieldId::Provider);
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.provider_id.as_deref(), Some("prov-b"));
        assert_eq!(st.draft.endpoint.as_deref(), Some("ep-b1"));
        assert_eq!(st.draft.base_url.as_deref(), Some("https://b.example.com/v1"));
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(st.draft.provider_id.as_deref(), Some("prov-a"));

        st.focus = row_index(FieldId::Model);
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.model.as_deref(), Some("gpt-5-mini"));

        // 端口字段 cycle 返回 Ok(false)，由 App 层处理。
        st.focus = row_index(FieldId::ActiveProfile);
        assert_eq!(st.cycle(Some(&c), 1).unwrap(), false);
    }

    #[test]
    fn patch_roundtrip_and_validation_on_save() {
        let c = cfg();
        let mut st = SettingsState::default();
        st.focus = row_index(FieldId::ContextLimit);
        st.commit_edit(Some(&c), EditBuffer { buf: "2000000".chars().collect(), cursor: 7 }).unwrap();
        assert!(!st.draft.is_empty());
        st.draft.validate().unwrap();
        let v = st.draft.to_json();
        assert_eq!(v["contextLimit"], 2_000_000);
        assert!(v.get("model").is_none(), "未改动字段不得出现在 wire 上");
    }
}
