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
    // 第三档（ansi16）刻意避开 `Blue` / `Magenta` / `Green` 这些**暗命名色**：
    // 在绝大多数终端的黑色背景上它们只有 1.3–4.4:1，标题/链接会直接糊掉。
    // 16 色盘里没有可读的"深蓝/深紫"，所以色相降级为 `Cyan` / `LightMagenta`，
    // 优先保证可读性。详见 `night_ansi16_markdown_is_readable_on_black`。
    MarkdownTokens {
        h1: token(support, Color::Rgb(143, 188, 187), 109, Color::LightCyan),
        h2: token(support, Color::Rgb(129, 161, 193), 110, Color::Cyan),
        h3: token(support, Color::Rgb(180, 142, 173), 139, Color::LightMagenta),
        h4: token(support, Color::Rgb(154, 163, 178), 247, Color::Gray),
        h5: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
        h6: token(support, Color::Rgb(91, 100, 114), 240, Color::DarkGray),
        text: token(support, Color::Rgb(184, 191, 204), 250, Color::Gray),
        code: token(support, Color::Rgb(136, 192, 208), 110, Color::LightCyan),
        code_bg: token(support, Color::Rgb(20, 24, 31), 234, Color::Black),
        link: token(support, Color::Rgb(129, 161, 193), 110, Color::Cyan),
        quote: token(support, Color::Rgb(123, 132, 148), 244, Color::DarkGray),
        rule: token(support, Color::Rgb(76, 86, 106), 240, Color::DarkGray),
        table_head: token(support, Color::Rgb(143, 188, 187), 109, Color::LightCyan),
        task_done: token(support, Color::Rgb(163, 190, 140), 108, Color::LightGreen),
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

    /// 经典 16 色 RGB（与 `color_support::named_rgb` 同一模型：VGA 暗色，
    /// 是所有终端调色板里最悲观的一档）。
    fn ansi16_rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Black => (0, 0, 0),
            Color::Red => (128, 0, 0),
            Color::Green => (0, 128, 0),
            Color::Yellow => (128, 128, 0),
            Color::Blue => (0, 0, 128),
            Color::Magenta => (128, 0, 128),
            Color::Cyan => (0, 128, 128),
            Color::Gray => (192, 192, 192),
            Color::DarkGray => (128, 128, 128),
            Color::LightRed => (255, 0, 0),
            Color::LightGreen => (0, 255, 0),
            Color::LightYellow => (255, 255, 0),
            Color::LightBlue => (0, 0, 255),
            Color::LightMagenta => (255, 0, 255),
            Color::LightCyan => (0, 255, 255),
            Color::White => (255, 255, 255),
            other => panic!("not a named ansi16 color: {other:?}"),
        }
    }

    fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
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

    fn contrast_against_black(color: Color) -> f64 {
        (luminance(ansi16_rgb(color)) + 0.05) / 0.05
    }

    #[test]
    fn markdown_tokens_follow_requested_support() {
        let night_16 = night(ColorSupport::Ansi16);
        assert_eq!(night_16.h1, Color::LightCyan);
        assert_eq!(night_16.task_done, Color::LightGreen);

        let terminal = terminal(ColorSupport::TrueColor);
        assert_eq!(terminal.code_bg, Color::Reset);
        assert_eq!(terminal.h2, Color::Blue);
    }

    /// 16 色兜底必须在黑底上可读。旧版把 h2/link 落到 `Blue`（1.31:1）、h3 落到
    /// `Magenta`（2.23:1）、task_done 落到 `Green`（4.09:1）——标题和链接直接
    /// 糊进黑色背景，用户看到的就是"难看的蓝紫"。
    ///
    /// 阈值取 4.0 而不是 4.5：`Cyan` 在本仓的 VGA 模型下是 4.40:1，而真实终端
    /// 的 cyan 普遍更亮（xterm `#00cdcd` ≈ 7.4:1、Windows Terminal `#3a96dd`
    /// ≈ 4.5:1）；标题另外还有 `Modifier::BOLD`。
    #[test]
    fn night_ansi16_markdown_is_readable_on_black() {
        let tokens = night(ColorSupport::Ansi16);
        for (name, color) in [
            ("h1", tokens.h1),
            ("h2", tokens.h2),
            ("h3", tokens.h3),
            ("h4", tokens.h4),
            ("h5", tokens.h5),
            ("h6", tokens.h6),
            ("text", tokens.text),
            ("code", tokens.code),
            ("link", tokens.link),
            ("quote", tokens.quote),
            ("rule", tokens.rule),
            ("table_head", tokens.table_head),
            ("task_done", tokens.task_done),
            ("task_todo", tokens.task_todo),
        ] {
            let ratio = contrast_against_black(color);
            assert!(
                ratio >= 4.0,
                "ansi16 markdown.{name} = {color:?} contrast on black was {ratio:.2}"
            );
        }
    }
}
