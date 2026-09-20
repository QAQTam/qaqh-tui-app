//! Terminal 原生主题：不覆盖终端 surface，保留用户配色。

use ratatui::style::{Color, Modifier};

use super::ColorSupport;
use super::{
    AccentTokens, BorderTokens, ChromeTokens, DiffTokens, GlyphTokens, ModifierTokens,
    SemanticTokens, SpacingTokens, SurfaceTokens, TextTokens, Theme, TokenColor, markdown,
};

fn native(support: ColorSupport, color: Color) -> Color {
    TokenColor::native(color).resolve(support)
}

pub(super) fn theme(support: ColorSupport) -> Theme {
    Theme {
        surface: SurfaceTokens {
            base: native(support, Color::Reset),
            dark: native(support, Color::Reset),
            light: native(support, Color::Reset),
            highlight: native(support, Color::Reset),
            hover: native(support, Color::Reset),
        },
        text: TextTokens {
            primary: native(support, Color::Reset),
            secondary: native(support, Color::Reset),
            dim: native(support, Color::DarkGray),
            muted: native(support, Color::DarkGray),
            bright: native(support, Color::Gray),
        },
        accent: AccentTokens {
            user: native(support, Color::White),
            assistant: native(support, Color::Magenta),
            thinking: native(support, Color::Magenta),
            tool: native(support, Color::Cyan),
            system: native(support, Color::Blue),
            error: native(support, Color::Red),
            success: native(support, Color::Green),
            running: native(support, Color::Cyan),
        },
        semantic: SemanticTokens {
            command: native(support, Color::Yellow),
            path: native(support, Color::Yellow),
            warning: native(support, Color::Yellow),
            plan: native(support, Color::Yellow),
            verify: native(support, Color::Magenta),
        },
        chrome: ChromeTokens {
            border: native(support, Color::DarkGray),
            border_active: native(support, Color::Reset),
            selection: native(support, Color::Reset),
            scrollbar: native(support, Color::DarkGray),
        },
        diff: DiffTokens {
            add_fg: native(support, Color::Green),
            add_bg: native(support, Color::Reset),
            del_fg: native(support, Color::Red),
            del_bg: native(support, Color::Reset),
            equal_fg: native(support, Color::DarkGray),
            gutter_fg: native(support, Color::DarkGray),
        },
        markdown: markdown::terminal(support),
        glyph: GlyphTokens::default(),
        spacing: SpacingTokens::default(),
        border: BorderTokens::default(),
        modifier: ModifierTokens {
            selection: Modifier::REVERSED,
            secondary: Modifier::DIM,
        },
    }
}
