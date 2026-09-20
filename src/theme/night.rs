//! QAQH Night：默认深色语义主题。

use ratatui::style::Color;

use super::ColorSupport;
use super::{
    AccentTokens, ChromeTokens, DiffTokens, GlyphTokens, ModifierTokens, SemanticTokens,
    SpacingTokens, SurfaceTokens, TextTokens, Theme, TokenColor, markdown,
};

fn token(support: ColorSupport, rgb: Color, ansi256: u8, ansi16: Color) -> Color {
    TokenColor::rgb(rgb, Color::Indexed(ansi256), ansi16).resolve(support)
}

fn derived(support: ColorSupport, rgb: Color) -> Color {
    TokenColor::derived(rgb).resolve(support)
}

pub(super) fn theme(support: ColorSupport) -> Theme {
    Theme {
        surface: SurfaceTokens {
            base: token(support, Color::Rgb(11, 13, 16), 232, Color::Black),
            dark: token(support, Color::Rgb(8, 9, 11), 233, Color::Black),
            light: token(support, Color::Rgb(21, 25, 34), 234, Color::Black),
            highlight: token(support, Color::Rgb(29, 36, 48), 235, Color::Black),
            hover: token(support, Color::Rgb(37, 45, 59), 236, Color::DarkGray),
        },
        text: TextTokens {
            primary: token(support, Color::Rgb(230, 233, 239), 255, Color::White),
            secondary: token(support, Color::Rgb(184, 191, 204), 250, Color::Gray),
            dim: token(support, Color::Rgb(91, 100, 114), 240, Color::DarkGray),
            muted: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
            bright: token(support, Color::Rgb(154, 163, 178), 247, Color::Gray),
        },
        accent: AccentTokens {
            user: token(support, Color::Rgb(216, 222, 233), 253, Color::White),
            assistant: token(support, Color::Rgb(180, 142, 173), 139, Color::Magenta),
            thinking: token(support, Color::Rgb(180, 142, 173), 139, Color::Magenta),
            tool: token(support, Color::Rgb(143, 188, 187), 109, Color::Cyan),
            system: token(support, Color::Rgb(129, 161, 193), 110, Color::Blue),
            error: token(support, Color::Rgb(191, 97, 106), 131, Color::Red),
            success: token(support, Color::Rgb(163, 190, 140), 108, Color::Green),
            running: token(support, Color::Rgb(136, 192, 208), 110, Color::Cyan),
        },
        semantic: SemanticTokens {
            command: token(support, Color::Rgb(235, 203, 139), 180, Color::Yellow),
            path: token(support, Color::Rgb(208, 135, 112), 173, Color::Yellow),
            warning: token(support, Color::Rgb(235, 203, 139), 180, Color::Yellow),
            plan: token(support, Color::Rgb(229, 192, 123), 180, Color::Yellow),
            verify: token(support, Color::Rgb(180, 142, 173), 139, Color::Magenta),
        },
        chrome: ChromeTokens {
            border: token(support, Color::Rgb(46, 52, 64), 236, Color::DarkGray),
            border_active: token(support, Color::Rgb(76, 86, 106), 240, Color::Gray),
            selection: token(support, Color::Rgb(59, 66, 82), 237, Color::DarkGray),
            scrollbar: token(support, Color::Rgb(67, 76, 94), 238, Color::DarkGray),
        },
        diff: DiffTokens {
            add_fg: token(support, Color::Rgb(163, 190, 140), 108, Color::Green),
            add_bg: token(support, Color::Rgb(31, 42, 31), 22, Color::Green),
            del_fg: token(support, Color::Rgb(191, 97, 106), 131, Color::Red),
            del_bg: token(support, Color::Rgb(46, 27, 30), 52, Color::Red),
            equal_fg: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
            gutter_fg: token(support, Color::Rgb(91, 100, 114), 240, Color::DarkGray),
        },
        markdown: markdown::night(support),
        glyph: GlyphTokens::default(),
        spacing: SpacingTokens::default(),
        border: super::BorderTokens::default(),
        modifier: ModifierTokens::default(),
    }
}
