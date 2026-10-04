//! Agent 动画的确定性帧映射。

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前动画帧号（200ms/帧，与 Tick 周期一致）。
pub(crate) fn frame_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 / 200)
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
/// Claude 的推进节奏是 120ms/帧；本仓的 Tick 是 200ms（`TICK_INTERVAL`），
/// 按 120ms 走会在两次重绘之间跳帧，所以保持与 Tick 对齐，一个呼吸周期 2.4s。
pub(crate) fn claude_spinner_glyph(frame: u64) -> &'static str {
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
    use super::{base_frames, claude_spinner_glyph};

    #[test]
    fn claude_spinner_ping_pongs_forward_then_backward() {
        let frames = base_frames();
        let len = frames.len() as u64;

        let forward: Vec<&str> = (0..len).map(claude_spinner_glyph).collect();
        assert_eq!(forward, frames);

        let mut expected = frames.to_vec();
        expected.reverse();
        let backward: Vec<&str> = (len..2 * len).map(claude_spinner_glyph).collect();
        assert_eq!(backward, expected, "后半程必须倒放");

        // 周期封闭：走满 12 帧回到第一帧。
        assert_eq!(claude_spinner_glyph(2 * len), claude_spinner_glyph(0));
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
