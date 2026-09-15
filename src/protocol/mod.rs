//! 协议常量与本仓自有视图（PLAN.md §4 类型镜像纪律）。
//!
//! **阶段二后本模块已基本不含镜像**：wire 类型一律 `use qaqh_client::*`
//! （ringing v1 的权威实现），配置类型重导出 `qaqh_config_api`。此处只剩两类
//! 东西——上游没有对应 Rust 产物的**协议字符串词表**，和 TUI 自己的解析视图：
//!
//! - [`methods`] — 服务方法名字表。上游 `service_methods.rs` 是 `match`
//!   字符串、无 `pub const` 表，`qaqh-client` 亦未导出名字映射，故无从导入
//!   （待 T-01 阶段 1.5 服务面迁到 `Client::query/action`）。守卫 =
//!   `methods::tests::method_table_matches_backend_shape`。
//! - [`snapshot`] — 频道快照的宽松解析视图（wire 类型本身来自 `qaqh_client`）。
//! - [`WireError`] — daemon 错误体，本仓服务面自用。
//!
//! 禁止在本仓散落协议字面量，全部 import 自本模块或上述权威 crate。
//!
//! 历史：2026-09-15 前这里是对 `qaqh-ringing`/`qaqh-domain`/`qaqh-config-api`
//! 的手工镜像（9 文件 2481 行），已漂移出 T-09～T-12 四项缺陷；阶段二逐刀
//! 删除，仅余上述非镜像内容。

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

/// daemon 统一 JSON 错误体（HTTP 4xx/5xx 的 body）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
