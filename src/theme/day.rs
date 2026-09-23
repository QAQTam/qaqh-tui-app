//! QAQH Day：亮色语义主题。

use ratatui::style::Color;

use super::ColorSupport;
use super::{
    AccentTokens, BorderTokens, ChromeTokens, DiffTokens, GlyphTokens, ModifierTokens,
    SemanticTokens, SpacingTokens, SurfaceTokens, TextTokens, Theme, TokenColor, markdown,
};

fn derived(support: ColorSupport, rgb: Color) -> Color {
    TokenColor::derived(rgb).resolve(support)
}

pub(super) fn theme(support: ColorSupport) -> Theme {
    Theme {
        surface: SurfaceTokens {
            base: derived(support, Color::Rgb(247, 248, 250)),
            dark: derived(support, Color::Rgb(238, 241, 245)),
            light: derived(support, Color::Rgb(255, 255, 255)),
            highlight: derived(support, Color::Rgb(230, 234, 240)),
            hover: derived(support, Color::Rgb(221, 227, 235)),
        },
        text: TextTokens {
            primary: derived(support, Color::Rgb(31, 41, 55)),
            secondary: derived(support, Color::Rgb(75, 85, 99)),
            dim: derived(support, Color::Rgb(156, 163, 175)),
            muted: derived(support, Color::Rgb(107, 114, 128)),
            bright: derived(support, Color::Rgb(75, 85, 99)),
        },
        accent: AccentTokens {
            user: derived(support, Color::Rgb(17, 24, 39)),
            assistant: derived(support, Color::Rgb(124, 58, 237)),
            thinking: derived(support, Color::Rgb(124, 58, 237)),
            tool: derived(support, Color::Rgb(15, 118, 110)),
            system: derived(support, Color::Rgb(37, 99, 235)),
            error: derived(support, Color::Rgb(220, 38, 38)),
            success: derived(support, Color::Rgb(22, 163, 74)),
            running: derived(support, Color::Rgb(8, 145, 178)),
        },
        semantic: SemanticTokens {
            command: derived(support, Color::Rgb(180, 83, 9)),
            path: derived(support, Color::Rgb(194, 65, 12)),
            warning: derived(support, Color::Rgb(180, 83, 9)),
            plan: derived(support, Color::Rgb(161, 98, 7)),
            verify: derived(support, Color::Rgb(124, 58, 237)),
        },
        chrome: ChromeTokens {
            border: derived(support, Color::Rgb(209, 213, 219)),
            border_active: derived(support, Color::Rgb(156, 163, 175)),
            selection: derived(support, Color::Rgb(219, 234, 254)),
            scrollbar: derived(support, Color::Rgb(199, 205, 214)),
        },
        diff: DiffTokens {
            add_fg: derived(support, Color::Rgb(21, 128, 61)),
            add_bg: derived(support, Color::Rgb(220, 252, 231)),
            del_fg: derived(support, Color::Rgb(185, 28, 28)),
            del_bg: derived(support, Color::Rgb(254, 226, 226)),
            equal_fg: derived(support, Color::Rgb(107, 114, 128)),
            gutter_fg: derived(support, Color::Rgb(156, 163, 175)),
        },
        markdown: markdown::day(support),
        glyph: GlyphTokens::default(),
        spacing: SpacingTokens::default(),
        border: BorderTokens::default(),
        modifier: ModifierTokens::default(),
    }
}
