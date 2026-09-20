//! Markdown 语义 token 的三套主题映射。
//!
//! 颜色只在这里定义；后续 markdown 渲染器只读取 [`MarkdownTokens`]。

use ratatui::style::Color;

use super::{ColorSupport, MarkdownTokens, TokenColor};

fn derived(support: ColorSupport, color: Color) -> Color {
    TokenColor::derived(color).resolve(support)
}

fn token(support: ColorSupport, rgb: Color, ansi256: u8, ansi16: Color) -> Color {
    TokenColor::rgb(rgb, Color::Indexed(ansi256), ansi16).resolve(support)
}

pub(super) fn night(support: ColorSupport) -> MarkdownTokens {
    MarkdownTokens {
        h1: token(support, Color::Rgb(143, 188, 187), 109, Color::Cyan),
        h2: token(support, Color::Rgb(129, 161, 193), 110, Color::Blue),
        h3: token(support, Color::Rgb(180, 142, 173), 139, Color::Magenta),
        h4: token(support, Color::Rgb(154, 163, 178), 247, Color::Gray),
        h5: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
        h6: token(support, Color::Rgb(91, 100, 114), 240, Color::DarkGray),
        text: token(support, Color::Rgb(184, 191, 204), 250, Color::Gray),
        code: token(support, Color::Rgb(136, 192, 208), 110, Color::Cyan),
        code_bg: token(support, Color::Rgb(20, 24, 31), 234, Color::Black),
        link: token(support, Color::Rgb(129, 161, 193), 110, Color::Blue),
        quote: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
        rule: token(support, Color::Rgb(76, 86, 106), 240, Color::DarkGray),
        table_head: token(support, Color::Rgb(143, 188, 187), 109, Color::Cyan),
        task_done: token(support, Color::Rgb(163, 190, 140), 108, Color::Green),
        task_todo: token(support, Color::Rgb(184, 191, 204), 250, Color::Gray),
    }
}

pub(super) fn day(support: ColorSupport) -> MarkdownTokens {
    MarkdownTokens {
        h1: derived(support, Color::Rgb(15, 118, 110)),
        h2: derived(support, Color::Rgb(37, 99, 235)),
        h3: derived(support, Color::Rgb(124, 58, 237)),
        h4: derived(support, Color::Rgb(75, 85, 99)),
        h5: derived(support, Color::Rgb(107, 114, 128)),
        h6: derived(support, Color::Rgb(156, 163, 175)),
        text: derived(support, Color::Rgb(75, 85, 99)),
        code: derived(support, Color::Rgb(15, 118, 110)),
        code_bg: derived(support, Color::Rgb(241, 245, 249)),
        link: derived(support, Color::Rgb(37, 99, 235)),
        quote: derived(support, Color::Rgb(107, 114, 128)),
        rule: derived(support, Color::Rgb(209, 213, 219)),
        table_head: derived(support, Color::Rgb(15, 118, 110)),
        task_done: derived(support, Color::Rgb(22, 163, 74)),
        task_todo: derived(support, Color::Rgb(75, 85, 99)),
    }
}

pub(super) fn terminal(support: ColorSupport) -> MarkdownTokens {
    let native = |color| TokenColor::native(color).resolve(support);
    MarkdownTokens {
        h1: native(Color::Cyan),
        h2: native(Color::Blue),
        h3: native(Color::Magenta),
        h4: native(Color::Gray),
        h5: native(Color::DarkGray),
        h6: native(Color::DarkGray),
        text: native(Color::Reset),
        code: native(Color::Cyan),
        code_bg: native(Color::Reset),
        link: native(Color::Blue),
        quote: native(Color::DarkGray),
        rule: native(Color::DarkGray),
        table_head: native(Color::Cyan),
        task_done: native(Color::Green),
        task_todo: native(Color::Reset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_tokens_follow_requested_support() {
        let night_16 = night(ColorSupport::Ansi16);
        assert_eq!(night_16.h1, Color::Cyan);
        assert_eq!(night_16.task_done, Color::Green);

        let terminal = terminal(ColorSupport::TrueColor);
        assert_eq!(terminal.code_bg, Color::Reset);
        assert_eq!(terminal.h2, Color::Blue);
    }
}
