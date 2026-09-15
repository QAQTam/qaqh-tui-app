//! 协议视图（PLAN.md §4 类型镜像纪律）。
//!
//! **本模块已清空**：G1（三频道快照 `state`）与 G2（`session.list` 条目）先后
//! 落地后，本仓不再持有**任何**协议解析视图——协议类型一律 `use qaqh_client::*`
//! （ringing v1 权威实现），配置类型重导出 `qaqh_config_api`，服务方法名归
//! `QueryRequest`/`ActionRequest` 的枚举持有。此处只剩两个 re-export 面。
//!
//! 两次删除的形状完全一样，值得记下来：
//!
//! - **G1**：三频道快照的 `state` 手解（`snapshot.rs`，206 行）删除前已经漏了
//!   `active_turn` / `last_round` / `compact_status` / `compact_id` / `cancelled` /
//!   `last_finished` 六个字段，且自身还带着两个零读取的死字段；其中「无 timeline
//!   时降级展示」那条路径**从未生效过**（中立 turn 不带 `TimelineTurn` 要求的
//!   `created_seq`/`sealed`/`state`/`failure`，逐条解析必然失败 → 全被
//!   `filter_map` 滤空）。
//! - **G2**：`session_meta.rs`（128 行）漏解 `created_at` / `turn_count` /
//!   `message_count` / `tool_mode` 等键，同样是零读取所以零人发现。
//!
//! 即**手抄必然漏字段，而且漏了不会报错**——手抄的失败模式是静默的。这是本仓
//! 宁可倒贴一次迁移成本也要改吃权威类型的唯一理由。
//!
//! 禁止在本仓散落协议字面量，全部 import 自本模块或上述权威 crate。
//!
//! 历史：2026-09-15 前这里是对 `qaqh-ringing`/`qaqh-domain`/`qaqh-config-api`
//! 的手工镜像（9 文件 2481 行），已漂移出 T-09～T-12 四项缺陷；此后逐刀删除
//! （config 367 行、死镜像三处、方法表 38 常量、快照手解、会话列表手解）。
//!
//! 删除纪律（T-13）：**别指望 `dead_code` 指出残留**——私有模块里的 `pub` 项
//! 和 `#![allow(dead_code)]` 都能让死镜像零警告地活着，自带测试还会反过来把
//! 它钉成「活的」。逐项 grep 真实调用点才算数。

/// 配置契约：**直接用权威 crate**，本仓不再手工镜像。
///
/// 2026-09-15 前的 `protocol/config.rs` 是 367 行手抄，已漂移出 T-11：
/// `ConfigPatch` 少 `permission_level`（后端 BUG-2026-09-13-15 补的 1..=4 值域
/// 校验因此形同虚设）、`ConfigDto` 少 `mcp`/`lsp`，且注释还写着「刻意不含」——
/// **文档断言与后端现状相反**。改为依赖后，此类漂移在编译期即暴露。
pub use qaqh_config_api::{ConfigDto, ConfigPatch, ProviderDto, SubagentDto, SubagentPatch};

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{SessionActivity, SessionListEntry};

    /// **G2 回归闸（消费侧）**：会话列表条目与会话活动快照的**权威类型**必须
    /// 解析得出 UI 真正要用的那几个字段。
    ///
    /// 与上面那条 config 闸同款用意：断言贴着「本仓实际读的字段」写，若有人把
    /// 依赖换成缩小版、或本仓又接回手抄件，这里立刻红。注意它**不**复制上游测试
    /// ——上游测的是上游的意图，这里测的是本仓的用量。
    #[test]
    fn session_payload_contract_exposes_fields_tui_needs() {
        let wire = serde_json::json!({
            "seed": "0123abcd",
            "created_at": 1757800000000u64,
            "updated_at": 1757900000000u64,
            "model": "m1",
            "message_count": 3,
            "title": "Bun 引导 daemon",
            "cwd": "/home/x/Projects/qaqh-backend",
            "mode": 1,
            "archived": true,
            "ephemeral": false,
            "running": true,
        });
        let entry: SessionListEntry =
            serde_json::from_value(wire.clone()).expect("会话列表条目可解析");
        // 列表渲染真正读的每一个字段（ui/home.rs、ui/overlays.rs、app/overlay_ops.rs）。
        assert_eq!(entry.meta.seed, "0123abcd");
        assert!(entry.meta.archived && !entry.meta.ephemeral);
        assert!(entry.running);
        assert_eq!(entry.meta.updated_at, 1757900000000);
        assert_eq!(entry.meta.display_title(), "Bun 引导 daemon");
        // 未分组/旧 daemon 不带 workspace_id 时必须仍是 `None` 而不是解析失败。
        assert_eq!(entry.workspace_id, None);

        // **严格度差异（须知会）**：`seed`/`created_at`/`updated_at`/`model`/
        // `message_count` 在 `SessionMeta` 上没有 `#[serde(default)]`，缺一个整条就被
        // 跳过；而 G2 之前那份手解把这些全当可选。实际不受影响——产出方就是
        // `SessionMeta` 本身，缺这些键的记录在 daemon 读盘那一步就已经被丢了，
        // 根本发不到 wire。这条断言是为了让「严格在哪」写成可执行的，而不是靠记忆。
        for required in ["seed", "created_at", "updated_at", "model", "message_count"] {
            let mut partial = wire.clone();
            partial.as_object_mut().unwrap().remove(required);
            assert!(
                serde_json::from_value::<SessionListEntry>(partial).is_err(),
                "{required} 缺失必须解析失败（无 serde(default)），严格度与手解不同"
            );
        }

        // 活动快照：本仓只用 seed + state 两个键（app/mod.rs 的 activity_cache）。
        let acts: Vec<SessionActivity> = serde_json::from_value(serde_json::json!([
            { "seed": "0123abcd", "state": "working", "turn_id": "t1", "seq": 3, "updated_at": 1 },
        ]))
        .expect("活动快照可解析");
        assert_eq!(acts[0].seed, "0123abcd");
        assert_eq!(acts[0].state, qaqh_client::DomainActivityState::Working);
        assert_eq!(acts[0].turn_id.as_deref(), Some("t1"));
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
