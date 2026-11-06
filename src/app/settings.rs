//! 设置页状态（`Overlay::Settings`）：行模型 + 草稿脏字段 + 编辑缓冲。
//!
//! 写纪律（对齐 qaqh-electron 的「两套读模型、三个写口」+ 后端 E25/E26 裁定）：
//! - **应用设置**：文本/数值/枚举/开关字段只累积 [`ConfigPatch`] 脏字段（K3
//!   Merge Patch），`s` 一次性 `config.save`——禁止整包写回；
//! - **模型目录**（provider/模型/默认模型）：`provider.*` 与
//!   `model.selection.set_default` **即时写**（`config_revision` CAS），不进
//!   config.save 草稿——目录权威是 `provider.list`，不是 ConfigDto；
//! - permissionLevel 是服务端独立写端口（数字即时生效）；
//! - apiKey 只进不出（SecretWrite::Set，空 = 保持现值）；
//! - ConfigChanged 重拉只替换 loaded 快照，脏字段草稿值优先（B5 回声拉回教训）。
//!
//! 历史（2026-11-06 v3 对齐）：旧「端点与模型（BYOK）」六字段 + activeProfile
//! 从 config.save 草稿整体退役——endpoint/协议/密钥/模型定义全部迁入 provider
//! 目录（`provider.upsert` / `provider.model.upsert`），全局默认模型走
//! `model.selection.set_default`（不再依赖 default_profile）。

use crate::protocol::{ConfigDto, ConfigPatch, ExecPatch, SubagentPatch};

/// provider 编辑表单的 wire 词表（`qaqh_types::Wire` 的规范 serde 拼写）。
///
/// v3 起这是**目录字段**（`provider.upsert` 的 `wire`），不再是 ConfigPatch 的
/// 旧拼写值域——规范名直发，无需任何读写侧拼写映射。
pub const WIRE_PROTOCOLS: [&str; 4] = ["chat", "responses", "messages", "gemini"];

/// provider 编辑表单的凭据来源（`qaqh_types::CredentialSource` 的 serde 拼写）。
pub const CREDENTIALS: [&str; 3] = ["api_key", "chatgpt_plan", "deepseek_account"];

/// 可聚焦字段的稳定标识（行序即 UI 顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    /// 账号登录（`chatgpt_plan` / `deepseek_account` 两档的登录与用量入口）。
    ///
    /// 它是**入口**而不是可编辑值：Enter 打开账号面板，值由
    /// `app::account::AccountBook` 的最近快照给出（`account_summary`）。
    Account,
    // ── 模型目录（即时写：provider.* / model.selection.set_default）──
    /// 全局默认模型：←→ 在目录模型间循环，Enter 应用（`set_default`）。
    DefaultModel,
    /// 新增模型：目标 provider（←→ 循环已有 provider）。
    AddModelProvider,
    /// 新增模型：模型 ID（手填；v3 已删预置目录，ID 由用户声明）。
    AddModelId,
    /// 新增模型：上下文窗口（tokens）。
    AddModelContext,
    /// 新增模型：单次输出上限（tokens）。
    AddModelMaxTokens,
    /// 新增模型：提交（`provider.model.upsert`）。
    AddModelApply,
    /// 新增 provider：ID（目录主键，[a-z0-9_-]）。
    NewProviderId,
    NewProviderName,
    NewProviderBaseUrl,
    /// provider 协议形态（规范名，直发）。
    NewProviderWire,
    /// provider 凭据来源。
    NewProviderCredential,
    /// provider API Key（SecretWrite::Set；空 = 不设/保持）。
    NewProviderApiKey,
    NewProviderApply,
    /// 删除模型：←→ 循环目录模型；Enter 提交删除（预检默认引用）。
    DeleteModelSel,
    DeleteModelApply,
    // ── 应用设置（config.save 草稿）──
    AutoCompactThreshold,
    SandboxEnabled,
    PermissionLevel,
    ExecDefaultShell,
    SessionIdleUnloadSecs,
    SubModel,
    SubBaseUrl,
    SubMaxTokens,
    SubTimeoutSecs,
    SubApiKey,
    SubDefaultTools,
    ComplianceEnabled,
    TokenizerPath,
    PerformanceMode,
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
    Row {
        id: FieldId::Account,
        label: "账号登录",
        kind: FieldKind::Port,
        section: "账号",
    },
    Row {
        id: FieldId::DefaultModel,
        label: "默认模型",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::AddModelProvider,
        label: "└ 目标 provider",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::AddModelId,
        label: "└ 模型 ID",
        kind: FieldKind::Text,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::AddModelContext,
        label: "└ 上下文窗口",
        kind: FieldKind::Number,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::AddModelMaxTokens,
        label: "└ 输出上限",
        kind: FieldKind::Number,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::AddModelApply,
        label: "└ 添加模型 ⏎",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderId,
        label: "新 provider ID",
        kind: FieldKind::Text,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderName,
        label: "└ 名称",
        kind: FieldKind::Text,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderBaseUrl,
        label: "└ Base URL",
        kind: FieldKind::Text,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderWire,
        label: "└ wire 协议",
        kind: FieldKind::Enum,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderCredential,
        label: "└ 凭据来源",
        kind: FieldKind::Enum,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderApiKey,
        label: "└ API Key",
        kind: FieldKind::Secret,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::NewProviderApply,
        label: "└ 保存 provider ⏎",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::DeleteModelSel,
        label: "删除模型",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::DeleteModelApply,
        label: "└ 确认删除 ⏎",
        kind: FieldKind::Port,
        section: "模型目录（即时生效）",
    },
    Row {
        id: FieldId::PerformanceMode,
        label: "渲染",
        kind: FieldKind::Enum,
        section: "TUI 本地（不发送 daemon）",
    },
    Row {
        id: FieldId::AutoCompactThreshold,
        label: "自动压缩阈值",
        kind: FieldKind::Float,
        section: "生成参数",
    },
    Row {
        id: FieldId::SandboxEnabled,
        label: "沙箱隔离",
        kind: FieldKind::Toggle,
        section: "运行时",
    },
    Row {
        id: FieldId::PermissionLevel,
        label: "权限级别",
        kind: FieldKind::Port,
        section: "运行时",
    },
    Row {
        id: FieldId::ExecDefaultShell,
        label: "exec 默认壳",
        kind: FieldKind::Text,
        section: "运行时",
    },
    Row {
        id: FieldId::SessionIdleUnloadSecs,
        label: "空闲卸载(s)",
        kind: FieldKind::Number,
        section: "运行时",
    },
    Row {
        id: FieldId::SubModel,
        label: "子代理模型",
        kind: FieldKind::Text,
        section: "子代理",
    },
    Row {
        id: FieldId::SubBaseUrl,
        label: "子代理 URL",
        kind: FieldKind::Text,
        section: "子代理",
    },
    Row {
        id: FieldId::SubMaxTokens,
        label: "子代理 maxTokens",
        kind: FieldKind::Number,
        section: "子代理",
    },
    Row {
        id: FieldId::SubTimeoutSecs,
        label: "子代理超时(s)",
        kind: FieldKind::Number,
        section: "子代理",
    },
    Row {
        id: FieldId::SubApiKey,
        label: "子代理 Key",
        kind: FieldKind::Secret,
        section: "子代理",
    },
    Row {
        id: FieldId::SubDefaultTools,
        label: "子代理工具",
        kind: FieldKind::Text,
        section: "子代理",
    },
    Row {
        id: FieldId::ComplianceEnabled,
        label: "合规模式",
        kind: FieldKind::Toggle,
        section: "通用",
    },
    Row {
        id: FieldId::TokenizerPath,
        // 后端把 tokenizer 装成进程级 OnceLock（只在 load 时初始化一次）：改了不重启
        // daemon 不会生效。标签上写明白，免得用户以为保存就等于换成分词器。
        label: "tokenizer(重启生效)",
        kind: FieldKind::Text,
        section: "通用",
    },
];

/// 单行文本编辑缓冲（沿用 AttachPath 的 Vec<char> + cursor 模式）。
#[derive(Debug, Clone, Default)]
pub struct EditBuffer {
    pub buf: Vec<char>,
    pub cursor: usize,
}

/// 设置页 UI 状态。随 `Overlay::Settings` 持有，Esc 关闭即丢弃草稿（取消）。
///
/// 目录分区**没有草稿**：provider/model/默认模型全部即时写（`config_revision`
/// CAS），Esc 取消不了已生效的目录变更——与 electron 的 `registry.perform()`
/// 同一口径。
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub focus: usize,
    pub editing: Option<EditBuffer>,
    /// 脏字段累积器（K3：只发改过的字段）——**只承载应用设置**。
    pub draft: ConfigPatch,
    /// 与协议 ConfigPatch 分离，本机配置永不序列化进 daemon 写请求。
    pub local_loaded: crate::local_config::LocalConfig,
    pub performance_draft: Option<bool>,
    /// 「账号登录」行的一行摘要。
    pub account_summary: String,
    /// provider 目录快照（`provider.list`）。App 在打开设置页与目录更新时写入；
    /// 目录分区的循环候选与展示全部读它。
    pub catalog: crate::app::session::ProviderCatalog,
    /// 目录分区的循环/编辑状态（与 ConfigPatch 草稿彻底隔离）。
    pub catalog_draft: CatalogDraft,
    /// 用户自由滚动偏移（滚轮/PgUp/PgDn 调整）。
    pub scroll: usize,
}

/// 模型目录分区的编辑缓冲（即时写，不经 config.save）。
#[derive(Debug, Clone, Default)]
pub struct CatalogDraft {
    /// 「新增模型」目标 provider（None = 目录第一个）。
    pub add_model_provider: Option<String>,
    pub add_model_id: String,
    pub add_model_context: Option<u64>,
    pub add_model_max_tokens: Option<u64>,
    pub new_provider_id: String,
    pub new_provider_name: String,
    pub new_provider_base_url: String,
    /// None = 展示 `chat`（wire 缺省值）。
    pub new_provider_wire: Option<String>,
    /// None = 展示 `api_key`。
    pub new_provider_credential: Option<String>,
    /// 空提交 = 保持/不设密钥（SecretWrite::Keep 语义由 apply 侧决定）。
    pub new_provider_api_key: String,
    /// 「删除模型」循环状态（"provider/model"）。
    pub delete_model_sel: Option<String>,
    /// 「默认模型」循环状态（"provider/model"）；None = 展示目录现值。
    pub default_model_sel: Option<String>,
}

impl CatalogDraft {
    /// 新增模型的目标 provider（缓冲优先，回落目录第一个）。
    pub fn add_model_target(&self, catalog: &crate::app::session::ProviderCatalog) -> Option<String> {
        if let Some(p) = &self.add_model_provider {
            return Some(p.clone());
        }
        catalog.providers.keys().next().cloned()
    }

    /// 「删除模型」的候选列表（"provider/model" 字符串）。
    pub fn delete_candidates(catalog: &crate::app::session::ProviderCatalog) -> Vec<String> {
        let mut out = Vec::new();
        for (provider_id, entry) in &catalog.providers {
            for model_id in entry.models.keys() {
                out.push(format!("{provider_id}/{model_id}"));
            }
        }
        out
    }
}

impl SettingsState {
    pub fn has_changes(&self) -> bool {
        !self.draft.is_empty() || self.performance_draft.is_some()
    }

    pub fn row(&self) -> &'static Row {
        &ROWS[self.focus.min(ROWS.len() - 1)]
    }

    pub fn move_focus(&mut self, delta: i32) {
        let n = ROWS.len() as i32;
        let next = (self.focus as i32 + delta).rem_euclid(n);
        self.focus = next as usize;
        // 焦点跳转（含 wrap）后旧滚动偏移没有意义：绘制层会保证焦点行可见，
        // 这里归零让视口回到焦点所在位置，而不是停在前一次自由滚动的偏移上。
        self.scroll = 0;
    }

    /// 设置卡片的行数估算（分区头 + 空行 + 数据行），供绘制层决定卡片高度。
    ///
    /// 与渲染共用同一个分区规则：遇到新分区追加空行 + 标题行，每个字段一行。
    pub fn total_lines(&self) -> usize {
        let mut total = 0usize;
        let mut section = "";
        for row in ROWS {
            if row.section != section {
                section = row.section;
                if total > 0 {
                    total += 1; // 分区间空行
                }
                total += 1; // 分区标题行
            }
            total += 1;
        }
        total
    }

    /// 当前字段是否已有未保存草稿值。
    ///
    /// 目录分区全部**即时写**，永远没有草稿——返回 false 是语义（不是偷懒）。
    pub fn dirty(&self, id: FieldId) -> bool {
        match id {
            FieldId::SandboxEnabled => self
                .draft
                .sandbox
                .as_ref()
                .is_some_and(|s| s.enabled.is_some()),
            FieldId::PerformanceMode => self.performance_draft.is_some(),
            FieldId::AutoCompactThreshold => self.draft.auto_compact_threshold.is_some(),
            FieldId::ExecDefaultShell => self
                .draft
                .exec
                .as_ref()
                .is_some_and(|e| e.default_shell.is_some()),
            FieldId::SessionIdleUnloadSecs => self.draft.session_idle_unload_secs.is_some(),
            FieldId::ComplianceEnabled => self.draft.compliance_enabled.is_some(),
            FieldId::TokenizerPath => self.draft.tokenizer_path.is_some(),
            FieldId::SubModel
            | FieldId::SubBaseUrl
            | FieldId::SubApiKey
            | FieldId::SubMaxTokens
            | FieldId::SubTimeoutSecs
            | FieldId::SubDefaultTools => {
                let Some(sub) = &self.draft.subagent else {
                    return false;
                };
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
            // 端口字段与目录分区即时生效，无草稿。
            FieldId::PermissionLevel
            | FieldId::Account
            | FieldId::DefaultModel
            | FieldId::AddModelProvider
            | FieldId::AddModelId
            | FieldId::AddModelContext
            | FieldId::AddModelMaxTokens
            | FieldId::AddModelApply
            | FieldId::NewProviderId
            | FieldId::NewProviderName
            | FieldId::NewProviderBaseUrl
            | FieldId::NewProviderWire
            | FieldId::NewProviderCredential
            | FieldId::NewProviderApiKey
            | FieldId::NewProviderApply
            | FieldId::DeleteModelSel
            | FieldId::DeleteModelApply => false,
        }
    }

    /// 当前生效的凭据来源（读模型；v3 起这是只读展示——provider 目录接管凭据）。
    ///
    /// 空串要折成 `api_key`：`ConfigDto::default()` 就是这个形状。
    pub fn effective_credential(&self, loaded: Option<&ConfigDto>) -> String {
        loaded
            .map(|c| c.credential.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "api_key".to_string())
    }

    /// 字段是否禁用。v3 起设置页没有「账号档接管 BYOK 字段」的禁用面
    /// （端点/密钥已迁目录，账号档登录入口独立）——恒 false，保留给调用方
    /// 免改渲染层。
    pub fn is_field_disabled(&self, _id: FieldId, _loaded: Option<&ConfigDto>) -> bool {
        false
    }

    /// 展示值：草稿值优先于 loaded 快照。
    pub fn display(&self, loaded: Option<&ConfigDto>, id: FieldId) -> String {
        let d = &self.draft;
        match id {
            FieldId::PerformanceMode => {
                let on = self
                    .performance_draft
                    .unwrap_or(self.local_loaded.performance_mode);
                if on { "性能模式" } else { "动效模式" }.into()
            }
            FieldId::Account => {
                let credential = self.effective_credential(loaded);
                match crate::app::account::AccountKind::from_credential(&credential) {
                    None => "回车打开账号面板".into(),
                    Some(_) if self.account_summary.is_empty() => "回车打开账号面板".into(),
                    Some(_) => self.account_summary.clone(),
                }
            }
            // ── 模型目录（即时生效，读 catalog 快照）──
            FieldId::DefaultModel => {
                let current = self
                    .catalog_draft
                    .default_model_sel
                    .clone()
                    .or_else(|| self.catalog.default_selection().map(|(p, m)| format!("{p}/{m}")));
                match current {
                    Some(sel) if self.catalog.default_selection().is_some() => sel,
                    Some(sel) => format!("{sel}（回车设为默认）"),
                    None if self.catalog.selections().is_empty() => {
                        "（目录为空：先添加 provider 与模型）".into()
                    }
                    None => "…".into(),
                }
            }
            FieldId::AddModelProvider => self
                .catalog_draft
                .add_model_target(&self.catalog)
                .unwrap_or_else(|| "（无 provider）".into()),
            FieldId::AddModelId => {
                let v = &self.catalog_draft.add_model_id;
                if v.is_empty() { "⏎ 编辑".into() } else { v.clone() }
            }
            FieldId::AddModelContext => num_or(
                self.catalog_draft.add_model_context,
                self.catalog_draft
                    .add_model_target(&self.catalog)
                    .and_then(|p| self.catalog.providers.get(&p))
                    .and_then(|_| None),
            ),
            FieldId::AddModelMaxTokens => {
                match self.catalog_draft.add_model_max_tokens {
                    Some(v) => v.to_string(),
                    None => "…".into(),
                }
            }
            FieldId::AddModelApply => "回车写入目录".into(),
            FieldId::NewProviderId => {
                let v = &self.catalog_draft.new_provider_id;
                if v.is_empty() { "⏎ 编辑".into() } else { v.clone() }
            }
            FieldId::NewProviderName => {
                let v = &self.catalog_draft.new_provider_name;
                if v.is_empty() { "⏎ 编辑".into() } else { v.clone() }
            }
            FieldId::NewProviderBaseUrl => {
                let v = &self.catalog_draft.new_provider_base_url;
                if v.is_empty() { "⏎ 编辑".into() } else { v.clone() }
            }
            FieldId::NewProviderWire => {
                let v = self
                    .catalog_draft
                    .new_provider_wire
                    .clone()
                    .unwrap_or_else(|| "chat".into());
                wire_label(&v)
            }
            FieldId::NewProviderCredential => {
                let v = self
                    .catalog_draft
                    .new_provider_credential
                    .clone()
                    .unwrap_or_else(|| "api_key".into());
                credential_label(&v)
            }
            FieldId::NewProviderApiKey => {
                let v = self.catalog_draft.new_provider_api_key.as_str();
                if v.is_empty() {
                    "(未填写；同 ID 已有密钥 = 保持)".into()
                } else {
                    "●●●●（保存时写入）".into()
                }
            }
            FieldId::NewProviderApply => "回车写入目录".into(),
            FieldId::DeleteModelSel => {
                let candidates = CatalogDraft::delete_candidates(&self.catalog);
                match self.catalog_draft.delete_model_sel.clone().or_else(|| candidates.first().cloned()) {
                    Some(sel) => sel,
                    None => "（目录为空）".into(),
                }
            }
            FieldId::DeleteModelApply => "回车删除（设为默认的模型不可删）".into(),
            FieldId::AutoCompactThreshold => {
                let v = d
                    .auto_compact_threshold
                    .or(loaded.map(|c| c.auto_compact_threshold));
                match v {
                    None => "—".into(),
                    Some(0.0) => "0（关闭）".into(),
                    Some(v) => format!("{v:.2}"),
                }
            }
            FieldId::SandboxEnabled => self.effective(loaded, |d, c| {
                if d.sandbox
                    .as_ref()
                    .and_then(|s| s.enabled)
                    .unwrap_or(c.sandbox.enabled)
                {
                    "开启".into()
                } else {
                    "关闭".into()
                }
            }),
            FieldId::PermissionLevel => loaded
                .map(|c| format!("{}（按 1-4 即时生效）", permission_tier(c.permission_level)))
                .unwrap_or_else(|| "…".into()),
            FieldId::ExecDefaultShell => {
                let v = d
                    .exec
                    .as_ref()
                    .and_then(|e| e.default_shell.clone())
                    .or_else(|| loaded.and_then(|c| c.exec.default_shell.clone()));
                match v.as_deref() {
                    None => "…".into(),
                    // 空串与 "auto" 在后端是同一个语义：平台优先级自动探测。
                    Some(s) if s.is_empty() || s.eq_ignore_ascii_case("auto") => "自动探测".into(),
                    Some(s) => s.to_string(),
                }
            }
            FieldId::SessionIdleUnloadSecs => {
                match d
                    .session_idle_unload_secs
                    .or(loaded.map(|c| c.session_idle_unload_secs))
                {
                    None => "…".into(),
                    Some(0) => "0（不卸载）".into(),
                    Some(v) => v.to_string(),
                }
            }
            FieldId::SubModel => sub_or(d, loaded, |s, c| (s.model.clone(), c.model.clone()), "—"),
            FieldId::SubBaseUrl => sub_or(
                d,
                loaded,
                |s, c| (s.base_url.clone(), c.base_url.clone()),
                "—",
            ),
            FieldId::SubMaxTokens => {
                let v = d
                    .subagent
                    .as_ref()
                    .and_then(|s| s.max_tokens)
                    .or(loaded.map(|c| c.subagent.max_tokens));
                num_or(v, v)
            }
            FieldId::SubTimeoutSecs => {
                let v = d
                    .subagent
                    .as_ref()
                    .and_then(|s| s.timeout_secs)
                    .or(loaded.map(|c| c.subagent.timeout_secs));
                num_or(v, v)
            }
            FieldId::SubApiKey => {
                match (d.subagent.as_ref().and_then(|s| s.api_key.clone()), loaded) {
                    (Some(_), _) => "●●●●（待保存）".into(),
                    (None, Some(c)) if c.subagent.api_key_set => "(已配置 ****)".into(),
                    (None, _) => "(未配置)".into(),
                }
            }
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
            FieldId::ComplianceEnabled => {
                toggle_str(d.compliance_enabled, loaded.map(|c| c.compliance_enabled))
            }
            FieldId::TokenizerPath => opt_str(
                d.tokenizer_path.clone(),
                loaded.and_then(|c| c.tokenizer_path.clone()),
            ),
        }
    }

    /// 开始编辑：Text/Secret/Number/Float 返回预填缓冲，
    /// Enum/Toggle/Port 返回 None（由 cycle/端口逻辑处理）。
    pub fn start_edit(&self, loaded: Option<&ConfigDto>) -> Option<EditBuffer> {
        let session_id = match self.row().id {
            FieldId::AddModelId => self.catalog_draft.add_model_id.clone(),
            FieldId::AddModelContext => self
                .catalog_draft
                .add_model_context
                .map(|v| v.to_string())
                .unwrap_or_default(),
            FieldId::AddModelMaxTokens => self
                .catalog_draft
                .add_model_max_tokens
                .map(|v| v.to_string())
                .unwrap_or_default(),
            FieldId::NewProviderId => self.catalog_draft.new_provider_id.clone(),
            FieldId::NewProviderName => self.catalog_draft.new_provider_name.clone(),
            FieldId::NewProviderBaseUrl => self.catalog_draft.new_provider_base_url.clone(),
            // 密钥类：输入即替换，缓冲恒空。
            FieldId::NewProviderApiKey | FieldId::SubApiKey => String::new(),
            FieldId::TokenizerPath => self.effective(loaded, |d, c| {
                d.tokenizer_path
                    .clone()
                    .unwrap_or_else(|| c.tokenizer_path.clone().unwrap_or_default())
            }),
            FieldId::SubModel => self.effective(loaded, |d, c| {
                d.subagent
                    .as_ref()
                    .and_then(|s| s.model.clone())
                    .unwrap_or_else(|| c.subagent.model.clone())
            }),
            FieldId::SubBaseUrl => self.effective(loaded, |d, c| {
                d.subagent
                    .as_ref()
                    .and_then(|s| s.base_url.clone())
                    .unwrap_or_else(|| c.subagent.base_url.clone())
            }),
            FieldId::SubMaxTokens => self.effective(loaded, |d, c| {
                d.subagent
                    .as_ref()
                    .and_then(|s| s.max_tokens)
                    .unwrap_or(c.subagent.max_tokens)
                    .to_string()
            }),
            FieldId::SubTimeoutSecs => self.effective(loaded, |d, c| {
                d.subagent
                    .as_ref()
                    .and_then(|s| s.timeout_secs)
                    .unwrap_or(c.subagent.timeout_secs)
                    .to_string()
            }),
            FieldId::SubDefaultTools => self.effective(loaded, |d, c| {
                let v = d
                    .subagent
                    .as_ref()
                    .and_then(|s| s.default_tools.clone())
                    .unwrap_or_else(|| c.subagent.default_tools.clone());
                if v.is_empty() {
                    String::new()
                } else {
                    v.join(", ")
                }
            }),
            FieldId::ExecDefaultShell => self.effective(loaded, |d, c| {
                d.exec
                    .as_ref()
                    .and_then(|e| e.default_shell.clone())
                    .unwrap_or_else(|| c.exec.default_shell.clone().unwrap_or_default())
            }),
            FieldId::SessionIdleUnloadSecs => self.effective(loaded, |d, c| {
                d.session_idle_unload_secs
                    .unwrap_or(c.session_idle_unload_secs)
                    .to_string()
            }),
            FieldId::AutoCompactThreshold => self.effective(loaded, |d, c| {
                d.auto_compact_threshold
                    .unwrap_or(c.auto_compact_threshold)
                    .to_string()
            }),
            FieldId::Account
            | FieldId::DefaultModel
            | FieldId::AddModelProvider
            | FieldId::AddModelApply
            | FieldId::NewProviderWire
            | FieldId::NewProviderCredential
            | FieldId::NewProviderApply
            | FieldId::DeleteModelSel
            | FieldId::DeleteModelApply
            | FieldId::ComplianceEnabled
            | FieldId::SandboxEnabled
            | FieldId::PermissionLevel
            | FieldId::PerformanceMode => return None,
        };
        let cursor = session_id.chars().count();
        Some(EditBuffer {
            buf: session_id.chars().collect(),
            cursor,
        })
    }

    /// 提交编辑缓冲（逐字段校验；失败返回 Err 且不落地）。
    ///
    /// 应用设置写 `draft`（config.save 草稿）；目录分区写 `catalog_draft`
    ///（即时写，由 App 层的端口激活发命令）。
    pub fn commit_edit(
        &mut self,
        loaded: Option<&ConfigDto>,
        buf: EditBuffer,
    ) -> Result<(), String> {
        let id = self.row().id;
        let raw: String = buf.buf.iter().collect();
        let text = raw.trim().to_string();
        match id {
            FieldId::AddModelContext | FieldId::AddModelMaxTokens => {
                let v: u64 = text.parse().map_err(|_| format!("{text:?} 不是有效整数"))?;
                if v == 0 {
                    return Err("数值必须大于 0".into());
                }
                if id == FieldId::AddModelContext {
                    self.catalog_draft.add_model_context = Some(v);
                } else {
                    self.catalog_draft.add_model_max_tokens = Some(v);
                }
            }
            FieldId::AddModelId => self.catalog_draft.add_model_id = text,
            FieldId::NewProviderId => {
                if !text.is_empty()
                    && !text
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
                {
                    return Err("provider ID 只允许 [a-z0-9_-]（后端目录主键约束）".into());
                }
                self.catalog_draft.new_provider_id = text;
            }
            FieldId::NewProviderName => self.catalog_draft.new_provider_name = text,
            FieldId::NewProviderBaseUrl => self.catalog_draft.new_provider_base_url = text,
            FieldId::NewProviderApiKey => {
                // 空提交 = 保持（已存在的密钥不动）。
                self.catalog_draft.new_provider_api_key = text;
            }
            FieldId::SubApiKey => {
                if !text.is_empty() {
                    self.draft
                        .subagent
                        .get_or_insert_with(SubagentPatch::default)
                        .api_key = Some(text);
                }
            }
            FieldId::AutoCompactThreshold => {
                let v: f64 = text.parse().map_err(|_| format!("{text:?} 不是有效数字"))?;
                if v.is_nan() || !(0.0..=1.0).contains(&v) {
                    return Err("自动压缩阈值必须在 [0, 1]（0 = 关闭）".into());
                }
                self.draft.auto_compact_threshold = Some(v);
            }
            FieldId::TokenizerPath => self.draft.tokenizer_path = Some(text),
            FieldId::SubModel => {
                self.draft
                    .subagent
                    .get_or_insert_with(SubagentPatch::default)
                    .model = Some(text)
            }
            FieldId::SubBaseUrl => {
                self.draft
                    .subagent
                    .get_or_insert_with(SubagentPatch::default)
                    .base_url = Some(text)
            }
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
                self.draft
                    .subagent
                    .get_or_insert_with(SubagentPatch::default)
                    .default_tools = Some(tools);
            }
            FieldId::ExecDefaultShell => {
                // 空串 = 「自动探测」是**有意的取值**，不是「没填」，照原样写进草稿。
                self.draft
                    .exec
                    .get_or_insert_with(ExecPatch::default)
                    .default_shell = Some(text);
            }
            FieldId::SessionIdleUnloadSecs => {
                // **0 是合法值**（0 = 禁用空闲卸载，后端缺省）。
                let v: u64 = text.parse().map_err(|_| format!("{text:?} 不是有效整数"))?;
                self.draft.session_idle_unload_secs = Some(v);
            }
            FieldId::SubMaxTokens | FieldId::SubTimeoutSecs => {
                let v: u64 = text.parse().map_err(|_| format!("{text:?} 不是有效整数"))?;
                if v == 0 {
                    return Err("数值必须大于 0".into());
                }
                self.set_sub_or_top(id, v);
            }
            // 不可编辑字段：静默忽略（理论上不会到达）。
            FieldId::Account
            | FieldId::DefaultModel
            | FieldId::AddModelProvider
            | FieldId::AddModelApply
            | FieldId::NewProviderWire
            | FieldId::NewProviderCredential
            | FieldId::NewProviderApply
            | FieldId::DeleteModelSel
            | FieldId::DeleteModelApply
            | FieldId::ComplianceEnabled
            | FieldId::SandboxEnabled
            | FieldId::PermissionLevel
            | FieldId::PerformanceMode => {
                let _ = loaded;
            }
        }
        Ok(())
    }

    /// 循环切换（←→ / Enter）。返回 Ok(true) = 已消费；Ok(false) = 无候选可循环
    /// （端口字段由 App 层处理，文本字段什么都不做）。
    pub fn cycle(&mut self, loaded: Option<&ConfigDto>, delta: i32) -> Result<bool, String> {
        let id = self.row().id;
        match id {
            FieldId::SandboxEnabled => {
                let current = self
                    .draft
                    .sandbox
                    .as_ref()
                    .and_then(|s| s.enabled)
                    .or_else(|| loaded.map(|c| c.sandbox.enabled))
                    .unwrap_or(true);
                self.draft
                    .sandbox
                    .get_or_insert_with(Default::default)
                    .enabled = Some(!current);
                Ok(true)
            }
            FieldId::PerformanceMode => {
                let next = !self
                    .performance_draft
                    .unwrap_or(self.local_loaded.performance_mode);
                self.performance_draft =
                    (next != self.local_loaded.performance_mode).then_some(next);
                Ok(true)
            }
            FieldId::ComplianceEnabled => {
                let cur = self
                    .draft
                    .compliance_enabled
                    .or(loaded.map(|c| c.compliance_enabled))
                    .unwrap_or(false);
                self.draft.compliance_enabled = Some(!cur);
                Ok(true)
            }
            // ── 模型目录：循环候选全部读 catalog 快照 ──
            FieldId::DefaultModel => {
                let selections = self.catalog.selections();
                if selections.is_empty() {
                    return Ok(true); // 目录为空：吃了按键但不做任何事（展示已说明）。
                }
                let candidates: Vec<String> =
                    selections.iter().map(|(p, m)| format!("{p}/{m}")).collect();
                let cur = self
                    .catalog_draft
                    .default_model_sel
                    .clone()
                    .or_else(|| self.catalog.default_selection().map(|(p, m)| format!("{p}/{m}")))
                    .unwrap_or_else(|| candidates[0].clone());
                let idx = candidates.iter().position(|c| *c == cur).unwrap_or(0);
                let next =
                    candidates[(idx as i32 + delta).rem_euclid(candidates.len() as i32) as usize].clone();
                self.catalog_draft.default_model_sel = Some(next);
                Ok(true)
            }
            FieldId::AddModelProvider => {
                let providers: Vec<String> = self.catalog.providers.keys().cloned().collect();
                if providers.is_empty() {
                    return Ok(true);
                }
                let cur = self
                    .catalog_draft
                    .add_model_target(&self.catalog)
                    .unwrap_or_else(|| providers[0].clone());
                let idx = providers.iter().position(|p| *p == cur).unwrap_or(0);
                let next =
                    providers[(idx as i32 + delta).rem_euclid(providers.len() as i32) as usize].clone();
                self.catalog_draft.add_model_provider = Some(next);
                Ok(true)
            }
            FieldId::NewProviderWire => {
                let cur = self
                    .catalog_draft
                    .new_provider_wire
                    .clone()
                    .unwrap_or_else(|| "chat".into());
                let idx = WIRE_PROTOCOLS.iter().position(|w| *w == cur).unwrap_or(0);
                self.catalog_draft.new_provider_wire =
                    Some(WIRE_PROTOCOLS[(idx as i32 + delta).rem_euclid(WIRE_PROTOCOLS.len() as i32) as usize].to_string());
                Ok(true)
            }
            FieldId::NewProviderCredential => {
                let cur = self
                    .catalog_draft
                    .new_provider_credential
                    .clone()
                    .unwrap_or_else(|| "api_key".into());
                let idx = CREDENTIALS.iter().position(|c| *c == cur).unwrap_or(0);
                self.catalog_draft.new_provider_credential =
                    Some(CREDENTIALS[(idx as i32 + delta).rem_euclid(CREDENTIALS.len() as i32) as usize].to_string());
                Ok(true)
            }
            FieldId::DeleteModelSel => {
                let candidates = CatalogDraft::delete_candidates(&self.catalog);
                if candidates.is_empty() {
                    return Ok(true);
                }
                let cur = self
                    .catalog_draft
                    .delete_model_sel
                    .clone()
                    .unwrap_or_else(|| candidates[0].clone());
                let idx = candidates.iter().position(|c| *c == cur).unwrap_or(0);
                let next =
                    candidates[(idx as i32 + delta).rem_euclid(candidates.len() as i32) as usize].clone();
                self.catalog_draft.delete_model_sel = Some(next);
                Ok(true)
            }
            // 端口字段 cycle 返回 Ok(false)，由 App 层处理。
            FieldId::PermissionLevel | FieldId::Account | FieldId::AddModelApply
            | FieldId::NewProviderApply | FieldId::DeleteModelApply => Ok(false),
            // 文本/数值字段：←→ 无操作，Enter 进编辑态。
            _ => Ok(false),
        }
    }

    fn effective<T>(
        &self,
        loaded: Option<&ConfigDto>,
        f: impl Fn(&ConfigPatch, &ConfigDto) -> T,
    ) -> T {
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
            FieldId::SubMaxTokens => {
                self.draft
                    .subagent
                    .get_or_insert_with(SubagentPatch::default)
                    .max_tokens = Some(v)
            }
            FieldId::SubTimeoutSecs => {
                self.draft
                    .subagent
                    .get_or_insert_with(SubagentPatch::default)
                    .timeout_secs = Some(v)
            }
            _ => {}
        }
    }
}

/// provider wire 规范名的展示拼写（词表本身就是规范名，直接复用）。
pub fn wire_label(wire: &str) -> String {
    match wire {
        "chat" => "chat（Chat Completions）".into(),
        "responses" => "responses（OpenAI Responses）".into(),
        "messages" => "messages（Anthropic Messages）".into(),
        "gemini" => "gemini（Google generateContent）".into(),
        other => other.to_owned(),
    }
}

/// provider 凭据来源的展示拼写。
pub fn credential_label(credential: &str) -> String {
    match credential {
        "api_key" => "api_key（自带密钥）".into(),
        "chatgpt_plan" => "chatgpt_plan（ChatGPT 套餐）".into(),
        "deepseek_account" => "deepseek_account（DeepSeek 账号）".into(),
        other => other.to_owned(),
    }
}

pub fn permission_tier(level: u8) -> &'static str {
    match level {
        1 => "read-only",
        2 => "workspace-write",
        3 => "skip-permissions",
        4 => "sandbox-run",
        _ => "未知权限档位",
    }
}

fn num_or(draft: Option<u64>, loaded: Option<u64>) -> String {
    match draft.or(loaded) {
        Some(v) => v.to_string(),
        None => "…".into(),
    }
}

/// tokenizer：None 或 Some("") 一律显示「跟随系统」语义。
fn opt_str(draft: Option<String>, loaded: Option<String>) -> String {
    let v = draft.or(loaded);
    match v {
        Some(s) if !s.is_empty() => s,
        _ => "(跟随系统)".into(),
    }
}

fn toggle_str(draft: Option<bool>, loaded: Option<bool>) -> String {
    let on = draft.or(loaded).unwrap_or(false);
    if on {
        "[x] 开".into()
    } else {
        "[ ] 关".into()
    }
}

fn sub_or(
    d: &ConfigPatch,
    loaded: Option<&ConfigDto>,
    pick: impl Fn(&SubagentPatch, &crate::protocol::SubagentDto) -> (Option<String>, String),
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
    use serde_json::json;

    /// 完整读模型载荷（v3 应用设置面：不再有 BYOK 六字段/activeProfile 断言）。
    fn cfg() -> ConfigDto {
        let mut payload = serde_json::to_value(ConfigDto::default()).expect("serialize");
        payload["autoCompactThreshold"] = json!(0.95);
        payload["permissionLevel"] = json!(3);
        payload["notificationsEnabled"] = json!(true);
        payload["complianceEnabled"] = json!(false);
        payload["subagent"] = json!({
            "model": "", "baseUrl": "", "apiKey": "", "apiKeySet": false,
            "maxTokens": 4096, "timeoutSecs": 120, "defaultTools": []
        });
        serde_json::from_value(payload).expect("完整载荷必须可解析")
    }

    fn row_index(id: FieldId) -> usize {
        ROWS.iter().position(|r| r.id == id).unwrap()
    }

    /// 目录 fixture（两个 provider / 各一模型 + 默认选择）。
    fn catalog() -> crate::app::session::ProviderCatalog {
        serde_json::from_value(json!({
            "revision": 2,
            "default_model_selection": {
                "provider_id": "probe", "model_id": "m1", "options": {}
            },
            "providers": {
                "probe": {
                    "name": "Probe", "base_url": "https://p.example.com/v1",
                    "wire": "chat", "credential": "api_key", "api_key": "set",
                    "models": {
                        "m1": {"enabled": true, "display_name": "M1",
                               "context_window": 128000, "max_output_tokens": 8192},
                        "m2": {"enabled": false}
                    }
                },
                "another": {
                    "name": "Another", "base_url": "https://a.example.com",
                    "wire": "responses", "credential": "chatgpt_plan",
                    "models": {"a1": {"enabled": true}}
                }
            }
        }))
        .expect("目录 fixture 可解析")
    }

    #[test]
    fn sandbox_toggle_round_trips_through_merge_patch() {
        let mut c = cfg();
        c.sandbox.enabled = true;
        let mut st = SettingsState {
            focus: row_index(FieldId::SandboxEnabled),
            ..Default::default()
        };
        assert_eq!(st.display(Some(&c), FieldId::SandboxEnabled), "开启");
        st.cycle(Some(&c), 1).unwrap();
        assert!(st.dirty(FieldId::SandboxEnabled));
        assert_eq!(st.display(Some(&c), FieldId::SandboxEnabled), "关闭");
        let patch = serde_json::to_value(&st.draft).unwrap();
        assert_eq!(patch, json!({ "sandbox": { "enabled": false } }));
        st.draft.validate().unwrap();
    }

    #[test]
    fn permission_display_names_all_four_tiers() {
        let mut c = cfg();
        for (level, name) in [
            (1, "read-only"),
            (2, "workspace-write"),
            (3, "skip-permissions"),
            (4, "sandbox-run"),
        ] {
            c.permission_level = level;
            assert_eq!(
                SettingsState::default().display(Some(&c), FieldId::PermissionLevel),
                format!("{name}（按 1-4 即时生效）"),
            );
        }
    }

    #[test]
    fn focus_moves_and_wraps() {
        let mut st = SettingsState::default();
        let n = ROWS.len();
        st.move_focus(1);
        assert_eq!(st.focus, 1);
        st.move_focus(-2);
        assert_eq!(st.focus, n - 1, "向上绕回尾部");
        st.move_focus(1);
        assert_eq!(st.focus, 0, "从尾部向下绕回首行");
    }

    /// 目录分区展示：默认模型读 `default_model_selection`；目录草稿字段的编辑
    /// 状态互不污染 config.save 草稿。
    #[test]
    fn catalog_section_displays_default_and_form_fields() {
        let cat = catalog();
        let mut st = SettingsState {
            focus: row_index(FieldId::DefaultModel),
            catalog: cat.clone(),
            ..Default::default()
        };
        // 默认模型行读目录默认。
        assert_eq!(
            st.display(None, FieldId::DefaultModel),
            "probe/m1",
            "default_model_selection 直接展示"
        );
        // 循环切到另一个模型：状态进 catalog_draft，config.save 草稿仍为空。
        st.cycle(None, 1).unwrap();
        assert!(st.draft.is_empty(), "目录操作不得污染 config.save 草稿");
        let sel = st.catalog_draft.default_model_sel.as_deref().expect("cycle 写入");
        assert!(sel == "another/a1" || sel == "probe/m1", "候选必须在目录里: {sel}");
        // 显示值不再是目录默认。
        assert_ne!(st.display(None, FieldId::DefaultModel), "probe/m1");
    }

    /// 删除模型候选 = 目录里全部模型（provider/model 拼接），删除预检所需。
    #[test]
    fn delete_candidates_cover_the_whole_catalog() {
        let cat = catalog();
        let candidates = CatalogDraft::delete_candidates(&cat);
        assert_eq!(
            candidates,
            ["another/a1", "probe/m1", "probe/m2"],
            "BTreeMap 字典序"
        );
    }

    /// provider 编辑表单：ID 词表校验（目录主键约束在本地先拦）。
    #[test]
    fn new_provider_id_validated_locally() {
        let mut st = SettingsState {
            focus: row_index(FieldId::NewProviderId),
            ..Default::default()
        };
        let buf = |text: &str| EditBuffer {
            buf: text.chars().collect(),
            cursor: text.chars().count(),
        };
        assert!(st.commit_edit(None, buf("my-provider_1")).is_ok());
        assert_eq!(st.catalog_draft.new_provider_id, "my-provider_1");
        let err = st.commit_edit(None, buf("Bad ID!")).unwrap_err();
        assert!(err.contains("[a-z0-9_-]"), "{err}");
    }

    /// 新增模型表单：数字字段进 catalog_draft 而非 config.save 草稿。
    #[test]
    fn add_model_fields_write_catalog_draft_only() {
        let mut st = SettingsState {
            focus: row_index(FieldId::AddModelContext),
            ..Default::default()
        };
        let buf = EditBuffer { buf: "200000".chars().collect(), cursor: 6 };
        st.commit_edit(None, buf).unwrap();
        assert_eq!(st.catalog_draft.add_model_context, Some(200_000));
        assert!(st.draft.is_empty(), "目录字段不得进 config.save 草稿");
        assert!(st.dirty(FieldId::AddModelContext) == false, "目录字段无草稿语义");
    }

    /// config.save 草稿在 v3 应用设置面上只含剩余字段（无 wire/model/credential）。
    #[test]
    fn draft_merge_patch_has_no_byok_keys() {
        let mut st = SettingsState::default();
        st.focus = row_index(FieldId::AutoCompactThreshold);
        let buf = EditBuffer { buf: "0.5".chars().collect(), cursor: 3 };
        st.commit_edit(None, buf).unwrap();
        let patch = serde_json::to_value(&st.draft).unwrap();
        assert_eq!(patch, json!({ "autoCompactThreshold": 0.5 }));
        for legacy in ["wire", "model", "baseUrl", "credential", "apiKey", "activeProfile"] {
            assert!(patch.get(legacy).is_none(), "{legacy} 不得再出现在草稿里");
        }
        st.draft.validate().unwrap();
    }

    #[test]
    fn exec_shell_and_idle_unload_roundtrip() {
        let mut c = cfg();
        let mut st = SettingsState {
            focus: row_index(FieldId::ExecDefaultShell),
            ..Default::default()
        };
        let buf = EditBuffer { buf: "/bin/zsh".chars().collect(), cursor: 8 };
        st.commit_edit(Some(&c), buf).unwrap();
        assert_eq!(
            st.display(Some(&c), FieldId::ExecDefaultShell),
            "/bin/zsh"
        );
        // 空串 = 自动探测（有意的取值）。
        let buf = EditBuffer::default();
        st.commit_edit(Some(&c), buf).unwrap();
        assert_eq!(
            st.display(Some(&c), FieldId::ExecDefaultShell),
            "自动探测"
        );

        st.focus = row_index(FieldId::SessionIdleUnloadSecs);
        let buf = EditBuffer { buf: "0".chars().collect(), cursor: 1 };
        st.commit_edit(Some(&c), buf).unwrap();
        assert_eq!(
            st.display(Some(&c), FieldId::SessionIdleUnloadSecs),
            "0（不卸载）"
        );
        let patch = serde_json::to_value(&st.draft).unwrap();
        assert_eq!(patch["sessionIdleUnloadSecs"], json!(0));
    }

    #[test]
    fn patch_roundtrip_and_validation_on_save() {
        let mut st = SettingsState::default();
        st.focus = row_index(FieldId::AutoCompactThreshold);
        let buf = EditBuffer { buf: "0.8".chars().collect(), cursor: 3 };
        st.commit_edit(None, buf).unwrap();
        st.draft.validate().expect("剩余字段的 Merge Patch 必须合法");
    }

    /// 账号行：快照摘要展示；凭据来源来自 loaded 读模型（v3 起不在草稿里）。
    #[test]
    fn account_row_speaks_the_snapshot() {
        let mut st = SettingsState::default();
        st.account_summary = "已登录 · 用量 12%".into();
        let c = ConfigDto {
            credential: "chatgpt_plan".into(),
            ..cfg()
        };
        assert_eq!(st.display(Some(&c), FieldId::Account), "已登录 · 用量 12%");
        // 未连通（无 loaded）时显示入口提示而不是摘要。
        assert_eq!(
            st.display(None, FieldId::Account),
            "回车打开账号面板"
        );
    }

    /// wire/credential 表单枚举循环：规范名直发（v3 无拼写映射）。
    #[test]
    fn provider_form_enums_cycle_the_canonical_vocabulary() {
        let mut st = SettingsState {
            focus: row_index(FieldId::NewProviderWire),
            ..Default::default()
        };
        st.cycle(None, 1).unwrap();
        assert_eq!(st.catalog_draft.new_provider_wire.as_deref(), Some("responses"));
        st.focus = row_index(FieldId::NewProviderCredential);
        st.cycle(None, 1).unwrap();
        assert_eq!(
            st.catalog_draft.new_provider_credential.as_deref(),
            Some("chatgpt_plan")
        );
        assert!(st.draft.is_empty(), "表单枚举不进 config.save 草稿");
    }
}
