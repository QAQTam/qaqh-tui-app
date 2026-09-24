//! V2 terminal primitives.
//!
//! M1 只提供隔离原型：
//! - [`commit`]：已提交块的幂等账本；
//! - [`inline`]：`--v2-inline` 隔离原型；
//! - [`agent`]：默认的 inline Agent View（`--v2-agent` 仍可显式选择）。
//!
//! alpha1 起 Agent View 是默认 UI；Ringing 协议切换另行推进，当前仍走 v1。

pub mod agent;
pub mod commit;
pub mod inline;
pub mod transcript;
