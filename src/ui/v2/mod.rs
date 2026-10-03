//! V2 UI 组件。
//!
//! M3 先提供独立 transcript view model 与主题化渲染；M4-M5 的 composer、
//! status、workspace 与 modal 继续放在此目录下。

pub mod adapter;
pub mod button;
pub mod fullscreen;
pub mod scrollbar;
pub mod sidebar;
// P0-C 之后 hit.rs 的生产路径只剩「绘制登记 + resolve + 发布前校验」。
pub mod hit;
pub mod markdown;
pub mod modal;
pub mod route;
pub mod transcript;
pub mod workspace;

/// 工具名的人类展示形：`exec` → `Exec`，`todo_write` → `Todo Write`。
///
/// wire 上的名字是模型向标识符（小写 + `_`/`-` 分隔）；对用户一律转标题式。
/// 空段过滤让 `_a` / `a_` 这类边角名也不会多出空格。
pub fn display_tool_name(name: &str) -> String {
    name.split(['_', '-', '.'])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
