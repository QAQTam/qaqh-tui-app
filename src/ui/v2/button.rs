//! 统一的按钮状态与样式（spec §5）。
//!
//! 全仓只有这一份 `hovered / pressed / disabled / focused` → 样式的推导。
//! 页面只负责提供"这一帧的指针状态 + 自己的焦点底色"，不再各自定义颜色规则。
//!
//! 优先级（spec §5.2）：`Disabled > Pressed > Hovered > Focused > Idle`。

use ratatui::style::{Color, Modifier, Style};

use crate::theme::Theme;

/// 归一化后的按钮状态（spec §5.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonState {
    #[default]
    Idle,
    Hovered,
    Pressed,
    Disabled,
}

/// 一个按钮在这一帧的视觉：归一化状态 + 是否聚焦。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonVisual {
    pub state: ButtonState,
    pub focused: bool,
}

impl ButtonVisual {
    /// 从原始指针状态推导。
    ///
    /// - `pressed` 必须**同时** hovered 才算按下：按下后把指针拖出按钮，视觉立刻
    ///   回常态，松开也不会提交（spec §5.2 / §5.3）；
    /// - disabled 压过一切，连 hover/pressed 都不显示。
    pub const fn derive(enabled: bool, focused: bool, hovered: bool, pressed: bool) -> Self {
        let state = if !enabled {
            ButtonState::Disabled
        } else if pressed && hovered {
            ButtonState::Pressed
        } else if hovered {
            ButtonState::Hovered
        } else {
            ButtonState::Idle
        };
        Self { state, focused }
    }

    pub const fn state(self) -> ButtonState {
        self.state
    }

    pub const fn focused(self) -> bool {
        self.focused
    }

    /// 交互底色（整块生效，不是只改文字色）。
    ///
    /// `focused_bg` 是调用方自己的"焦点 / 选中"底色（例如 Workspace 的
    /// `chrome.selection`）。它只在 `Idle` 且 `focused` 时生效——被 hover/pressed
    /// 覆盖，正是 spec §5.2 的优先级。用 `▶` 前缀表达焦点的页面传
    /// `Color::Reset` 即可。
    ///
    /// 拿不到颜色（`Reset`：无彩色主题 / `NO_COLOR`）时退回 `REVERSED`，
    /// 否则悬停在这类终端里完全不可见（实测踩过）。
    pub fn surface_style(self, theme: &Theme, focused_bg: Color) -> Style {
        match self.state {
            ButtonState::Disabled => Style::new(),
            ButtonState::Pressed => surface(theme.surface.highlight).add_modifier(Modifier::BOLD),
            ButtonState::Hovered => surface(theme.surface.hover),
            ButtonState::Idle if self.focused => surface(focused_bg),
            ButtonState::Idle => Style::new(),
        }
    }

    /// 前景色；disabled 必须与正常态可区分。
    pub fn foreground(self, theme: &Theme) -> Style {
        match self.state {
            ButtonState::Disabled => Style::new().fg(theme.text.dim),
            _ => Style::new().fg(theme.text.primary),
        }
    }
}

/// 底色：`Reset` 时退回反显。
fn surface(color: Color) -> Style {
    if color == Color::Reset {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().bg(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    fn no_color_theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::NoColor)
    }

    /// spec §5.2 的优先级表必须逐条成立。
    #[test]
    fn priority_is_disabled_then_pressed_then_hovered() {
        // disabled 压过 hover + pressed
        assert_eq!(
            ButtonVisual::derive(false, true, true, true).state(),
            ButtonState::Disabled
        );
        // pressed 要求同时 hovered
        assert_eq!(
            ButtonVisual::derive(true, false, true, true).state(),
            ButtonState::Pressed
        );
        assert_eq!(
            ButtonVisual::derive(true, false, false, true).state(),
            ButtonState::Idle,
            "按下但指针不在按钮上 → 不是按下态"
        );
        // hovered 压过 focused
        let hovered = ButtonVisual::derive(true, true, true, false);
        assert_eq!(hovered.state(), ButtonState::Hovered);
        assert!(hovered.focused());
        // idle + focused 才轮到焦点
        let focused = ButtonVisual::derive(true, true, false, false);
        assert_eq!(focused.state(), ButtonState::Idle);
        assert!(focused.focused());
    }

    /// 焦点底色只在 Idle 生效，被 hover/pressed 覆盖。
    #[test]
    fn focused_surface_only_applies_when_idle() {
        let theme = theme();
        let selection = theme.chrome.selection;

        let focused = ButtonVisual::derive(true, true, false, false);
        assert_eq!(focused.surface_style(&theme, selection).bg, Some(selection));

        let hovered = ButtonVisual::derive(true, true, true, false);
        assert_eq!(
            hovered.surface_style(&theme, selection).bg,
            Some(theme.surface.hover),
            "hovered 必须压过 focused"
        );

        let pressed = ButtonVisual::derive(true, true, true, true);
        assert_eq!(
            pressed.surface_style(&theme, selection).bg,
            Some(theme.surface.highlight),
            "pressed 必须压过 hovered/focused"
        );

        // 没有焦点底色的页面（Modal 用 `▶` 表达焦点）
        let idle = ButtonVisual::derive(true, false, false, false);
        assert_eq!(idle.surface_style(&theme, selection).bg, None);
    }

    /// 无彩色终端：hover / pressed 必须退回反显，否则完全不可见。
    #[test]
    fn no_color_falls_back_to_reversed_and_pressed_adds_bold() {
        let theme = no_color_theme();
        assert_eq!(theme.surface.hover, Color::Reset);
        assert_eq!(theme.surface.highlight, Color::Reset);

        let hovered =
            ButtonVisual::derive(true, false, true, false).surface_style(&theme, Color::Reset);
        assert!(hovered.add_modifier.contains(Modifier::REVERSED));
        assert!(!hovered.add_modifier.contains(Modifier::BOLD));

        let pressed =
            ButtonVisual::derive(true, false, true, true).surface_style(&theme, Color::Reset);
        assert!(pressed.add_modifier.contains(Modifier::REVERSED));
        assert!(
            pressed.add_modifier.contains(Modifier::BOLD),
            "无彩色终端里 pressed 只靠反显区分不出来，必须再加粗"
        );
    }

    /// disabled 必须与正常态可区分。
    #[test]
    fn disabled_is_distinguishable_from_idle() {
        let theme = theme();
        let disabled = ButtonVisual::derive(false, false, false, false);
        let idle = ButtonVisual::derive(true, false, false, false);

        assert_eq!(disabled.surface_style(&theme, Color::Reset).bg, None);
        assert_eq!(idle.surface_style(&theme, Color::Reset).bg, None);
        assert_eq!(disabled.foreground(&theme).fg, Some(theme.text.dim));
        assert_eq!(idle.foreground(&theme).fg, Some(theme.text.primary));
        assert_ne!(disabled.foreground(&theme).fg, idle.foreground(&theme).fg);
    }
}
