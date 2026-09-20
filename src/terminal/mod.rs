//! V2 terminal primitives.
//!
//! M1 只提供隔离原型：
//! - [`commit`]：已提交块的幂等账本；
//! - [`inline`]：`--v2-inline` 实验入口。
//!
//! 默认 v1 全屏路径不依赖本模块。

pub mod commit;
pub mod inline;
