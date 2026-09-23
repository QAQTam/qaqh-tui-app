//! V2 语义主题底座。
//!
//! M2 冻结 token 结构、主题选择和颜色降级；M3-M5 的组件只读取解析后的
//! [`Theme`]，不得直接写 `Color::*`。
//!
//! 主题在启动时解析一次并缓存，避免每帧读取环境变量或重复做颜色量化。

#![allow(dead_code)] // M2 先冻结完整 token 集，后续里程碑逐组消费。

mod color_support;
mod day;
mod markdown;
mod night;
mod terminal;

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier};

pub use color_support::{ColorSupport, quantize};

/// 用户可选择的主题。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    #[default]
    QaqhNight,
    QaqhDay,
    Terminal,
    Auto,
}

impl ThemeKind {
    /// 解析 `QAQH_THEME` 的值；未知值由调用方回退默认主题。
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("night") || value.eq_ignore_ascii_case("qaqh-night") {
            Some(Self::QaqhNight)
        } else if value.eq_ignore_ascii_case("day") || value.eq_ignore_ascii_case("qaqh-day") {
            Some(Self::QaqhDay)
        } else if value.eq_ignore_ascii_case("terminal") {
            Some(Self::Terminal)
        } else if value.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoTheme {
    Dark,
    Light,
}

/// 解析后的主题 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub surface: SurfaceTokens,
    pub text: TextTokens,
    pub accent: AccentTokens,
    pub semantic: SemanticTokens,
    pub chrome: ChromeTokens,
    pub diff: DiffTokens,
    pub markdown: MarkdownTokens,
    pub glyph: GlyphTokens,
    pub spacing: SpacingTokens,
    pub border: BorderTokens,
    pub modifier: ModifierTokens,
}

impl Theme {
    /// 生产环境唯一主题入口。
    ///
    /// 首次调用读取 `QAQH_THEME`、颜色能力和 Auto 亮暗判断，之后返回同一个
    /// 静态快照；因此绘制路径不承担环境探测或量化成本。
    pub fn current() -> &'static Self {
        static CURRENT: OnceLock<Theme> = OnceLock::new();
        CURRENT.get_or_init(|| {
            let kind = std::env::var("QAQH_THEME")
                .ok()
                .as_deref()
                .and_then(ThemeKind::parse)
                .unwrap_or_default();
            Self::resolve_with_auto(kind, ColorSupport::detect(), detect_auto_theme())
        })
    }

    pub(crate) fn resolve(kind: ThemeKind, support: ColorSupport) -> Self {
        Self::resolve_with_auto(kind, support, AutoTheme::Dark)
    }

    fn resolve_with_auto(kind: ThemeKind, support: ColorSupport, auto: AutoTheme) -> Self {
        match kind {
            ThemeKind::QaqhNight => night::theme(support),
            ThemeKind::QaqhDay => day::theme(support),
            ThemeKind::Terminal => terminal::theme(support),
            ThemeKind::Auto => match auto {
                AutoTheme::Dark => night::theme(support),
                AutoTheme::Light => day::theme(support),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceTokens {
    pub base: Color,
    pub dark: Color,
    pub light: Color,
    pub highlight: Color,
    pub hover: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTokens {
    pub primary: Color,
    pub secondary: Color,
    pub dim: Color,
    pub muted: Color,
    pub bright: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccentTokens {
    pub user: Color,
    pub assistant: Color,
    pub thinking: Color,
    pub tool: Color,
    pub system: Color,
    pub error: Color,
    pub success: Color,
    pub running: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticTokens {
    pub command: Color,
    pub path: Color,
    pub warning: Color,
    pub plan: Color,
    pub verify: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeTokens {
    pub border: Color,
    pub border_active: Color,
    pub selection: Color,
    pub scrollbar: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffTokens {
    pub add_fg: Color,
    pub add_bg: Color,
    pub del_fg: Color,
    pub del_bg: Color,
    pub equal_fg: Color,
    pub gutter_fg: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownTokens {
    pub h1: Color,
    pub h2: Color,
    pub h3: Color,
    pub h4: Color,
    pub h5: Color,
    pub h6: Color,
    pub text: Color,
    pub code: Color,
    pub code_bg: Color,
    pub link: Color,
    pub quote: Color,
    pub rule: Color,
    pub table_head: Color,
    pub task_done: Color,
    pub task_todo: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphTokens {
    pub user: &'static str,
    pub assistant: &'static str,
    pub thinking: &'static str,
    pub tool: &'static str,
    pub success: &'static str,
    pub failure: &'static str,
    pub running: &'static str,
    pub system: &'static str,
    pub quote: &'static str,
    pub fold: &'static str,
    pub truncated: &'static str,
    pub cursor: &'static str,
}

impl Default for GlyphTokens {
    fn default() -> Self {
        Self {
            user: "❯",
            assistant: "◆",
            thinking: "◇",
            tool: "⚙",
            success: "✓",
            failure: "✗",
            running: "◐",
            system: "·",
            quote: "▎",
            fold: "…",
            truncated: "◌",
            cursor: "▌",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpacingTokens {
    pub space_0: u16,
    pub space_1: u16,
    pub space_2: u16,
    pub space_3: u16,
    pub space_4: u16,
    pub rail_width: u16,
    pub block_pad_left: u16,
    pub block_pad_right: u16,
    pub outer_pad: u16,
    pub composer_min_height: u16,
    pub composer_max_height: u16,
    pub status_height: u16,
    pub shortcuts_height: u16,
}

impl Default for SpacingTokens {
    fn default() -> Self {
        Self {
            space_0: 0,
            space_1: 1,
            space_2: 2,
            space_3: 3,
            space_4: 4,
            rail_width: 1,
            block_pad_left: 2,
            block_pad_right: 2,
            outer_pad: 2,
            composer_min_height: 3,
            composer_max_height: 8,
            status_height: 1,
            shortcuts_height: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderTokens {
    pub none: &'static str,
    pub hairline: &'static str,
    pub rounded: &'static str,
    pub heavy: &'static str,
}

impl Default for BorderTokens {
    fn default() -> Self {
        Self {
            none: "",
            hairline: "│",
            rounded: "╭─╮│╰─╯",
            heavy: "┃",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierTokens {
    pub selection: Modifier,
    pub secondary: Modifier,
}

impl Default for ModifierTokens {
    fn default() -> Self {
        Self {
            selection: Modifier::empty(),
            secondary: Modifier::empty(),
        }
    }
}

/// 主题内部颜色规格：显式保存三档 fallback，解析后不再携带 palette 常量。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokenColor {
    truecolor: Color,
    ansi256: Color,
    ansi16: Color,
    native: bool,
}

impl TokenColor {
    pub(crate) const fn rgb(truecolor: Color, ansi256: Color, ansi16: Color) -> Self {
        Self {
            truecolor,
            ansi256,
            ansi16,
            native: false,
        }
    }

    pub(crate) fn derived(truecolor: Color) -> Self {
        Self {
            truecolor,
            ansi256: quantize(truecolor, ColorSupport::Ansi256),
            ansi16: quantize(truecolor, ColorSupport::Ansi16),
            native: false,
        }
    }

    pub(crate) const fn native(color: Color) -> Self {
        Self {
            truecolor: color,
            ansi256: color,
            ansi16: color,
            native: true,
        }
    }

    pub(crate) fn resolve(self, support: ColorSupport) -> Color {
        if support == ColorSupport::NoColor {
            return Color::Reset;
        }
        if self.native {
            return self.truecolor;
        }
        match support {
            ColorSupport::TrueColor => quantize(self.truecolor, support),
            ColorSupport::Ansi256 => quantize(self.ansi256, support),
            ColorSupport::Ansi16 => quantize(self.ansi16, support),
            ColorSupport::NoColor => Color::Reset,
        }
    }
}

fn detect_auto_theme() -> AutoTheme {
    auto_theme_from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
}

fn auto_theme_from_colorfgbg(value: Option<&str>) -> AutoTheme {
    value
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .map_or(AutoTheme::Dark, |background| {
            if background < 8 {
                AutoTheme::Dark
            } else {
                AutoTheme::Light
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_colors(theme: &Theme) -> Vec<Color> {
        vec![
            theme.surface.base,
            theme.surface.dark,
            theme.surface.light,
            theme.surface.highlight,
            theme.surface.hover,
            theme.text.primary,
            theme.text.secondary,
            theme.text.dim,
            theme.text.muted,
            theme.text.bright,
            theme.accent.user,
            theme.accent.assistant,
            theme.accent.thinking,
            theme.accent.tool,
            theme.accent.system,
            theme.accent.error,
            theme.accent.success,
            theme.accent.running,
            theme.semantic.command,
            theme.semantic.path,
            theme.semantic.warning,
            theme.semantic.plan,
            theme.semantic.verify,
            theme.chrome.border,
            theme.chrome.border_active,
            theme.chrome.selection,
            theme.chrome.scrollbar,
            theme.diff.add_fg,
            theme.diff.add_bg,
            theme.diff.del_fg,
            theme.diff.del_bg,
            theme.diff.equal_fg,
            theme.diff.gutter_fg,
            theme.markdown.h1,
            theme.markdown.h2,
            theme.markdown.h3,
            theme.markdown.h4,
            theme.markdown.h5,
            theme.markdown.h6,
            theme.markdown.text,
            theme.markdown.code,
            theme.markdown.code_bg,
            theme.markdown.link,
            theme.markdown.quote,
            theme.markdown.rule,
            theme.markdown.table_head,
            theme.markdown.task_done,
            theme.markdown.task_todo,
        ]
    }

    fn snapshot(theme: &Theme) -> String {
        format!(
            "surface.base={:?}; text.primary={:?}; accent.user={:?}; accent.assistant={:?}; \
             accent.tool={:?}; accent.error={:?}; accent.success={:?}; semantic.command={:?}; \
             chrome.border={:?}; diff.add_fg={:?}; md.h1={:?}; md.code_bg={:?}",
            theme.surface.base,
            theme.text.primary,
            theme.accent.user,
            theme.accent.assistant,
            theme.accent.tool,
            theme.accent.error,
            theme.accent.success,
            theme.semantic.command,
            theme.chrome.border,
            theme.diff.add_fg,
            theme.markdown.h1,
            theme.markdown.code_bg,
        )
    }

    fn rgb(color: Color) -> (u8, u8, u8) {
        match quantize(color, ColorSupport::TrueColor) {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected RGB token, got {other:?}"),
        }
    }

    fn relative_luminance(color: Color) -> f64 {
        let (r, g, b) = rgb(color);
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast_ratio(foreground: Color, background: Color) -> f64 {
        let foreground = relative_luminance(foreground);
        let background = relative_luminance(background);
        let (lighter, darker) = if foreground > background {
            (foreground, background)
        } else {
            (background, foreground)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn theme_kind_parse_is_case_insensitive_and_aliases_qaqh_names() {
        assert_eq!(ThemeKind::parse("night"), Some(ThemeKind::QaqhNight));
        assert_eq!(ThemeKind::parse("QAQH-DAY"), Some(ThemeKind::QaqhDay));
        assert_eq!(ThemeKind::parse("terminal"), Some(ThemeKind::Terminal));
        assert_eq!(ThemeKind::parse("auto"), Some(ThemeKind::Auto));
        assert_eq!(ThemeKind::parse("unknown"), None);
    }

    #[test]
    fn auto_theme_uses_background_component_and_defaults_dark() {
        assert_eq!(auto_theme_from_colorfgbg(Some("15;0")), AutoTheme::Dark);
        assert_eq!(auto_theme_from_colorfgbg(Some("0;15")), AutoTheme::Light);
        assert_eq!(auto_theme_from_colorfgbg(Some("broken")), AutoTheme::Dark);
        assert_eq!(auto_theme_from_colorfgbg(None), AutoTheme::Dark);
    }

    #[test]
    fn night_theme_has_semantic_contrast() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let ratio = contrast_ratio(theme.text.primary, theme.surface.base);
        assert!(ratio >= 7.0, "night contrast ratio was {ratio:.2}");
    }

    #[test]
    fn day_theme_has_semantic_contrast() {
        let theme = Theme::resolve(ThemeKind::QaqhDay, ColorSupport::TrueColor);
        let ratio = contrast_ratio(theme.text.primary, theme.surface.base);
        assert!(ratio >= 7.0, "day contrast ratio was {ratio:.2}");
    }

    #[test]
    fn night_ansi256_uses_explicit_fallbacks() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::Ansi256);
        assert_eq!(theme.surface.base, Color::Indexed(232));
        assert_eq!(theme.text.primary, Color::Indexed(255));
        assert_eq!(theme.accent.assistant, Color::Indexed(139));
        assert_eq!(theme.accent.error, Color::Indexed(131));
        assert_eq!(theme.semantic.path, Color::Indexed(173));
    }

    #[test]
    fn night_ansi16_uses_named_fallbacks() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::Ansi16);
        assert_eq!(theme.surface.base, Color::Black);
        assert_eq!(theme.text.primary, Color::White);
        assert_eq!(theme.accent.assistant, Color::Magenta);
        assert_eq!(theme.accent.tool, Color::Cyan);
        assert_eq!(theme.semantic.path, Color::Yellow);
    }

    #[test]
    fn no_color_never_leaks_palette_colors() {
        for kind in [
            ThemeKind::QaqhNight,
            ThemeKind::QaqhDay,
            ThemeKind::Terminal,
        ] {
            let theme = Theme::resolve(kind, ColorSupport::NoColor);
            assert!(
                theme_colors(&theme)
                    .iter()
                    .all(|color| *color == Color::Reset),
                "{kind:?} leaked color under NO_COLOR"
            );
        }
    }

    #[test]
    fn terminal_theme_uses_reset_surfaces_and_reversed_selection() {
        let theme = Theme::resolve(ThemeKind::Terminal, ColorSupport::TrueColor);
        assert_eq!(theme.surface.base, Color::Reset);
        assert_eq!(theme.surface.dark, Color::Reset);
        assert_eq!(theme.surface.light, Color::Reset);
        assert_eq!(theme.surface.highlight, Color::Reset);
        assert_eq!(theme.surface.hover, Color::Reset);
        assert_eq!(theme.text.primary, Color::Reset);
        assert_eq!(theme.modifier.selection, Modifier::REVERSED);
    }

    #[test]
    fn theme_snapshots_are_stable() {
        let night = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        assert_eq!(
            snapshot(&night),
            "surface.base=Rgb(11, 13, 16); text.primary=Rgb(230, 233, 239); \
             accent.user=Rgb(216, 222, 233); accent.assistant=Rgb(180, 142, 173); \
             accent.tool=Rgb(143, 188, 187); accent.error=Rgb(191, 97, 106); \
             accent.success=Rgb(163, 190, 140); semantic.command=Rgb(235, 203, 139); \
             chrome.border=Rgb(46, 52, 64); diff.add_fg=Rgb(163, 190, 140); \
             md.h1=Rgb(143, 188, 187); md.code_bg=Rgb(20, 24, 31)"
        );

        let day = Theme::resolve(ThemeKind::QaqhDay, ColorSupport::TrueColor);
        assert_eq!(
            snapshot(&day),
            "surface.base=Rgb(247, 248, 250); text.primary=Rgb(31, 41, 55); \
             accent.user=Rgb(17, 24, 39); accent.assistant=Rgb(124, 58, 237); \
             accent.tool=Rgb(15, 118, 110); accent.error=Rgb(220, 38, 38); \
             accent.success=Rgb(22, 163, 74); semantic.command=Rgb(180, 83, 9); \
             chrome.border=Rgb(209, 213, 219); diff.add_fg=Rgb(21, 128, 61); \
             md.h1=Rgb(15, 118, 110); md.code_bg=Rgb(241, 245, 249)"
        );

        let terminal = Theme::resolve(ThemeKind::Terminal, ColorSupport::TrueColor);
        assert_eq!(
            snapshot(&terminal),
            "surface.base=Reset; text.primary=Reset; accent.user=White; \
             accent.assistant=Magenta; accent.tool=Cyan; accent.error=Red; \
             accent.success=Green; semantic.command=Yellow; chrome.border=DarkGray; \
             diff.add_fg=Green; md.h1=Cyan; md.code_bg=Reset"
        );

        let auto_dark = Theme::resolve(ThemeKind::Auto, ColorSupport::TrueColor);
        assert_eq!(snapshot(&auto_dark), snapshot(&night));
        let auto_light =
            Theme::resolve_with_auto(ThemeKind::Auto, ColorSupport::TrueColor, AutoTheme::Light);
        assert_eq!(snapshot(&auto_light), snapshot(&day));
    }

    #[test]
    fn theme_is_copy_and_bounded() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Theme>();
        assert!(
            std::mem::size_of::<Theme>() <= 512,
            "Theme grew to {} bytes",
            std::mem::size_of::<Theme>()
        );
    }

    #[test]
    fn current_theme_is_cached() {
        assert!(std::ptr::eq(Theme::current(), Theme::current()));
    }
}
