//! 终端颜色能力探测与降级。
//!
//! 主题在启动时解析一次；组件每帧只读取已量化 token，不重复探测环境。

use std::ffi::OsStr;

use ratatui::style::Color;

/// 终端可安全输出的颜色能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ColorSupport {
    /// 从常见终端环境变量探测能力。
    pub fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let colorterm = std::env::var_os("COLORTERM");
        let term = std::env::var_os("TERM");
        Self::detect_from(no_color, colorterm.as_deref(), term.as_deref())
    }

    fn detect_from(no_color: bool, colorterm: Option<&OsStr>, term: Option<&OsStr>) -> Self {
        if no_color || os_eq_ignore_ascii_case(term, "dumb") {
            return Self::NoColor;
        }
        if os_eq_ignore_ascii_case(colorterm, "truecolor")
            || os_eq_ignore_ascii_case(colorterm, "24bit")
        {
            return Self::TrueColor;
        }
        if os_contains_ignore_ascii_case(term, "256color") {
            return Self::Ansi256;
        }
        Self::Ansi16
    }
}

/// 把任意 ratatui 颜色量化到指定能力档。
///
/// 主题解析只在启动时调用；函数本身不分配内存。
pub fn quantize(color: Color, support: ColorSupport) -> Color {
    match support {
        ColorSupport::TrueColor => to_truecolor(color),
        ColorSupport::Ansi256 => to_ansi256(color),
        ColorSupport::Ansi16 => to_ansi16(color),
        ColorSupport::NoColor => Color::Reset,
    }
}

fn os_eq_ignore_ascii_case(value: Option<&OsStr>, expected: &str) -> bool {
    value
        .and_then(OsStr::to_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn os_contains_ignore_ascii_case(value: Option<&OsStr>, needle: &str) -> bool {
    value
        .and_then(OsStr::to_str)
        .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

fn to_truecolor(color: Color) -> Color {
    match color {
        Color::Reset => Color::Reset,
        Color::Rgb(..) => color,
        _ => color_rgb(color).map_or(Color::Reset, |(r, g, b)| Color::Rgb(r, g, b)),
    }
}

fn to_ansi256(color: Color) -> Color {
    match color {
        Color::Reset => Color::Reset,
        Color::Indexed(..) => color,
        Color::Rgb(r, g, b) => nearest_ansi256(r, g, b),
        _ => named_to_indexed(color),
    }
}

fn to_ansi16(color: Color) -> Color {
    match color {
        Color::Reset => Color::Reset,
        Color::Rgb(r, g, b) => nearest_ansi16(r, g, b),
        Color::Indexed(index) => indexed_to_ansi16(index),
        _ => color,
    }
}

fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index) => Some(indexed_rgb(index)),
        Color::Reset => None,
        _ => named_rgb(color),
    }
}

fn named_to_indexed(color: Color) -> Color {
    let index = match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Reset | Color::Rgb(..) | Color::Indexed(..) => return color,
    };
    Color::Indexed(index)
}

fn indexed_to_ansi16(index: u8) -> Color {
    if let Some(color) = ansi16_color(index) {
        return color;
    }
    let (r, g, b) = indexed_rgb(index);
    nearest_ansi16(r, g, b)
}

fn nearest_ansi256(r: u8, g: u8, b: u8) -> Color {
    let mut best_index = 16;
    let mut best_distance = u32::MAX;
    for index in 0..=u8::MAX {
        let (candidate_r, candidate_g, candidate_b) = indexed_rgb(index);
        let distance = color_distance(r, g, b, candidate_r, candidate_g, candidate_b);
        if distance < best_distance {
            best_distance = distance;
            best_index = index;
        }
    }
    Color::Indexed(best_index)
}

fn nearest_ansi16(r: u8, g: u8, b: u8) -> Color {
    let mut best = Color::Black;
    let mut best_distance = u32::MAX;
    for candidate in ANSI16 {
        let Some((candidate_r, candidate_g, candidate_b)) = named_rgb(candidate) else {
            continue;
        };
        let distance = color_distance(r, g, b, candidate_r, candidate_g, candidate_b);
        if distance < best_distance {
            best_distance = distance;
            best = candidate;
        }
    }
    best
}

fn color_distance(r: u8, g: u8, b: u8, other_r: u8, other_g: u8, other_b: u8) -> u32 {
    let dr = i32::from(r) - i32::from(other_r);
    let dg = i32::from(g) - i32::from(other_g);
    let db = i32::from(b) - i32::from(other_b);
    (dr * dr + dg * dg + db * db) as u32
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    if let Some((r, g, b)) = ansi16_rgb(index) {
        return (r, g, b);
    }
    if index < 232 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let cube = index - 16;
        let r = LEVELS[usize::from(cube / 36)];
        let g = LEVELS[usize::from((cube % 36) / 6)];
        let b = LEVELS[usize::from(cube % 6)];
        return (r, g, b);
    }
    let gray = 8 + (index - 232) * 10;
    (gray, gray, gray)
}

fn ansi16_rgb(index: u8) -> Option<(u8, u8, u8)> {
    let color = ansi16_color(index)?;
    named_rgb(color)
}

fn ansi16_color(index: u8) -> Option<Color> {
    ANSI16.get(usize::from(index)).copied()
}

fn named_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Reset | Color::Rgb(..) | Color::Indexed(..) => None,
    }
}

const ANSI16: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_respects_no_color_and_dumb_terminal() {
        assert_eq!(
            ColorSupport::detect_from(true, Some(OsStr::new("truecolor")), None),
            ColorSupport::NoColor
        );
        assert_eq!(
            ColorSupport::detect_from(false, None, Some(OsStr::new("dumb"))),
            ColorSupport::NoColor
        );
    }

    #[test]
    fn detect_from_prefers_truecolor_then_256_then_16() {
        assert_eq!(
            ColorSupport::detect_from(
                false,
                Some(OsStr::new("24bit")),
                Some(OsStr::new("xterm-256color"))
            ),
            ColorSupport::TrueColor
        );
        assert_eq!(
            ColorSupport::detect_from(false, None, Some(OsStr::new("xterm-256color"))),
            ColorSupport::Ansi256
        );
        assert_eq!(
            ColorSupport::detect_from(false, None, Some(OsStr::new("xterm"))),
            ColorSupport::Ansi16
        );
    }

    #[test]
    fn quantize_truecolor_to_ansi256_never_keeps_rgb() {
        for color in [
            Color::Rgb(230, 233, 239),
            Color::Rgb(191, 97, 106),
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
        ] {
            assert!(matches!(
                quantize(color, ColorSupport::Ansi256),
                Color::Indexed(_)
            ));
        }
    }

    #[test]
    fn quantize_truecolor_to_ansi16_never_keeps_rgb_or_indexed() {
        for color in [
            Color::Rgb(230, 233, 239),
            Color::Rgb(191, 97, 106),
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
        ] {
            let quantized = quantize(color, ColorSupport::Ansi16);
            assert!(named_rgb(quantized).is_some(), "{quantized:?}");
        }
    }

    #[test]
    fn no_color_strips_foreground_and_background() {
        for color in [
            Color::Rgb(1, 2, 3),
            Color::Indexed(200),
            Color::Cyan,
            Color::Reset,
        ] {
            assert_eq!(quantize(color, ColorSupport::NoColor), Color::Reset);
        }
    }

    #[test]
    fn quantization_is_idempotent_for_every_supported_palette() {
        let colors = [
            Color::Rgb(11, 13, 16),
            Color::Rgb(230, 233, 239),
            Color::Rgb(191, 97, 106),
            Color::Indexed(0),
            Color::Indexed(255),
            Color::Cyan,
        ];
        for support in [
            ColorSupport::TrueColor,
            ColorSupport::Ansi256,
            ColorSupport::Ansi16,
            ColorSupport::NoColor,
        ] {
            for color in colors {
                let once = quantize(color, support);
                let twice = quantize(once, support);
                assert_eq!(once, twice, "{support:?} {color:?}");
            }
        }
    }

    #[test]
    fn every_indexed_color_quantizes_to_ansi16_without_rgb_leak() {
        for index in 0..=u8::MAX {
            let color = quantize(Color::Indexed(index), ColorSupport::Ansi16);
            assert!(
                matches!(color, Color::Reset) || named_rgb(color).is_some(),
                "index {index} leaked {color:?}"
            );
        }
    }
}
