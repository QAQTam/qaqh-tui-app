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
/// `ConfigPatch` 少 `permission_level`（后端 BUG-2026-09-13-15 补的 1..=3 值域
/// 校验因此形同虚设；2026-10-03 收敛为三档制）、`ConfigDto` 少 `mcp`/`lsp`，且注释还写着「刻意不含」——
/// **文档断言与后端现状相反**。改为依赖后，此类漂移在编译期即暴露。
///
/// 同理暴露 BYOK（2026-10-06）：后端删了 `ProviderDto`/`EndpointDto`，`ConfigDto`
/// 以 `wire` + `contextLength` 取代 `providerId`/`endpoint`/`contextLimit`——这里
/// 少一个 re-export、设置页多一处引用，都直接变成编译错误，不会静默。
pub use qaqh_config_api::{ConfigDto, ConfigPatch, ExecPatch, SubagentDto, SubagentPatch};

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{SessionActivity, SessionListEntry, SessionRunStatus};

    /// **G2 回归闸（消费侧）**：会话列表条目与会话活动快照的**权威类型**必须
    /// 解析得出 UI 真正要用的那几个字段。
    ///
    /// 与上面那条 config 闸同款用意：断言贴着「本仓实际读的字段」写，若有人把
    /// 依赖换成缩小版、或本仓又接回手抄件，这里立刻红。注意它**不**复制上游测试
    /// ——上游测的是上游的意图，这里测的是本仓的用量。
    #[test]
    fn session_payload_contract_exposes_fields_tui_needs() {
        let wire = serde_json::json!({
            "session_id": "0123abcd",
            "created_at": 1757800000000u64,
            "updated_at": 1757900000000u64,
            "model": "m1",
            "message_count": 3,
            "title": "Bun 引导 daemon",
            "cwd": "/home/x/Projects/qaqh-backend",
            "mode": 1,
            "archived": true,
            "ephemeral": false,
            "status": "working",
        });
        let entry: SessionListEntry =
            serde_json::from_value(wire.clone()).expect("会话列表条目可解析");
        // 列表渲染真正读的每一个字段（ui/home.rs、ui/overlays.rs、app/overlay_ops.rs）。
        assert_eq!(entry.meta.session_id, "0123abcd");
        assert!(entry.meta.archived && !entry.meta.ephemeral);
        assert_eq!(entry.status, SessionRunStatus::Working);
        assert_eq!(entry.meta.updated_at, 1757900000000);
        assert_eq!(entry.meta.display_title(), "Bun 引导 daemon");
        // 未分组/旧 daemon 不带 workspace_id 时必须仍是 `None` 而不是解析失败。
        assert_eq!(entry.workspace_id, None);
        // 2026-10-06 归一裁决：旧 `running: bool`（worker 存在性）退场，统一状态
        // 词表 `status` 落 wire。缺 `status` 键的旧回包必须兜默认 `not_running`，
        // **不得被误读成 idle**（与后端 `session_run_status_wire_vocabulary_is_locked` 同口径）。
        let mut legacy = wire.clone();
        legacy.as_object_mut().unwrap().remove("status");
        let legacy: SessionListEntry =
            serde_json::from_value(legacy).expect("缺 status 的旧回包仍须可解析");
        assert_eq!(legacy.status, SessionRunStatus::NotRunning);

        // **严格度差异（须知会）**：`session_id`/`created_at`/`updated_at`/`model`/
        // `message_count` 在 `SessionMeta` 上没有 `#[serde(default)]`，缺一个整条就被
        // 跳过；而 G2 之前那份手解把这些全当可选。实际不受影响——产出方就是
        // `SessionMeta` 本身，缺这些键的记录在 daemon 读盘那一步就已经被丢了，
        // 根本发不到 wire。这条断言是为了让「严格在哪」写成可执行的，而不是靠记忆。
        for required in [
            "session_id",
            "created_at",
            "updated_at",
            "model",
            "message_count",
        ] {
            let mut partial = wire.clone();
            partial.as_object_mut().unwrap().remove(required);
            assert!(
                serde_json::from_value::<SessionListEntry>(partial).is_err(),
                "{required} 缺失必须解析失败（无 serde(default)），严格度与手解不同"
            );
        }

        // 活动快照：本仓只用 session_id + state 两个键（app/mod.rs 的 activity_cache）。
        let acts: Vec<SessionActivity> = serde_json::from_value(serde_json::json!([
            { "session_id": "0123abcd", "state": "working", "turn_id": "t1", "seq": 3, "updated_at": 1 },
        ]))
        .expect("活动快照可解析");
        assert_eq!(acts[0].session_id, "0123abcd");
        assert_eq!(acts[0].state, qaqh_client::DomainActivityState::Working);
        assert_eq!(acts[0].turn_id.as_deref(), Some("t1"));
    }

    /// **压缩三态的消费侧闸门**（2026-10-09 后端把压缩的瞬态镜像并进 v2
    /// conversation 流）：三帧的 wire kind 名与本仓读的字段都得钉住。
    ///
    /// 这三帧**不入 fact 链、不进快照**，没有「回放补齐」这层安全网：后端改名或
    /// 挪字段，TUI 只会表现为「压缩卡永远不出现」——静默且难复现。所以断言写在
    /// 解析层，让它在这一轮就变红。
    #[test]
    fn compact_transient_frames_wire_vocabulary_is_locked() {
        use qaqh_client::{ClientV2CompactStatus, ClientV2ConversationDelta as D};

        let started: D = serde_json::from_value(serde_json::json!({
            "kind": "compact_started",
            "data": { "revision": 7, "compact_id": "c1", "turns_total": 12, "turns_keeping": 3 },
        }))
        .expect("CompactStarted 必须可解析");
        match started {
            D::CompactStarted {
                compact_id,
                turns_total,
                turns_keeping,
                ..
            } => {
                assert_eq!(compact_id, "c1");
                assert_eq!((turns_total, turns_keeping), (12, 3));
            }
            other => panic!("compact_started 的 wire 形状变了：{other:?}"),
        }

        // `delta` 是**累计全文**快照而不是 chunk 增量：本仓据此整帧替换。
        let progress: D = serde_json::from_value(serde_json::json!({
            "kind": "compact_progress",
            "data": { "revision": 8, "compact_id": "c1", "delta": "摘要全文" },
        }))
        .expect("CompactProgress 必须可解析");
        match progress {
            D::CompactProgress { delta, .. } => assert_eq!(delta, "摘要全文"),
            other => panic!("compact_progress 的 wire 形状变了：{other:?}"),
        }

        // 终态：四个 status 词都要认；`summaryChars` / `turns*` 是可选键
        //（后端 `skip_serializing_if` 掉了就整键缺席），缺席必须仍可解析。
        let finished: D = serde_json::from_value(serde_json::json!({
            "kind": "compact_finished",
            "data": { "revision": 9, "compact_id": "c1", "status": "cancelled" },
        }))
        .expect("CompactFinished 缺可选字段仍须可解析");
        match finished {
            D::CompactFinished { status, .. } => {
                assert_eq!(status, ClientV2CompactStatus::Cancelled)
            }
            other => panic!("compact_finished 的 wire 形状变了：{other:?}"),
        }
        for word in ["completed", "skipped", "failed"] {
            let frame = serde_json::json!({
                "kind": "compact_finished",
                "data": { "revision": 1, "compact_id": "c", "status": word },
            });
            match serde_json::from_value::<D>(frame).expect("终态词表可解析") {
                D::CompactFinished { .. } => {}
                other => panic!("期望 CompactFinished，得到 {other:?}"),
            }
        }
        // 词表是闭集：未知终态不得被兜成某个已知值（那会把失败读成完成）。
        let bogus = serde_json::json!({
            "kind": "compact_finished",
            "data": { "revision": 1, "compact_id": "c", "status": "aborted" },
        });
        assert!(
            serde_json::from_value::<D>(bogus).is_err(),
            "未知 compact status 必须解析失败"
        );
    }

    /// 每轮 provider 真值 usage 的落点（`TurnFinished.usage`）。
    ///
    /// 后端删掉 conversation 投影上的聚合 `usage` / `usage_totals` 之后，本仓的
    /// 单次用量只从 `AssistantBlockSealed` / `TurnFinished` 两条 delta 取，累计值
    /// 取 `SessionMeta.usage_totals`——两条源都要有闸。
    #[test]
    fn per_turn_usage_and_session_totals_are_the_only_two_sources() {
        use qaqh_client::ClientV2ConversationDelta as D;
        let finished: D = serde_json::from_value(serde_json::json!({
            "kind": "turn_finished",
            "data": {
                "revision": 1,
                "turn_id": "t1",
                "terminal": "completed",
                "usage": {
                    "prompt_tokens": 1200,
                    "completion_tokens": 300,
                    "total_tokens": 1500
                },
            },
        }))
        .expect("TurnFinished 带 usage 可解析");
        match finished {
            D::TurnFinished { usage, .. } => {
                let usage = usage.expect("usage 有值就必须解得出");
                assert_eq!(usage.prompt_tokens, 1200);
                assert_eq!(usage.completion_tokens, 300);
            }
            other => panic!("turn_finished 的 wire 形状变了：{other:?}"),
        }

        // 累计值：`session.list` 的 meta.usage_totals（cache% 的唯一分母之一）。
        let entry: SessionListEntry = serde_json::from_value(serde_json::json!({
            "session_id": "0123abcd",
            "created_at": 1,
            "updated_at": 2,
            "model": "m1",
            "message_count": 1,
            "status": "working",
            "usage_totals": {
                "prompt_tokens": 900,
                "completion_tokens": 100,
                "total_tokens": 1000
            },
        }))
        .expect("带 usage_totals 的条目可解析");
        assert_eq!(entry.meta.usage_totals.prompt_tokens, 900);
        assert_eq!(entry.meta.usage_totals.completion_tokens, 100);
    }

    /// T-11 回归闸：这几条断言**必须**对着权威 crate 成立，否则说明本仓又
    /// 悄悄接回了手抄件（或依赖被换成了缩小版）。断言刻意贴着「TUI 实际要用的
    /// 那几个字段」，而非上游测试的复制。
    #[test]
    fn config_contract_exposes_fields_tui_needs() {
        // 1) 写路径：权限档位可经 patch 下发并受值域校验（BUG-2026-09-13-15；
        //    2026-10-03 收敛为三档 1..=3，旧四档值 4 必须被拒）。
        let ok = ConfigPatch {
            permission_level: Some(3),
            ..Default::default()
        };
        ok.validate().expect("档位 3 合法");
        assert_eq!(serde_json::to_value(&ok).unwrap()["permissionLevel"], 3);

        let legacy = ConfigPatch {
            permission_level: Some(4),
            ..Default::default()
        };
        // 2026-10-09 后端 `qaqh-policy` 加了第四档 `SandboxRun = 4`，但写口
        // （`ConfigPatch::validate` / `config.set_permission_level` / config.toml 的
        // `permission_tier`）仍是 1..=3 —— 所以档位 4 现在**只能读、不能写**，设置页
        // 不提供该选项。后端哪天放开值域，这条断言与 `settings.rs` 的只读分支要一起改。
        assert!(legacy.validate().is_err(), "旧四档值 4 必须被拒");

        let bad = ConfigPatch {
            permission_level: Some(5),
            ..Default::default()
        };
        assert!(bad.validate().is_err(), "档位 5 必须被拒");

        // 1b) BYOK（2026-10-06 provider 目录退役）：设置页那两行直接读
        //     `wire` + `contextLength`——协议由用户声明，词表与值域都得由
        //     权威 crate 兜住，本仓不另立第二张表。
        let wire = ConfigPatch {
            wire: Some("anthropic".into()),
            ..Default::default()
        };
        wire.validate().expect("anthropic 是合法 wire");
        assert_eq!(serde_json::to_value(&wire).unwrap()["wire"], "anthropic");
        for bogus in ["openai-chat", "", "responses "] {
            let rejected = ConfigPatch {
                wire: Some(bogus.into()),
                ..Default::default()
            };
            assert!(rejected.validate().is_err(), "wire {bogus:?} 必须被拒");
        }
        assert!(
            ConfigPatch {
                context_length: Some(0),
                ..Default::default()
            }
            .validate()
            .is_err(),
            "contextLength = 0 必须被拒（它是本地压缩的分母）"
        );

        // 1c) 第四值 `gemini`（2026-10-08 后端 `Wire::Gemini`）：设置页的循环表
        //     `[&str; 4]` 必须与这里的值域同源——少一个值就是「循环里选不到」，
        //     多一个值就是「保存被 validate 拒」。
        for word in ["openai", "responses", "anthropic", "gemini"] {
            let patch = ConfigPatch {
                wire: Some(word.into()),
                ..Default::default()
            };
            patch
                .validate()
                .unwrap_or_else(|e| panic!("{word} 合法却被拒：{e}"));
            assert_eq!(serde_json::to_value(&patch).unwrap()["wire"], word);
        }
        assert_eq!(crate::app::settings::WIRE_PROTOCOLS.len(), 4);
        for declared in crate::app::settings::WIRE_PROTOCOLS {
            assert!(
                ConfigPatch {
                    wire: Some(declared.into()),
                    ..Default::default()
                }
                .validate()
                .is_ok(),
                "本地循环表里的 {declared:?} 不在后端值域内"
            );
        }

        // 1d) 2026-10-09 新开的两个可写字段：嵌套 `exec.defaultShell` 与
        //     `sessionIdleUnloadSecs`。camelCase 键名是设置页保存路径的唯一依赖，
        //     改名 / 挪层级都会在这里先红。
        let exec_patch = ConfigPatch {
            exec: Some(ExecPatch {
                default_shell: Some("pwsh".into()),
            }),
            session_idle_unload_secs: Some(0),
            ..Default::default()
        };
        exec_patch
            .validate()
            .expect("exec/sessionIdleUnloadSecs 可写；idle=0 是合法的「禁用」");
        let saved = serde_json::to_value(&exec_patch).expect("serialize");
        assert_eq!(saved["exec"]["defaultShell"], "pwsh");
        assert_eq!(saved["sessionIdleUnloadSecs"], 0);

        // 读路径同样够得着（设置页两行的 loaded 值）。
        let mut dto_payload = serde_json::to_value(ConfigDto::default()).expect("serialize");
        dto_payload["exec"]["defaultShell"] = serde_json::json!("bash");
        dto_payload["sessionIdleUnloadSecs"] = serde_json::json!(900);
        let dto: ConfigDto = serde_json::from_value(dto_payload).expect("读模型可解析");
        assert_eq!(dto.exec.default_shell.as_deref(), Some("bash"));
        assert_eq!(dto.session_idle_unload_secs, 900);

        // 2) 读路径：mcp/lsp 两段不再是盲区（T-11 前 ConfigDto 里没有）。
        //    载荷由权威类型自身生成——**完整**是它的默认状态。本仓要钉的是
        //    「这几个字段够得着」，wire 形状本身由后端
        //    `qaqh-config-api::tests::dto_parses_the_full_wire_shape` 锁住。
        let mut payload = serde_json::to_value(ConfigDto::default()).expect("serialize");
        payload["mcp"]["enabled"] = serde_json::json!(true);
        payload["lsp"]["idleShutdownSecs"] = serde_json::json!(600);
        payload["wire"] = serde_json::json!("anthropic");
        payload["contextLength"] = serde_json::json!(1_000_000);
        let dto: ConfigDto = serde_json::from_value(payload).expect("完整载荷必须可解析");
        assert!(dto.mcp.enabled);
        assert_eq!(dto.lsp.idle_shutdown_secs, 600);
        assert_eq!(dto.wire, "anthropic");
        assert_eq!(dto.context_length, 1_000_000);

        // 3) 反向闸：**残缺载荷必须失败**。G2 之前这里断言的是相反的行为
        //    （「旧 daemon 的 snake_case 形状仍须可解析」）——该兼容臂已按后端
        //    spec §0b 删除：前后端共进退，不留「另一个版本的对方」。
        //    留着 struct 级 default 更糟：未知键被忽略 + 缺字段走 default ⇒
        //    解析**成功**但得到一份全默认的配置，设置页会显示一堆空值。
        assert!(
            serde_json::from_value::<ConfigDto>(serde_json::json!({ "base_url": "x" })).is_err(),
            "残缺/旧形状载荷必须报错，不得静默降级成全默认"
        );
    }
}
