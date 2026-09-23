//! V2 terminal primitives.
//!
//! M1 只提供隔离原型：
//! - [`commit`]：已提交块的幂等账本；
//! - [`inline`]：`--v2-inline` 隔离原型；
//! - [`agent`]：`--v2-agent` 真实事件循环的 inline Agent 外壳。
//!
//! 默认 v1 全屏路径不依赖本模块。

pub mod agent;
pub mod commit;
pub mod inline;
pub mod transcript;
