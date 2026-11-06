//! Generative UI（A2UI v1.0 + qaqh-ui 围栏）渲染底座。
//!
//! 职责分层（对齐桌面端 `qaqh-desktop-app/src/genui/` 的同名职责）：
//!
//! - [`wire`]：A2UI v1.0 agent→renderer 消息信封与三态值（`@path`/`@call`）。
//!   权威参照 = 桌面端 `genui/v2/types.ts`（vendored 自
//!   a2ui-project/a2ui `specification/v1_0/json/agent_to_renderer.json`，锁 commit）。
//! - [`store`]：SurfaceRegistry——按序 patch 流 + 预算 + 渲染闸门
//!   （桌面端 `genui/v2/store.ts`）。
//! - [`eval`]：DynamicValue 求值（`@path` JSON Pointer / `@call` 白名单函数）。
//! - [`dsl`]：qaqh-ui 围栏（tsx/jsonl DSL）→ SurfaceIR → A2UI wire 的编译链
//!   （桌面端 `source/decoder.ts` + `compiler/{ir,parse,jsonl}.ts` + `v2/translate.ts`）。
//!
//! 为什么由 TUI 自持一套：daemon 对围栏零感知（只透明搬运 assistant 文本），
//! 编译发生在客户端；桌面端是 TS 实现，本模块是它的 Rust 移植件，漂移方向是
//! 「桌面端改、这里跟」，由 `dsl::tests` 的交叉验证 fixture 钉住。
//!
//! **两个刻意 bug-compatible 的桌面端行为**（契约测试钉住，修桌面端时两边同步）：
//!
//! 1. `dsl::translate::lower_expr` 把 `>=`/`<=` 降级为严格 `gt`/`lt`
//!    （translate.ts 的 kind 映射 `">=": "gt"`）——表达式丢失等号比较；
//! 2. `dsl::translate::translate_components` 的包装根只包一个组件
//!    （`rootId ?? forestRoots[0]`）——多顶层 surface 其余部分成为孤儿
//!    （软告警，不渲染）。
//!
//! 落地纪律：本模块**不是**后端协议的手抄镜像（对比 `protocol/mod.rs` 反对的
//! 手抄），不存在「漏字段静默不报错」的失败模式——wire 形状由 serde 反序列化
//! 和交叉验证测试约束。渲染接线（transcript/hit/焦点）在后续切片；接线完成前
//! 部分信封字段暂无调用点，模块级 `allow(dead_code)` 是临时的，随接线逐项摘除。

#![allow(dead_code)]

pub mod dsl;
pub mod eval;
pub mod store;
pub mod wire;
