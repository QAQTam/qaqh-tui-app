#![allow(dead_code)]
//! 配置契约镜像（对照 `QAQ-Harness/crates/qaqh-config-api/src/lib.rs`，改动须对照后端 PR）。
//!
//! - K2：wire 键风格定死 camelCase；读路径带历史 snake_case `alias` 向前兼容旧 daemon。
//! - K3：写语义 = JSON Merge Patch（RFC 7386）——[`ConfigPatch`] 只含 `Option` 字段、
//!   序列化跳过 None，缺失 = 不动。**保存只发脏字段，禁止整包写回**
//!   （2026-08-25 winui 设置页「全零草稿整包毒化」事故的根因防线）。
//! - 守卫语义冻结（与后端 `qaqh_config::dto::apply_patch` 一致）：
//!   - `apiKey` / `subagent.apiKey`：掩码 `"****"` 或空串 = 保持现值（无删除端口）；
//!   - `lang` / `theme`：`Some("")` = 清除（跟随系统）；
//!   - 数值：[`ConfigPatch::validate`] 值域（>0；autoCompactThreshold ∈ [0,1]，0 = 关闭）。

use serde::{Deserialize, Serialize};

fn default_notifications_enabled() -> bool {
    true
}

/// 读模型：`config.load` 响应。`serde(default)` 保证旧 daemon 缺字段时向前兼容。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigDto {
    pub model: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "provider_id")]
    pub provider_id: String,
    pub endpoint: String,
    #[serde(alias = "max_tokens")]
    pub max_tokens: u64,
    #[serde(alias = "context_limit")]
    pub context_limit: u64,
    #[serde(alias = "reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(alias = "auto_compact_threshold")]
    pub auto_compact_threshold: f64,
    #[serde(alias = "permission_level")]
    pub permission_level: u8,
    /// 空串 = 未配置；`"****"` = 已配置（掩码，明文永不出 daemon）。
    #[serde(alias = "api_key")]
    pub api_key: String,
    pub lang: Option<String>,
    #[serde(alias = "font_family")]
    pub font_family: String,
    /// None/空 = 跟随系统。
    pub theme: Option<String>,
    #[serde(default = "default_notifications_enabled", alias = "notifications_enabled")]
    pub notifications_enabled: bool,
    #[serde(alias = "active_profile")]
    pub active_profile: String,
    /// profile 名列表（服务端派生，只读）。
    pub profiles: Vec<String>,
    #[serde(alias = "compliance_enabled")]
    pub compliance_enabled: bool,
    /// provider 预设目录（服务端派生，只读；选择器数据源）。
    pub providers: Vec<ProviderDto>,
    pub subagent: SubagentDto,
    pub workspace: WorkspaceDto,
    #[serde(alias = "tokenizer_path")]
    pub tokenizer_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderDto {
    pub id: String,
    pub display: String,
    pub endpoints: Vec<EndpointDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointDto {
    pub id: String,
    pub display: String,
    pub protocol: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "default_model")]
    pub default_model: String,
    pub models: Vec<String>,
    pub stateful: bool,
    pub beta: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentDto {
    pub model: String,
    #[serde(alias = "base_url")]
    pub base_url: String,
    #[serde(alias = "api_key")]
    pub api_key: String,
    #[serde(alias = "api_key_set")]
    pub api_key_set: bool,
    #[serde(alias = "max_tokens")]
    pub max_tokens: u64,
    #[serde(alias = "timeout_secs")]
    pub timeout_secs: u64,
    /// 空数组 = 全部工具可用（配置语义，非缺省）。
    #[serde(alias = "default_tools")]
    pub default_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkspaceDto {
    pub mode: String,
}

/// 写模型：JSON Merge Patch。序列化时跳过 None——wire 上永不出现 `"field": null`。
///
/// 刻意**不含**（服务端独立写端口，TUI 走专用 service 方法）：`permission_level`
/// （`config.set_permission_level`）、`active_profile`（`profile.apply`）、
/// `workspace.mode`（`workspace.set_mode`）、providers/profiles（服务端派生）、
/// `api_key_set`（服务端派生）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigPatch {
    /// 主密钥：仅用户显式输入新值时 Some；掩码/空串 = 保持现值。
    #[serde(skip_serializing_if = "Option::is_none", alias = "api_key")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "base_url")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "provider_id")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "max_tokens")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "context_limit")]
    pub context_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "reasoning_effort")]
    pub reasoning_effort: Option<String>,
    /// 值域 [0,1]；0 = 关闭自动压缩。
    #[serde(skip_serializing_if = "Option::is_none", alias = "auto_compact_threshold")]
    pub auto_compact_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "compliance_enabled")]
    pub compliance_enabled: Option<bool>,
    /// Some("") = 清除（跟随系统）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "font_family")]
    pub font_family: Option<String>,
    /// Some("") = 跟随系统。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "notifications_enabled")]
    pub notifications_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "tokenizer_path")]
    pub tokenizer_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentPatch>,
}

/// 子代理配置段（写模型），嵌套于 [`ConfigPatch::subagent`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "base_url")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "api_key")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "max_tokens")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "timeout_secs")]
    pub timeout_secs: Option<u64>,
    /// 空数组 = 全部工具可用（与后端 `qaqh-config-api` 语义一致）。
    #[serde(skip_serializing_if = "Option::is_none", alias = "default_tools")]
    pub default_tools: Option<Vec<String>>,
}

impl ConfigPatch {
    /// 客户端预校验（镜像后端 `ConfigPatch::validate`；发送前必过）。
    pub fn validate(&self) -> Result<(), String> {
        if let Some(t) = self.auto_compact_threshold
            && (t.is_nan() || !(0.0..=1.0).contains(&t))
        {
            return Err(format!("autoCompactThreshold 必须在 [0, 1] 区间（0=关闭），收到 {t}"));
        }
        if let Some(v) = self.max_tokens
            && v == 0
        {
            return Err("maxTokens 必须大于 0".into());
        }
        if let Some(v) = self.context_limit
            && v == 0
        {
            return Err("contextLimit 必须大于 0".into());
        }
        if let Some(e) = &self.reasoning_effort
            && !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
        {
            return Err(format!("reasoningEffort 仅允许 low|medium|high|xhigh|max，收到 {e}"));
        }
        if let Some(sub) = &self.subagent {
            if let Some(v) = sub.max_tokens
                && v == 0
            {
                return Err("subagent.maxTokens 必须大于 0".into());
            }
            if let Some(v) = sub.timeout_secs
                && v == 0
            {
                return Err("subagent.timeoutSecs 必须大于 0".into());
            }
        }
        Ok(())
    }

    /// 是否为空补丁——UI 据此显示「未保存」标记 / 跳过保存。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 序列化为 camelCase wire JSON。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K2/K3：camelCase wire；空 Patch = "{}"（不发 null、不发未改动字段）。
    #[test]
    fn patch_serializes_camel_case_and_skips_none() {
        let s = serde_json::to_string(&ConfigPatch::default()).unwrap();
        assert_eq!(s, "{}");

        let patch = ConfigPatch {
            model: Some("m".into()),
            context_limit: Some(1_000_000),
            auto_compact_threshold: Some(0.95),
            subagent: Some(SubagentPatch {
                timeout_secs: Some(240),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = patch.to_json();
        assert_eq!(v["model"], "m");
        assert_eq!(v["contextLimit"], 1_000_000);
        assert_eq!(v["autoCompactThreshold"], 0.95);
        assert_eq!(v["subagent"]["timeoutSecs"], 240);
        assert!(v.get("baseUrl").is_none());
        assert!(v.get("theme").is_none());
    }

    /// 读路径兼容：后端活体形状（历史 snake_case 键）必须能无损解析。
    #[test]
    fn dto_parses_live_daemon_shape_with_legacy_aliases() {
        let payload = serde_json::json!({
            "api_key": "****",
            "model": "ox-alpha-free",
            "base_url": "https://opencode.ai/zen/go/v1",
            "provider_id": "opencode-go",
            "endpoint": "openai",
            "max_tokens": 96000,
            "context_limit": 1000000,
            "reasoning_effort": "max",
            "auto_compact_threshold": 0.95,
            "permission_level": 4,
            "lang": null,
            "font_family": "",
            "theme": null,
            "notifications_enabled": true,
            "active_profile": "default",
            "profiles": ["default"],
            "compliance_enabled": false,
            "providers": [{
                "id": "opencode-go",
                "display": "OpenCode",
                "endpoints": [{
                    "id": "openai",
                    "display": "OpenAI",
                    "protocol": "openai",
                    "base_url": "https://opencode.ai/zen/go/v1",
                    "default_model": "",
                    "models": ["ox-alpha-free"],
                    "stateful": false,
                    "beta": false
                }]
            }],
            "subagent": {
                "model": "",
                "base_url": "",
                "api_key": "",
                "api_key_set": false,
                "max_tokens": 4096,
                "timeout_secs": 120,
                "default_tools": ["read"]
            },
            "workspace": { "mode": "local" },
            "tokenizer_path": null
        });
        let dto: ConfigDto = serde_json::from_value(payload).unwrap();
        assert_eq!(dto.context_limit, 1_000_000);
        assert_eq!(dto.reasoning_effort, "max");
        assert_eq!(dto.api_key, "****");
        assert!(!dto.subagent.api_key_set);
        assert_eq!(dto.workspace.mode, "local");
        assert_eq!(dto.providers[0].endpoints[0].models, vec!["ox-alpha-free"]);
    }

    /// 读路径兼容：新 daemon 的 camelCase 形状。
    #[test]
    fn dto_parses_camel_case_shape() {
        let payload = serde_json::json!({
            "model": "m1",
            "baseUrl": "https://x/v1",
            "autoCompactThreshold": 0.8,
            "notificationsEnabled": false,
        });
        let dto: ConfigDto = serde_json::from_value(payload).unwrap();
        assert_eq!(dto.base_url, "https://x/v1");
        assert!((dto.auto_compact_threshold - 0.8).abs() < f64::EPSILON);
        // 缺省 notifications_enabled = true（daemon 契约），显式 false 胜出。
        assert!(!dto.notifications_enabled);
    }

    #[test]
    fn patch_validate_rejects_out_of_range() {
        let bad = ConfigPatch { auto_compact_threshold: Some(1.5), ..Default::default() };
        assert!(bad.validate().is_err());
        let nan = ConfigPatch { auto_compact_threshold: Some(f64::NAN), ..Default::default() };
        assert!(nan.validate().is_err());
        let disabled = ConfigPatch { auto_compact_threshold: Some(0.0), ..Default::default() };
        assert!(disabled.validate().is_ok(), "0 = 关闭自动压缩，合法");
        let zero = ConfigPatch { max_tokens: Some(0), ..Default::default() };
        assert!(zero.validate().is_err());
        let effort = ConfigPatch { reasoning_effort: Some("ultra".into()), ..Default::default() };
        assert!(effort.validate().is_err());
        let sub = ConfigPatch {
            subagent: Some(SubagentPatch { timeout_secs: Some(0), ..Default::default() }),
            ..Default::default()
        };
        assert!(sub.validate().is_err());
    }
}
