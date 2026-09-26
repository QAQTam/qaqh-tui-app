//! V2 UI 组件。
//!
//! M3 先提供独立 transcript view model 与主题化渲染；M4-M5 的 composer、
//! status、workspace 与 modal 继续放在此目录下。

pub mod adapter;
pub mod button;
pub mod fullscreen;
pub mod scrollbar;
// P0-C 之后 hit.rs 的生产路径只剩「绘制登记 + resolve + 发布前校验」。
pub mod hit;
pub mod markdown;
pub mod modal;
pub mod route;
pub mod transcript;
pub mod workspace;
