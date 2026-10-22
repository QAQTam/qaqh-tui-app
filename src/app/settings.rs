//! 设置页状态（`Overlay::Settings`）：行模型 + 草稿脏字段 + 编辑缓冲。
//!
//! 写纪律（镜像后端 `docs/config-revamp-plan.md` 硬性约束）：
//! - 文本/数值/枚举/开关字段：编辑只累积 [`ConfigPatch`] 脏字段（K3 Merge
//!   Patch），`s` 一次性 `config.save`——**禁止整包写回**；
//! - permissionLevel / activeProfile / workspace.mode 是服务端独立写端口，
//!   不在 Patch 内：回车/数字即时生效（App 层发起 service 调用）；
//! - apiKey 只进不出：掩码/空 = 保持现值，用户显式输入才写；
//! - ConfigChanged 重拉只替换 loaded 快照，脏字段草稿值优先（B5 回声拉回教训）。
//!
//! BYOK（2026-10-06 后端 provider 预设目录退役）：设置面就是那六个字段自身
//! ——`baseUrl` / `wire` / `apiKey` / `model` / `maxTokens` / `contextLength`。
//! daemon 不再下发 provider/endpoint 目录，协议由用户直接声明（[`WIRE_PROTOCOLS`]），
//! 切 wire 也**不许**顺手改写 baseUrl——那正是「改 maxTokens 后端点被改回预设」
//! 那个缺陷的入口，webui 同批删除了 `applyEndpoint()`。

use crate::protocol::{ConfigDto, ConfigPatch, ExecPatch, SubagentPatch};

/// 后端 `validate` 允许的思考强度枚举。
///
/// `ultra` 是 2026-10-14 后端随 `mutil-ai 0.6.0` 放开的一档（`qaqh-config-api`
/// 的 `patch_validate_*` 里「ultra 曾当非法样本」那条测试同步改了口径）。
/// 少这一档的后果不是编译错：`config.load` 读回 `"ultra"` 时循环表 `position`
/// 落空，用户一点方向键就从 ultra 跳到 medium——界面在替他改配置。
pub const REASONING_EFFORTS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

/// 凭据来源三档（`qaqh_client::CredentialSource` 的 serde 拼写）。
///
/// 这不是"又一个开关"：它就是配置里的 `credential` 本身——换个端（桌面端、
/// 手改 `config.toml`）看到的是同一件事，不会出现「UI 说开着、daemon 说没开」。
pub const CREDENTIALS: [&str; 3] = ["api_key", "chatgpt_plan", "deepseek_account"];

/// BYOK 的四条 wire（`ConfigPatch::validate` 的值域）。provider 目录已退役，
/// 协议没有可查的表，由用户在设置页直接声明。
///
/// `gemini` 是 2026-10-08 后端 `Wire::Gemini` 落地后加的：它的 canonical path 带
/// `{model}` 占位（`:generateContent` / 流式 `:streamGenerateContent`），且鉴权走
/// `key` **查询参数**而不是 header——所以切到 gemini 时 baseUrl 的含义与前三个
/// 不同，这正是本表刻意不连带改写 baseUrl 的原因（端点得由用户自己声明对）。
pub const WIRE_PROTOCOLS: [&str; 4] = ["openai", "responses", "anthropic", "gemini"];

/// 把**读模型回显的 anywhere 拼写**折回本页词表里的**写模型拼写**。
///
/// 2026-10-15 起后端 `Wire` 的序列化键换成了官方短名（`chat` / `messages`，
/// `qaqh-types/src/provider.rs:66-80`），而 `ConfigPatch::validate` 的值域**仍是**
/// 旧拼写（`qaqh-config-api/src/lib.rs:314`：直发 `chat` = 400 `action_failed`；
/// 桌面端同一条闸 `settings/patch::wireForDaemon`）。于是读回来是 `chat`、写出去
/// 必须是 `openai`——两边不一致，必须有一处转换。
///
/// 转换放在**读侧**：词表继续只存写模型拼写（出网永远不会带规范名），而循环/
/// 展示先把读回来的值折进来，`position` 才认得当前值。否则用户点一下方向键，
/// 界面就从 `messages` 跳到 `openai`——替用户改了配置还看不出来。
pub fn wire_daemon_spelling(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "chat" | "openai" | "openai-compatible" | "chat_completions" | "chat/completions" => {
            Some("openai")
        }
        "responses" | "openai-responses" => Some("responses"),
        "messages" | "anthropic" | "claude" => Some("anthropic"),
        "gemini"
        | "google"
        | "generatecontent"
        | "generate_content"
        | "gemini-generate-content" => Some("gemini"),
        _ => None,
    }
}

/// 可聚焦字段的稳定标识（行序即 UI 顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    /// 凭据来源（`api_key` / `chatgpt_plan` / `deepseek_account`）—— BYOK 与
    /// 账号额度的总开关，排在 BYOK 字段之前。
    Credential,
    /// 账号登录（`chatgpt_plan` / `deepseek_account` 两档的登录与用量入口）。
    ///
    /// 它是**入口**而不是可编辑值：Enter 打开账号面板，值由
    /// `app::account::AccountBook` 的最近快照给出（`account_summary`）。
    Account,
    BaseUrl,
    Wire,
    Model,
    ApiKey,
    MaxTokens,
    ContextLength,
    ReasoningEffort,
    AutoCompactThreshold,
    PermissionLevel,
    ActiveProfile,
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
        id: FieldId::Credential,
        label: "凭据来源",
        kind: FieldKind::Enum,
        section: "凭据来源",
    },
    Row {
        id: FieldId::Account,
        label: "账号登录",
        kind: FieldKind::Port,
        section: "凭据来源",
    },
    Row {
        id: FieldId::PerformanceMode,
        label: "渲染",
        kind: FieldKind::Enum,
        section: "TUI 本地（不发送 daemon）",
    },
    Row {
        id: FieldId::BaseUrl,
        label: "Base URL",
        kind: FieldKind::Text,
        section: "端点与模型（BYOK）",
    },
    Row {
        id: FieldId::Wire,
        label: "wire 协议",
        kind: FieldKind::Enum,
        section: "端点与模型（BYOK）",
    },
    Row {
        id: FieldId::Model,
        label: "模型",
        kind: FieldKind::Text,
        section: "端点与模型（BYOK）",
    },
    Row {
        id: FieldId::ApiKey,
        label: "API Key",
        kind: FieldKind::Secret,
        section: "端点与模型（BYOK）",
    },
    Row {
        id: FieldId::MaxTokens,
        label: "maxTokens",
        kind: FieldKind::Number,
        section: "生成参数",
    },
    Row {
        id: FieldId::ContextLength,
        label: "contextLength",
        kind: FieldKind::Number,
        section: "生成参数",
    },
    Row {
        id: FieldId::ReasoningEffort,
        label: "思考强度",
        kind: FieldKind::Enum,
        section: "生成参数",
    },
    Row {
        id: FieldId::AutoCompactThreshold,
        label: "自动压缩阈值",
        kind: FieldKind::Float,
        section: "生成参数",
    },
    Row {
        id: FieldId::PermissionLevel,
        label: "权限级别",
        kind: FieldKind::Port,
        section: "运行时",
    },
    Row {
        id: FieldId::ActiveProfile,
        label: "Profile",
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
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub focus: usize,
    pub editing: Option<EditBuffer>,
    /// 脏字段累积器（K3：只发改过的字段）。
    pub draft: ConfigPatch,
    /// 与协议 ConfigPatch 分离，本机配置永不序列化进 daemon 写请求。
    pub local_loaded: crate::local_config::LocalConfig,
    pub performance_draft: Option<bool>,
    /// Profile 端口的候选名（None = 展示服务端现值）。
    pub profile_sel: Option<String>,
    /// 「账号登录」行的一行摘要。
    ///
    /// 由 App 在打开设置页与账号结果到达时写入（与 `profile_sel` 同款：设置页
    /// 不自己发请求，也就不可能因为「忘了刷」而显示过期状态）。
    pub account_summary: String,
    /// 账号档的模型目录（`siwc.models` / `dsaccount.models`）。
    ///
    /// 非空时 model 行用 ←→ 在目录里切换；空表（BYOK / 目录还没取回）退回自由文本。
    /// 目录**按档隔离**，换档时必须重取（见 `account_ops::apply_account_models`）。
    pub model_options: Vec<crate::app::account::ModelOption>,
    /// 用户自由滚动偏移（滚轮/PgUp/PgDn 调整）。焦点移动时自动跟随焦点行，
    /// 其余时候尊重用户的视口——旧实现只能「跟随焦点滚」，顶部内容在焦点
    /// 位于底部行时永远不可见。
    pub scroll: usize,
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
    pub fn dirty(&self, id: FieldId) -> bool {
        match id {
            FieldId::PerformanceMode => self.performance_draft.is_some(),
            FieldId::Credential => self.draft.credential.is_some(),
            FieldId::BaseUrl => self.draft.base_url.is_some(),
            FieldId::Wire => self.draft.wire.is_some(),
            FieldId::Model => self.draft.model.is_some(),
            FieldId::ApiKey => self.draft.api_key.is_some(),
            FieldId::MaxTokens => self.draft.max_tokens.is_some(),
            FieldId::ContextLength => self.draft.context_length.is_some(),
            FieldId::ReasoningEffort => self.draft.reasoning_effort.is_some(),
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
            // 端口字段即时生效，无草稿。
            FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::Account => false,
        }
    }

    /// 当前生效的凭据来源（草稿优先；两边都没有/空串 = 后端缺省 `api_key`）。
    ///
    /// 空串要折成 `api_key`：`ConfigDto::default()` 就是这个形状，而真实 daemon
    /// 永远显式下发三值之一——把 `""` 当成"第三种状态"会让下面的「这把 key 用不用」
    /// 判断在没有 `config.load` 的路径上（测试、未连通）翻转。
    pub fn effective_credential(&self, loaded: Option<&ConfigDto>) -> String {
        self.draft
            .credential
            .clone()
            .or_else(|| loaded.map(|c| c.credential.clone()))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "api_key".to_string())
    }

    fn credential_now(&self, loaded: Option<&ConfigDto>) -> String {
        self.effective_credential(loaded)
    }

    /// 当前生效的 wire，已折成**写模型拼写**（见 [`wire_daemon_spelling`]）。
    fn current_wire(&self, loaded: Option<&ConfigDto>) -> Option<String> {
        let raw = self
            .draft
            .wire
            .clone()
            .or_else(|| loaded.map(|c| c.wire.clone()))?;
        Some(match wire_daemon_spelling(&raw) {
            Some(canonical) => canonical.to_string(),
            None => raw,
        })
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
            FieldId::BaseUrl => {
                owned_or(d.base_url.clone(), loaded.map(|c| c.base_url.as_str()), "—")
            }
            FieldId::Credential => match self.credential_now(loaded).as_str() {
                "api_key" => "api_key（自带密钥）".into(),
                "chatgpt_plan" => "chatgpt_plan（ChatGPT 套餐额度）".into(),
                "deepseek_account" => "deepseek_account（DeepSeek 账号余额）".into(),
                other => format!("{other}（未识别取值）"),
            },
            FieldId::Account => {
                let credential = self.effective_credential(loaded);
                match crate::app::account::AccountKind::from_credential(&credential) {
                    // BYOK 档没有账号概念：明说「无需登录」，别让用户去找一个不存在的入口。
                    None => "（BYOK 档，无需登录）".into(),
                    Some(_) if self.account_summary.is_empty() => "回车打开账号面板".into(),
                    Some(_) => self.account_summary.clone(),
                }
            }
            // 读模型回显**规范名**（`chat`/`messages`），本页词表只存**写拼写**。
            // 先折一次再展示：用户看到的必须等于「按一下会写出什么」。
            FieldId::Wire => match d.wire.clone().or_else(|| loaded.map(|c| c.wire.clone())) {
                Some(raw) => wire_daemon_spelling(&raw).map_or(raw, str::to_string),
                None => "—".into(),
            },
            FieldId::Model => owned_or(d.model.clone(), loaded.map(|c| c.model.as_str()), "—"),
            FieldId::ApiKey => {
                let base = match (&d.api_key, loaded) {
                    (Some(_), _) => "●●●●（待保存）".to_string(),
                    (None, Some(c)) if c.api_key == "****" => "(已配置 ****)".to_string(),
                    (None, _) => "(未配置)".to_string(),
                };
                // 账号额度不用这把键。继续显示「已配置」正是后端 `CredentialSource`
                // 注释里点名的最坏情形：用户以为在用套餐，实际打的是自己的 key。
                //
                // 只认这两个**已知**账号档：将来后端再加第四种来源时，这里不会被
                // 误判成"这把 key 没用"（那种误判会让人删掉真正在用的密钥）。
                let account = matches!(
                    self.credential_now(loaded).as_str(),
                    "chatgpt_plan" | "deepseek_account"
                );
                if account {
                    format!("{base} · 当前凭据来源不使用此键")
                } else {
                    base
                }
            }
            FieldId::MaxTokens => num_or(d.max_tokens, loaded.map(|c| c.max_tokens)),
            FieldId::ContextLength => num_or(d.context_length, loaded.map(|c| c.context_length)),
            FieldId::ReasoningEffort => owned_or(
                d.reasoning_effort.clone(),
                loaded
                    .map(|c| c.reasoning_effort.as_str())
                    .filter(|s| !s.is_empty()),
                "—",
            ),
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
            FieldId::PermissionLevel => loaded
                .map(|c| match c.permission_level {
                    0..=3 => format!("L{}（按 1-3 即时生效）", c.permission_level),
                    // 档位 4（SandboxRun）已在后端 `qaqh-policy` 落地，但写口
                    // （`ConfigPatch::validate` 与 `config.set_permission_level`）仍是
                    // 1..=3，本页**给不出**这个选项；daemon 侧要是报上来，只读展示，
                    // 且不许按数字大小解释语义（ADR 2026-10-09 的单调性例外：4 数值
                    // 最大，网络工具却仍然走审批）。
                    n => format!("L{n}（daemon 侧档位，本页只读）"),
                })
                .unwrap_or_else(|| "…".into()),
            FieldId::ActiveProfile => {
                let cur = self
                    .profile_sel
                    .clone()
                    .or_else(|| loaded.map(|c| c.active_profile.clone()));
                match cur {
                    Some(name) => {
                        let active = loaded
                            .map(|c| c.active_profile.as_str() == name.as_str())
                            .unwrap_or(true);
                        if active {
                            name
                        } else {
                            format!("{name}（回车应用）")
                        }
                    }
                    None => "…".into(),
                }
            }
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

    /// 开始编辑：Text/Secret/Number/Float 返回预填缓冲（Secret 恒空），
    /// Enum/Toggle/Port 返回 None（由 cycle/端口逻辑处理）。
    pub fn start_edit(&self, loaded: Option<&ConfigDto>) -> Option<EditBuffer> {
        let session_id = match self.row().id {
            FieldId::Model => self.effective(loaded, |d, c| {
                d.model.clone().unwrap_or_else(|| c.model.clone())
            }),
            FieldId::BaseUrl => self.effective(loaded, |d, c| {
                d.base_url.clone().unwrap_or_else(|| c.base_url.clone())
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
            FieldId::TokenizerPath => self.effective(loaded, |d, c| {
                d.tokenizer_path
                    .clone()
                    .unwrap_or_else(|| c.tokenizer_path.clone().unwrap_or_default())
            }),
            FieldId::MaxTokens => self.effective(loaded, |d, c| {
                d.max_tokens.unwrap_or(c.max_tokens).to_string()
            }),
            FieldId::ContextLength => self.effective(loaded, |d, c| {
                d.context_length.unwrap_or(c.context_length).to_string()
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
            FieldId::AutoCompactThreshold => self.effective(loaded, |d, c| {
                d.auto_compact_threshold
                    .unwrap_or(c.auto_compact_threshold)
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
            FieldId::ApiKey | FieldId::SubApiKey => String::new(),
            FieldId::Credential
            | FieldId::Account
            | FieldId::Wire
            | FieldId::ReasoningEffort
            | FieldId::ComplianceEnabled
            | FieldId::PermissionLevel
            | FieldId::PerformanceMode
            | FieldId::ActiveProfile => return None,
        };
        let cursor = session_id.chars().count();
        Some(EditBuffer {
            buf: session_id.chars().collect(),
            cursor,
        })
    }

    /// 提交编辑缓冲到草稿（逐字段校验；失败返回 Err 且不落地）。
    pub fn commit_edit(
        &mut self,
        loaded: Option<&ConfigDto>,
        buf: EditBuffer,
    ) -> Result<(), String> {
        let id = self.row().id;
        let raw: String = buf.buf.iter().collect();
        let text = raw.trim().to_string();
        match id {
            FieldId::MaxTokens
            | FieldId::ContextLength
            | FieldId::SubMaxTokens
            | FieldId::SubTimeoutSecs => {
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
                    self.draft
                        .subagent
                        .get_or_insert_with(SubagentPatch::default)
                        .api_key = Some(text);
                }
            }
            FieldId::Model => self.draft.model = Some(text),
            FieldId::BaseUrl => self.draft.base_url = Some(text),
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
                // 与其它 Number 字段不同：**0 是合法值**（0 = 禁用空闲卸载，后端缺省），
                // 所以不走上面那条「必须大于 0」的共用分支。
                let v: u64 = text.parse().map_err(|_| format!("{text:?} 不是有效整数"))?;
                self.draft.session_idle_unload_secs = Some(v);
            }
            // 不可编辑字段：静默忽略（理论上不会到达）。`Credential` 是 Enum 行，
            // 走 `cycle`；Enter 不会进编辑态（`start_edit` 返回 None）。
            FieldId::Credential
            | FieldId::Account
            | FieldId::Wire
            | FieldId::ReasoningEffort
            | FieldId::ComplianceEnabled
            | FieldId::PermissionLevel
            | FieldId::PerformanceMode
            | FieldId::ActiveProfile => {
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
            FieldId::PerformanceMode => {
                let next = !self
                    .performance_draft
                    .unwrap_or(self.local_loaded.performance_mode);
                self.performance_draft =
                    (next != self.local_loaded.performance_mode).then_some(next);
                Ok(true)
            }
            FieldId::ReasoningEffort => {
                let cur = self
                    .draft
                    .reasoning_effort
                    .clone()
                    .or_else(|| loaded.map(|c| c.reasoning_effort.clone()))
                    .unwrap_or_else(|| "medium".into());
                let idx = REASONING_EFFORTS
                    .iter()
                    .position(|e| *e == cur)
                    .unwrap_or(1);
                let next = (idx as i32 + delta).rem_euclid(REASONING_EFFORTS.len() as i32) as usize;
                self.draft.reasoning_effort = Some(REASONING_EFFORTS[next].to_string());
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
            FieldId::Credential => {
                let cur = self.credential_now(loaded);
                let idx = CREDENTIALS.iter().position(|c| *c == cur).unwrap_or(0);
                let next =
                    CREDENTIALS[(idx as i32 + delta).rem_euclid(CREDENTIALS.len() as i32) as usize];
                self.draft.credential = Some(next.to_string());
                // 后端的组合校验在**保存期**才拒绝（`qaqh-config/src/dto.rs:139-161`：
                // `chatgpt_plan` 只认 `responses`；`deepseek_account` 只认 chat/responses），
                // 而用户这一下只改了一个字段。先把 wire 归位，否则用户会经历
                // 「改一个 → 保存报错 → 回来再改另一个」。与桌面端 `switchFields` 同口径。
                //
                // 但**不碰 baseUrl**：端点仍须由用户自己声明对（gemini/套餐/账号
                // 三家的端点形态各不相同，猜一个 URL 填进用户配置是最坏的兜底）。
                match next {
                    "chatgpt_plan" if self.current_wire(loaded).as_deref() != Some("responses") => {
                        self.draft.wire = Some("responses".into());
                    }
                    "deepseek_account"
                        if !matches!(
                            self.current_wire(loaded).as_deref(),
                            Some("openai") | Some("responses")
                        ) =>
                    {
                        self.draft.wire = Some("openai".into());
                    }
                    _ => {}
                }
                Ok(true)
            }
            FieldId::Wire => {
                // BYOK：协议由用户声明，daemon 不给目录，所以词表就是本地常量。
                // 刻意**不**跟着改 baseUrl——切 wire 只切 wire。
                //
                // `cur` 必须先折回写拼写：读模型回显规范名（`chat`/`messages`），
                // 直接拿去做 `position` 会落空 → 用户点一下就从 messages 跳到 openai。
                let cur = self.current_wire(loaded).unwrap_or_default();
                let idx = WIRE_PROTOCOLS.iter().position(|w| *w == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(WIRE_PROTOCOLS.len() as i32) as usize;
                self.draft.wire = Some(WIRE_PROTOCOLS[next].to_string());
                Ok(true)
            }
            // 账号档有服务端下发的模型目录时，←→ 在目录里切换（切档/切账户后
            // 目录会被重取，见 `account_ops::apply_account_models`）。
            //
            // 目录为空（BYOK 档，或目录还没取回）时**不猜**任何候选，退回自由文本。
            FieldId::Model if !self.model_options.is_empty() => {
                let current = self
                    .draft
                    .model
                    .clone()
                    .or_else(|| loaded.map(|c| c.model.clone()))
                    .unwrap_or_default();
                let len = self.model_options.len() as i32;
                let next = match self
                    .model_options
                    .iter()
                    .position(|option| option.value == current)
                {
                    Some(idx) => self.model_options[(idx as i32 + delta).rem_euclid(len) as usize]
                        .value
                        .clone(),
                    // 当前值不在目录里（手写的 slug / 另一档的模型）：按方向取首/末项，
                    // 而不是跳过一项。
                    None if delta >= 0 => self.model_options[0].value.clone(),
                    None => self.model_options[len as usize - 1].value.clone(),
                };
                self.draft.model = Some(next);
                Ok(true)
            }
            // 端口字段 cycle 返回 Ok(false)，由 App 层处理。
            FieldId::PermissionLevel | FieldId::ActiveProfile | FieldId::Account => Ok(false),
            // model/baseUrl 是自由文本（BYOK 后没有目录可循环）：←→ 无操作，
            // Enter 进编辑态。
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
            FieldId::MaxTokens => self.draft.max_tokens = Some(v),
            FieldId::ContextLength => self.draft.context_length = Some(v),
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

    /// 完整读模型载荷。
    ///
    /// **由权威类型自身生成**（`ConfigDto::default()` 序列化出来就是完整形状），
    /// 只覆盖本测试真正关心的那几个字段——这样后端新增字段时这里不会因为
    /// 「少写一个键」而红，而要钉住的东西（BYOK 六字段、subagent）仍是显式写出来的。
    ///
    /// 历史：原先这份 fixture 还手搭一棵 `providers` 目录树（两个 provider、各自的
    /// endpoints 与 models），靠 `qaqh-config-api` 的 struct 级 `#[serde(default)]`
    /// 才解析得动。2026-10-06 BYOK 之后 daemon 不再下发目录，那棵树连同
    /// `providerId`/`endpoint` 两个键一起删了。
    fn cfg() -> ConfigDto {
        let mut payload = serde_json::to_value(ConfigDto::default()).expect("serialize");
        payload["model"] = json!("gpt-5");
        payload["baseUrl"] = json!("https://api.example.com/v1");
        payload["wire"] = json!("openai");
        payload["maxTokens"] = json!(96000);
        payload["contextLength"] = json!(1000000);
        payload["reasoningEffort"] = json!("high");
        payload["autoCompactThreshold"] = json!(0.95);
        payload["permissionLevel"] = json!(3);
        payload["apiKey"] = json!("****");
        payload["activeProfile"] = json!("default");
        payload["profiles"] = json!(["default", "fast"]);
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

    #[test]
    fn focus_moves_and_wraps() {
        let mut st = SettingsState {
            focus: row_index(FieldId::Model),
            ..Default::default()
        };
        st.move_focus(1);
        assert_eq!(st.row().id, FieldId::ApiKey);
        st.move_focus(-2);
        assert_eq!(st.row().id, FieldId::Wire);
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
        assert_eq!(
            st.display(Some(&c), FieldId::AutoCompactThreshold),
            "0（关闭）"
        );
    }

    #[test]
    fn commit_edit_validates_ranges() {
        let c = cfg();
        let mut st = SettingsState {
            focus: row_index(FieldId::MaxTokens),
            ..Default::default()
        };
        let buf = |s: &str| EditBuffer {
            buf: s.chars().collect(),
            cursor: s.len(),
        };
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
    }

    #[test]
    fn cycle_effort_toggles_and_wire() {
        let c = cfg();
        let mut st = SettingsState {
            focus: row_index(FieldId::ReasoningEffort),
            ..Default::default()
        };
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.reasoning_effort.as_deref(), Some("xhigh"));
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(st.draft.reasoning_effort.as_deref(), Some("high"));

        // wire：四值循环（openai → responses → anthropic → gemini → openai）。
        // `gemini` 是 2026-10-08 后端 `Wire::Gemini` 落地后补的第四值；切 wire 仍然
        // **不许**连带改写 baseUrl——目录已退役，任何「顺手填端点」都是凭空发明
        // （gemini 的端点形态还与前三者不同，更不能猜）。
        st.focus = row_index(FieldId::Wire);
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("responses"));
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("anthropic"));
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("gemini"));
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("openai"));
        assert_eq!(st.draft.base_url, None, "切 wire 不得落 baseUrl 草稿");
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("gemini"));
        // 已加载的 wire 不在词表里（旧配置）：从表首开始，不 panic。
        st.draft.wire = Some("legacy".into());
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("responses"));

        // BYOK 后没有模型目录：model 行 ←→ 无操作（Ok(false)），Enter 才进编辑。
        st.focus = row_index(FieldId::Model);
        assert!(!st.cycle(Some(&c), 1).unwrap());

        // 端口字段 cycle 返回 Ok(false)，由 App 层处理。
        st.focus = row_index(FieldId::ActiveProfile);
        assert!(!st.cycle(Some(&c), 1).unwrap());
    }

    /// `ultra`（2026-10-14 后端随 `mutil-ai 0.6.0` 放开的一档）必须进词表。
    ///
    /// 反例是它缺席时的真实行为：`position` 落空 → `unwrap_or(1)` → 用户看到的是
    /// ultra，按一下方向键却落到 medium，再按回来变成 low——界面静默替用户改了配置。
    #[test]
    fn ultra_effort_survives_the_cycle_table() {
        let mut c = cfg();
        c.reasoning_effort = "ultra".into();
        let mut st = SettingsState {
            focus: row_index(FieldId::ReasoningEffort),
            ..Default::default()
        };
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(
            st.draft.reasoning_effort.as_deref(),
            Some("low"),
            "ultra 是最后一档：前进回到表首"
        );
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(
            st.draft.reasoning_effort.as_deref(),
            Some("ultra"),
            "后退必须回到 ultra，而不是 medium"
        );
    }

    /// 读模型回显**规范名**（`chat`/`messages`），写模型只收**旧拼写**。
    ///
    /// 这条闸的价值不在「能显示」，而在**光标不丢**：不折的话 `position` 落空，
    /// 用户从 `messages` 点一下就到 `openai`——跨了两个协议却毫无提示。
    #[test]
    fn canonical_wire_spellings_do_not_reset_the_cycle_cursor() {
        let mut c = cfg();
        c.wire = "messages".into();
        let mut st = SettingsState {
            focus: row_index(FieldId::Wire),
            ..Default::default()
        };
        assert_eq!(
            st.display(Some(&c), FieldId::Wire),
            "anthropic",
            "展示值必须是**写拼写**（按键会写出什么就显示什么）"
        );
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(
            st.draft.wire.as_deref(),
            Some("gemini"),
            "从 messages 前进到 gemini，而不是跳回 openai"
        );

        c.wire = "chat".into();
        let mut st = SettingsState {
            focus: row_index(FieldId::Wire),
            ..Default::default()
        };
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.wire.as_deref(), Some("responses"));
    }

    /// 上游 `Wire::as_str()` 的每个输出都必须折得回本地词表。
    ///
    /// 这是**漂移护栏**：后端再改一次序列化拼写时，这条会立刻红，而不是等到
    /// 用户在设置页发现「点一下跳两格」。
    #[test]
    fn every_upstream_wire_spelling_maps_into_the_local_vocabulary() {
        for wire in [
            qaqh_client::Wire::Chat,
            qaqh_client::Wire::Responses,
            qaqh_client::Wire::Messages,
            qaqh_client::Wire::Gemini,
        ] {
            let canonical = wire.as_str();
            let daemon = wire_daemon_spelling(canonical)
                .unwrap_or_else(|| panic!("`{canonical}` 没有折回写拼写"));
            assert!(
                WIRE_PROTOCOLS.contains(&daemon),
                "`{canonical}` → `{daemon}`，但写词表里没有 `{daemon}`"
            );
        }
    }

    /// 写词表与后端值域**两向**对齐（R3 之后的漂移护栏）。
    ///
    /// - 本页能写出的每个拼写都必须过 `ConfigPatch::validate`（否则「选得到、存不下」）；
    /// - 后端规范名也必须合法（读回来的值不该在保存时被判非法——R3 已放开，
    ///   这条测试把那句话钉住；哪天收紧，这里先红）。
    #[test]
    fn write_table_and_upstream_value_domain_agree() {
        for wire in WIRE_PROTOCOLS {
            ConfigPatch {
                wire: Some(wire.to_string()),
                ..Default::default()
            }
            .validate()
            .unwrap_or_else(|e| panic!("写词表里的 {wire} 被后端拒绝：{e}"));
        }
        for canonical in ["chat", "responses", "messages", "gemini"] {
            ConfigPatch {
                wire: Some(canonical.to_string()),
                ..Default::default()
            }
            .validate()
            .unwrap_or_else(|e| panic!("规范名 {canonical} 必须可写：{e}"));
        }
    }

    /// 凭据来源三档：切换时把 wire 归位，且**不**发明端点；账号档要说明那把键不被使用。
    #[test]
    fn credential_cycle_keeps_wire_compatible_and_flags_the_unused_key() {
        let c = cfg(); // wire = openai，credential = api_key
        let mut st = SettingsState {
            focus: row_index(FieldId::Credential),
            ..Default::default()
        };
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.credential.as_deref(), Some("chatgpt_plan"));
        assert_eq!(
            st.draft.wire.as_deref(),
            Some("responses"),
            "套餐直连只提供 Responses API：不归位就会在保存期被拒"
        );
        assert_eq!(st.draft.base_url, None, "切凭据来源不得凭空写 baseUrl");
        st.draft.validate().expect("归位后的草稿必须过得去值域校验");

        // 账号额度：当前 anthropic → 必须归位到 chat（写拼写 = openai）。
        let mut c2 = cfg();
        c2.wire = "anthropic".into();
        let mut st2 = SettingsState {
            focus: row_index(FieldId::Credential),
            ..Default::default()
        };
        st2.draft.credential = Some("chatgpt_plan".into());
        st2.cycle(Some(&c2), 1).unwrap();
        assert_eq!(st2.draft.credential.as_deref(), Some("deepseek_account"));
        assert_eq!(st2.draft.wire.as_deref(), Some("openai"));
        st2.draft.validate().expect("值域合法");

        // 诚实性：账号档下 API Key 行必须说清「不用此键」。
        assert!(
            st2.display(Some(&c2), FieldId::ApiKey)
                .contains("不使用此键"),
            "账号额度不用那把 key，继续显示「已配置」会让人以为在花自己的钱"
        );
        assert!(
            st2.display(Some(&c2), FieldId::Credential)
                .contains("DeepSeek")
        );

        // 回到 BYOK：credential 归一，wire 不动（它已经是合法的 chat 了）。
        st2.cycle(Some(&c2), 1).unwrap();
        assert_eq!(st2.draft.credential.as_deref(), Some("api_key"));
        assert_eq!(st2.draft.wire.as_deref(), Some("openai"));
    }

    /// 后端 2026-10-09 新开的两个可写字段：`exec.defaultShell` 与
    /// `sessionIdleUnloadSecs`。各自要守住的语义不一样：
    /// - exec 的**空串是有意义取值**（= 平台自动探测），不是「没填」；
    /// - idle 的 **0 是合法值**（后端缺省，= 禁用空闲卸载），所以它不能走
    ///   `maxTokens` 那条「必须大于 0」的共用数值校验。
    #[test]
    fn exec_shell_and_idle_unload_roundtrip() {
        let c = cfg();
        let buf = |s: &str| EditBuffer {
            buf: s.chars().collect(),
            cursor: s.chars().count(),
        };
        let mut st = SettingsState {
            focus: row_index(FieldId::ExecDefaultShell),
            ..Default::default()
        };
        st.commit_edit(Some(&c), buf("pwsh")).unwrap();
        assert_eq!(
            st.draft
                .exec
                .as_ref()
                .and_then(|e| e.default_shell.as_deref()),
            Some("pwsh")
        );
        assert!(st.dirty(FieldId::ExecDefaultShell));
        assert_eq!(
            st.display(Some(&c), FieldId::ExecDefaultShell),
            "pwsh",
            "草稿值必须盖过 loaded"
        );

        st.commit_edit(Some(&c), buf("")).unwrap();
        assert_eq!(
            st.draft
                .exec
                .as_ref()
                .and_then(|e| e.default_shell.as_deref()),
            Some("")
        );
        assert_eq!(
            st.display(Some(&c), FieldId::ExecDefaultShell),
            "自动探测",
            "空串要显示成它的语义，而不是空白"
        );

        st.focus = row_index(FieldId::SessionIdleUnloadSecs);
        st.commit_edit(Some(&c), buf("0")).unwrap();
        assert_eq!(st.draft.session_idle_unload_secs, Some(0));
        assert_eq!(
            st.display(Some(&c), FieldId::SessionIdleUnloadSecs),
            "0（不卸载）"
        );
        st.commit_edit(Some(&c), buf("900")).unwrap();
        assert_eq!(st.draft.session_idle_unload_secs, Some(900));

        // 非整数照旧拒绝。
        assert!(st.commit_edit(Some(&c), buf("abc")).is_err());
        // 反向确认：0 的限制没有被这两条分支互相污染。
        st.focus = row_index(FieldId::MaxTokens);
        assert!(
            st.commit_edit(Some(&c), buf("0")).is_err(),
            "maxTokens = 0 仍须被拒"
        );
    }

    #[test]
    fn patch_roundtrip_and_validation_on_save() {
        let c = cfg();
        let mut st = SettingsState {
            focus: row_index(FieldId::ContextLength),
            ..Default::default()
        };
        st.commit_edit(
            Some(&c),
            EditBuffer {
                buf: "2000000".chars().collect(),
                cursor: 7,
            },
        )
        .unwrap();
        // BYOK：切 wire 与改 contextLength 一起发，键名跟齐权威 crate。
        st.focus = row_index(FieldId::Wire);
        st.cycle(Some(&c), 1).unwrap();
        assert!(!st.draft.is_empty());
        st.draft.validate().unwrap();
        let v = serde_json::to_value(&st.draft).unwrap();
        assert_eq!(v["contextLength"], 2_000_000);
        assert_eq!(v["wire"], "responses");
        assert!(v.get("model").is_none(), "未改动字段不得出现在 wire 上");
        assert!(
            v.get("providerId").is_none() && v.get("endpoint").is_none(),
            "provider 目录已退役，patch 上不得出现目录键：{v}"
        );
    }

    /// 「账号登录」行：BYOK 档明说无需登录；账号档显示最近快照；它没有草稿。
    #[test]
    fn account_row_speaks_the_snapshot_and_byok_has_no_login() {
        use crate::app::account::ModelOption;

        let c = cfg(); // credential = api_key
        let mut st = SettingsState {
            focus: row_index(FieldId::Account),
            ..Default::default()
        };
        assert!(
            st.display(Some(&c), FieldId::Account).contains("无需登录"),
            "BYOK 档没有账号概念，不许暗示有一个可登录的东西"
        );
        assert!(!st.dirty(FieldId::Account));
        assert!(st.start_edit(Some(&c)).is_none(), "账号行不是文本字段");
        assert!(
            !st.cycle(Some(&c), 1).unwrap(),
            "账号行由 App 层处理（Enter 打开面板），cycle 不消费"
        );

        st.draft.credential = Some("chatgpt_plan".into());
        assert_eq!(st.display(Some(&c), FieldId::Account), "回车打开账号面板");
        st.account_summary = "工作 (a@b.c)".into();
        assert_eq!(st.display(Some(&c), FieldId::Account), "工作 (a@b.c)");

        // 模型目录：有目录时 ←→ 在目录里循环，回绕且不跳项。
        let mut st = SettingsState {
            focus: row_index(FieldId::Model),
            ..Default::default()
        };
        st.model_options = vec![
            ModelOption {
                value: "gpt-5".into(),
                label: "GPT-5".into(),
            },
            ModelOption {
                value: "gpt-5-mini".into(),
                label: "GPT-5 mini".into(),
            },
        ];
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.model.as_deref(), Some("gpt-5-mini"));
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.model.as_deref(), Some("gpt-5"), "目录循环回绕");
        st.cycle(Some(&c), -1).unwrap();
        assert_eq!(st.draft.model.as_deref(), Some("gpt-5-mini"));

        // 当前值不在目录里（手写 slug / 另一档模型）：按方向取首/末项，而不是跳过一项。
        st.draft.model = Some("legacy-slug".into());
        st.cycle(Some(&c), 1).unwrap();
        assert_eq!(st.draft.model.as_deref(), Some("gpt-5"));
    }
}
