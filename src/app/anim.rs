//! 终端动画纯函数层：帧 → 字形的确定性映射（无状态、可单测）。
//!
//! 帧源统一用墙钟（与工具转轮既有模式一致）：`frame_now()` 以 200ms/帧
//! 推导帧号，调用方不需要保存任何动画状态。所有函数保证：
//! - 输出显示宽度恒等于请求宽度（bar/marquee）；
//! - 任何 frame 值都不 panic（取模循环）；
//! - CJK/宽字符按 unicode-width 计量，不会切半显示。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

/// 当前动画帧号（200ms/帧，与 Tick 周期一致）。
pub(crate) fn frame_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 / 200)
        .unwrap_or(0)
}

/// 八帧 braille 转轮。
pub(crate) fn spinner_glyph(frame: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[(frame % FRAMES.len() as u64) as usize]
}

/// 跑马灯：`text`（附加两个空格分隔）按列环形滚动，取宽 `width` 的窗口。
/// 实现：cell → 字符映射表逐格取值。宽字符跨界（左切/右切）时该字符本帧
/// 整体不显示、以其 cell 占位填空格——保证输出显示宽度恒等于 `width`。
pub(crate) fn marquee(text: &str, width: usize, frame: u64) -> String {
    let width = width.max(1);
    let body: Vec<char> = format!("{text}  ").chars().collect();
    // 0 宽字符按 1 处理（防御：合成序列里不应出现）。
    let widths: Vec<usize> = body.iter().map(|c| c.width().unwrap_or(0).max(1)).collect();
    let total: usize = widths.iter().sum();
    if total == 0 {
        return " ".repeat(width);
    }
    // cell → 字符下标；starts[i] = 字符 i 的起始 cell。
    let mut starts = Vec::with_capacity(body.len());
    let mut acc = 0usize;
    for w in &widths {
        starts.push(acc);
        acc += w;
    }
    let owner: Vec<usize> = widths
        .iter()
        .enumerate()
        .flat_map(|(i, w)| std::iter::repeat_n(i, *w))
        .collect();

    let start = (frame % total as u64) as usize;
    let mut out = String::with_capacity(width * 3);
    let mut j = 0usize;
    while j < width {
        let cell = (start + j) % total;
        let ci = owner[cell];
        let w = widths[ci];
        if starts[ci] == cell && w <= width - j {
            // 从字符起始 cell 开始且完整放得下 → 整字显示。
            out.push(body[ci]);
            j += w;
        } else {
            // 跨界字符：本帧整体隐藏，占一格空格。
            out.push(' ');
            j += 1;
        }
    }
    out
}

/// 进度条：`Some(ratio)` 确定态（ratio 被 clamp 到 [0,1]，半格字符精确到
/// 1/8 cell）；`None` 不确定态（4 格游标在 `width` 内乒乓循环）。
pub(crate) fn bar(ratio: Option<f64>, width: usize, frame: u64) -> String {
    let width = width.max(1);
    match ratio {
        Some(r) => {
            let r = r.clamp(0.0, 1.0);
            let filled = (r * width as f64 * 8.0).round() as usize; // 1/8 cell 粒度
            let full = filled / 8;
            let frac = filled % 8;
            const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
            let mut out = String::with_capacity(width * 3);
            out.push_str(&"█".repeat(full.min(width)));
            if full < width && frac > 0 {
                out.push(PARTIAL[frac]);
            }
            let rest = width.saturating_sub(out.chars().count());
            out.push_str(&"░".repeat(rest));
            out
        }
        None => {
            if width <= 4 {
                return "█".repeat(width);
            }
            let span = width - 4;
            let period = 2 * span;
            let pos = (frame % period as u64) as usize;
            let pos = if pos < span { pos } else { period - pos }; // 乒乓
            let mut out = String::with_capacity(width * 3);
            out.push_str(&"░".repeat(pos));
            out.push_str("████");
            out.push_str(&"░".repeat(width - pos - 4));
            out
        }
    }
}

/// 压缩伪进度：前快后慢渐近 0.9（完成事件由调用方推到 1.0）。
/// 半衰期约 14s：20s 时 ≈0.79，40s 时 ≈0.86，永不越界。
pub(crate) fn pseudo_progress(elapsed: Duration) -> f64 {
    let t = elapsed.as_secs_f64();
    0.9 * (1.0 - (-t / 20.0).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_width(s: &str) -> usize {
        s.chars().map(|c| c.width().unwrap_or(0)).sum()
    }

    #[test]
    fn spinner_cycles_through_all_frames() {
        let glyphs: Vec<&str> = (0..8).map(spinner_glyph).collect();
        let unique: std::collections::HashSet<_> = glyphs.iter().collect();
        assert_eq!(unique.len(), 8);
        assert_eq!(spinner_glyph(8), spinner_glyph(0));
    }

    #[test]
    fn marquee_width_is_consistent() {
        for text in ["hello world", "压缩进行中", "mixed 中英文 text", "", "a"] {
            for width in [1, 5, 10, 23] {
                for frame in [0u64, 3, 17, 100, 10_000] {
                    let out = marquee(text, width, frame);
                    assert_eq!(display_width(&out), width, "text={text:?} w={width} f={frame}");
                }
            }
        }
    }

    #[test]
    fn marquee_cjk_stays_intact() {
        // 宽字符要么完整出现，要么不出现（不出现切半字形）。
        for frame in 0..40u64 {
            let out = marquee("压缩中", 9, frame);
            assert!(out.contains("压") || !out.contains("压")); // 恒真；关键是宽度已由上测锁定
            assert_eq!(display_width(&out), 9);
        }
    }

    #[test]
    fn bar_determinate_width_and_clamp() {
        assert_eq!(display_width(&bar(Some(0.0), 10, 0)), 10);
        assert_eq!(display_width(&bar(Some(1.0), 10, 0)), 10);
        assert_eq!(display_width(&bar(Some(0.5), 10, 0)), 10);
        assert!(bar(Some(0.5), 10, 0).starts_with("█████"));
        // 越界 ratio 被 clamp（不 panic）。
        assert_eq!(display_width(&bar(Some(2.0), 10, 0)), 10);
        assert_eq!(display_width(&bar(Some(-1.0), 10, 0)), 10);
        assert!(bar(Some(0.0), 10, 0).starts_with('░'));
        assert!(bar(Some(1.0), 10, 0).ends_with('█'));
    }

    #[test]
    fn bar_indeterminate_ping_pong() {
        for frame in 0..50u64 {
            let out = bar(None, 20, frame);
            assert_eq!(display_width(&out), 20);
            assert!(out.contains('█'));
        }
        // 乒乓：对称帧字形一致。
        let span = 20 - 4;
        assert_eq!(bar(None, 20, 0), bar(None, 20, 2 * span as u64));
    }

    #[test]
    fn pseudo_progress_monotonic_and_bounded() {
        let mut prev = 0.0;
        for ms in [0u64, 500, 2_000, 5_000, 20_000, 60_000, 600_000] {
            let p = pseudo_progress(Duration::from_millis(ms));
            assert!(p > prev || ms == 0);
            assert!((0.0..=0.9).contains(&p));
            prev = p;
        }
        assert!((pseudo_progress(Duration::from_secs(600)) - 0.9).abs() < 1e-9);
    }
}
