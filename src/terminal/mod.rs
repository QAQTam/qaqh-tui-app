//! V2 terminal primitives.
//!
//! - [`commit`]：已提交块的幂等账本；
//! - [`agent`]：生产 Agent shell（默认 fullscreen；v1 是独立兼容路径）。
//!
//! `--v2-inline` 的 M1 隔离原型已退役；inline production shell 仍由
//! [`agent`] 的兼容分支承载，待 fullscreen parity 完成后再删除。

pub mod agent;
pub mod clipboard;
pub mod commit;
pub mod transcript;
