//! 协议类型镜像（PLAN.md §4 类型镜像纪律）。
//!
//! 手工镜像 `QAQ-Harness/crates/qaqh-ringing` 与 `qaqh-domain` 的 wire 形状。
//! **改动须对照后端 PR**；镜像以 `F:\QAQ-Harness` 2026-08 协议瘦身后的最终形态
//! 为基准（协议版本 1）。禁止在本仓散落协议字面量，全部 import 自本模块。
//!
//! 对应关系：
//! - `qaqh-ringing/src/protocol.rs` → [`mod.rs`] 常量
//! - `qaqh-ringing/src/envelope.rs` → [`envelope`]
//! - `qaqh-domain/src/command.rs` → [`command`]
//! - `qaqh-domain/src/event.rs` → [`event`]
//! - `qaqh-ringing/src/{snapshot,reset}.rs` → [`snapshot`]
//! - `qaqh-runtime/src/ringing/service_methods.rs` → [`methods`]
//! - `qaqh-config-api/src/lib.rs` → **本模块直接重导出**（无镜像）

pub mod methods;
pub mod snapshot;

/// 配置契约：**直接用权威 crate**，本仓不再手工镜像。
///
/// 2026-09-15 前的 `protocol/config.rs` 是 367 行手抄，已漂移出 T-11：
/// `ConfigPatch` 少 `permission_level`（后端 BUG-2026-09-13-15 补的 1..=4 值域
/// 校验因此形同虚设）、`ConfigDto` 少 `mcp`/`lsp`，且注释还写着「刻意不含」——
/// **文档断言与后端现状相反**。改为依赖后，此类漂移在编译期即暴露。
pub use qaqh_config_api::{
    ConfigDto, ConfigPatch, ProviderDto, SubagentDto, SubagentPatch,
};

use serde::{Deserialize, Serialize};

/// Ringing 三频道（`qaqh-domain/src/channel.rs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Control,
    Conversation,
    Tool,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Control, Channel::Conversation, Channel::Tool];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Conversation => "conversation",
            Channel::Tool => "tool",
        }
    }

    /// SSE 帧 id 频道段：`<epoch>:<channel>:<seq>`。
    #[allow(dead_code)]
    pub fn from_path_segment(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

/// daemon 统一 JSON 错误体（HTTP 4xx/5xx 的 body）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_round_trip() {
        for c in Channel::ALL {
            assert_eq!(
                c.as_str(),
                serde_json::to_string(&c).unwrap().trim_matches('"')
            );
            assert_eq!(Channel::from_path_segment(c.as_str()), Some(c));
        }
        assert_eq!(Channel::from_path_segment("bogus"), None);
    }

    /// T-11 回归闸：这三条断言**必须**对着权威 crate 成立，否则说明本仓又
    /// 悄悄接回了手抄件（或依赖被换成了缩小版）。断言刻意贴着「TUI 实际要用的
    /// 那几个字段」，而非上游测试的复制。
    #[test]
    fn config_contract_exposes_fields_tui_needs() {
        // 1) 写路径：权限档位可经 patch 下发并受值域校验（BUG-2026-09-13-15）。
        let ok = ConfigPatch {
            permission_level: Some(4),
            ..Default::default()
        };
        ok.validate().expect("档位 4 合法");
        assert_eq!(serde_json::to_value(&ok).unwrap()["permissionLevel"], 4);

        let bad = ConfigPatch {
            permission_level: Some(5),
            ..Default::default()
        };
        assert!(bad.validate().is_err(), "档位 5 必须被拒，不得落成 Level 4");

        // 2) 读路径：mcp/lsp 两段不再是盲区（T-11 前 ConfigDto 里没有）。
        let dto: ConfigDto = serde_json::from_value(serde_json::json!({
            "model": "m1",
            "mcp": { "enabled": true, "idleShutdownSecs": 300, "servers": [] },
            "lsp": { "enabled": true, "idleShutdownSecs": 600, "servers": [] },
        }))
        .unwrap();
        assert!(dto.mcp.enabled);
        assert_eq!(dto.lsp.idle_shutdown_secs, 600);
        // 3) 旧 daemon 的 snake_case 形状仍须可解析（前向兼容未随迁移丢失）。
        let legacy: ConfigDto =
            serde_json::from_value(serde_json::json!({ "base_url": "https://x/v1" })).unwrap();
        assert_eq!(legacy.base_url, "https://x/v1");
    }
}
