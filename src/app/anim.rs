//! Agent 动画的确定性帧映射。

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::style::Color;

/// 动画帧时长（ms）。与 tick 周期**解耦**（B2）：动画相位由 `Instant` 差值
/// 驱动、按本时长量化；tick 只负责醒来的频率。动画需要更快的步进时改这里即可，
/// 不再把两者焊死在同一个常量上。
///
/// 120ms 对齐 Claude 原版的推进节奏；busy tick（60ms，见 terminal/agent 的
/// `TICK_ANIM_INTERVAL`）比它更快，因此不会在两次重绘之间跳帧。一个呼吸周期
/// 12 × 120ms = 1.44s。
pub(crate) const FRAME_MILLIS: u64 = 120;

/// 选中标签底色渐显时长（ms）——opentui Timeline 移植的落点之一：列表选中
/// 切换不做位移（行高恒为 1），做**颜色插值**（surface 底 → selection 底）。
pub(crate) const TAB_FADE_MS: u128 = 120;

// ───────────────────────── 全局动画开关 ─────────────────────────

static ANIMATIONS: OnceLock<bool> = OnceLock::new();

/// 全局动画开关：`QAQH_TUI_ANIM=0|false|off` 关闭（SSH 高延迟 / 老终端）。
///
/// 关闭后：菊花定格、shimmer 静态 dim、滚动/渐显瞬切、入场扫光不播——
/// 「在干活」的信号（颜色 + 文案）全部保留，只去掉运动。
pub(crate) fn enabled() -> bool {
    *ANIMATIONS.get_or_init(|| parse_anim_flag(std::env::var("QAQH_TUI_ANIM").ok().as_deref()))
}

/// 纯函数便于回归：缺省开；`0` / `false` / `off`（大小写不敏感）关。
fn parse_anim_flag(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim),
        Some(v) if v.eq_ignore_ascii_case("0")
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("off")
    )
}

// ───────────────────────── 缓动与混合 ─────────────────────────

/// outQuad：起快收慢。底色/偏移这类短程过渡的默认缓动
/// （对齐 opentui `easingFunctions.outQuad`）。
pub(crate) fn ease_out_quad(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * (2.0 - t)
}

/// 两色线性混合（t=0 → a，t=1 → b）。
///
/// 任一端不是真彩 Rgb 时返回 `None`，调用方退回瞬切——降级纪律与 shimmer
/// 一致：插值的动态对比本来就依赖真彩混合。
pub(crate) fn mix_rgb(a: Color, b: Color, t: f32) -> Option<Color> {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix =
                |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
            Some(Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb)))
        }
        _ => None,
    }
}

/// 当前动画帧号（FRAME_MILLIS/帧）。
pub(crate) fn frame_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 / FRAME_MILLIS)
        .unwrap_or(0)
}

/// Claude 风格星芒的六个基础字形。
///
/// 与 Claude Code `components/Spinner/utils.ts:getDefaultCharacters()` 同源，
/// 连平台差异一起搬过来：
///
/// - `✳`(U+2733) 在 Unicode emoji 集合里（`emoji-test.txt` 里同时有
///   `2733 FE0F ; fully-qualified` 与 `2733 ; unqualified`）。Windows/Linux
///   终端的彩色 emoji 字体会把它抢走，渲染成**宽 2 的彩色字形**——占位从一列
///   变两列，整行跟着错位。非 macOS 因此一律换成 ASCII `*`。
/// - `✽`(U+273D) 不在 emoji 集合里，但 Ghostty 下字形本身偏移，同样换 `*`。
///
/// 同款取舍见 Claude Code `constants/figures.ts`：
/// `BLACK_CIRCLE = darwin ? '⏺' : '●'`，注释写着前者对齐更好但
/// "isn't usually supported on Windows/Linux"。
fn base_frames() -> &'static [&'static str] {
    const MACOS: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
    const NON_MACOS: [&str; 6] = ["·", "✢", "*", "✶", "✻", "✽"];
    const GHOSTTY: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "*"];

    static FRAMES: OnceLock<&'static [&'static str]> = OnceLock::new();
    FRAMES.get_or_init(|| {
        if std::env::var("TERM").as_deref() == Ok("xterm-ghostty") {
            &GHOSTTY
        } else if cfg!(target_os = "macos") {
            &MACOS
        } else {
            &NON_MACOS
        }
    })
}

/// Claude 风格十二帧星芒（thinking 行 / 侧栏 working 行专用）。
///
/// 正放六帧再倒放六帧（`SPINNER_FRAMES = [...D, ...[...D].reverse()]`，见
/// Claude Code `Spinner/SpinnerGlyph.tsx`）。倒放让菊花"开→合"地呼吸，而不是
/// 从最重的 `✽` 硬切回 `·`；峰顶那一帧按 Claude 的写法连出两次。
///
/// Claude 的推进节奏是 120ms/帧；本仓动画相位是 `FRAME_MILLIS`（120ms），
/// busy tick（60ms）快于帧量子，不会跳帧。一个呼吸周期 1.44s。
///
/// 动画总开关关闭时定格在峰顶帧 `✻`：单列宽、非 emoji、和侧栏的
/// 空闲 `·` / `○` 状态字形可区分——「在干活」的信号保留，只是不动。
pub(crate) fn claude_spinner_glyph(frame: u64) -> &'static str {
    if !enabled() {
        return base_frames()[4];
    }
    spinner_glyph_at(frame)
}

/// 相位映射本体（无开关，便于测试钉住 ping-pong 周期）。
fn spinner_glyph_at(frame: u64) -> &'static str {
    let frames = base_frames();
    let cycle = frames.len() * 2;
    let index = (frame % cycle as u64) as usize;
    if index < frames.len() {
        frames[index]
    } else {
        frames[cycle - 1 - index]
    }
}

#[cfg(test)]
mod tests {
    use super::{base_frames, ease_out_quad, mix_rgb, parse_anim_flag, spinner_glyph_at};
    use ratatui::style::Color;

    /// 开关解析：缺省开；`0`/`false`/`off` 关；大小写与空白不敏感。
    #[test]
    fn anim_flag_defaults_on_and_parses_off_values() {
        assert!(parse_anim_flag(None));
        assert!(parse_anim_flag(Some("1")));
        assert!(parse_anim_flag(Some(" true ")));
        assert!(!parse_anim_flag(Some("0")));
        assert!(!parse_anim_flag(Some("False")));
        assert!(!parse_anim_flag(Some(" off ")));
    }

    /// 相位映射（绕过开关）依旧 ping-pong 封闭。
    #[test]
    fn spinner_phase_ping_pongs_forward_then_backward() {
        let frames = base_frames();
        let len = frames.len() as u64;

        let forward: Vec<&str> = (0..len).map(spinner_glyph_at).collect();
        assert_eq!(forward, frames);

        let mut expected = frames.to_vec();
        expected.reverse();
        let backward: Vec<&str> = (len..2 * len).map(spinner_glyph_at).collect();
        assert_eq!(backward, expected, "后半程必须倒放");

        // 周期封闭：走满 12 帧回到第一帧。
        assert_eq!(spinner_glyph_at(2 * len), spinner_glyph_at(0));
    }

    /// outQuad 端点精确、整体单调、前半程偏快（"起快收慢"）。
    #[test]
    fn ease_out_quad_is_monotonic_with_fast_start() {
        assert_eq!(ease_out_quad(0.0), 0.0);
        assert_eq!(ease_out_quad(1.0), 1.0);
        assert!(ease_out_quad(0.5) > 0.5, "outQuad 前半程必须偏快");
        let mut previous = -1.0_f32;
        for step in 0..=10 {
            let t = ease_out_quad(step as f32 / 10.0);
            assert!(t >= previous, "缓动必须单调：step {step}");
            previous = t;
        }
    }

    /// 混合在端点精确返回两端色；非真彩端降级为 None（调用方瞬切）。
    #[test]
    fn mix_rgb_blends_endpoints_and_degrades_without_truecolor() {
        let black = Color::Rgb(0, 0, 0);
        let accent = Color::Rgb(100, 50, 250);
        assert_eq!(mix_rgb(black, accent, 0.0), Some(black));
        assert_eq!(mix_rgb(black, accent, 1.0), Some(accent));
        assert_eq!(mix_rgb(black, accent, 0.5), Some(Color::Rgb(50, 25, 125)));
        assert_eq!(mix_rgb(Color::Reset, accent, 0.5), None);
        assert_eq!(mix_rgb(black, Color::Indexed(4), 0.5), None);
        // t 越界被钳制，不越色。
        assert_eq!(mix_rgb(black, accent, 2.0), Some(accent));
    }

    /// 非 macOS 的兜底集不得含 emoji 字形：`✳`(U+2733) 会被彩色 emoji 字体抢走，
    /// 渲染成宽 2 的彩色字形，撑坏 `" {glyph} "` 的一列占位。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_spinner_avoids_emoji_glyph() {
        assert!(
            !base_frames().contains(&"✳"),
            "U+2733 在 emoji-test.txt 里，非 macOS 必须换成 ASCII"
        );
    }

    #[test]
    fn spinner_frames_stay_one_column_wide() {
        for glyph in base_frames() {
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(*glyph),
                1,
                "{glyph:?} 必须只占一列"
            );
        }
    }
}
