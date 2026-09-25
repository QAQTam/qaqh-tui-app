//! Agent 动画的确定性帧映射。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前动画帧号（200ms/帧，与 Tick 周期一致）。
pub(crate) fn frame_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 / 200)
        .unwrap_or(0)
}

/// Claude 风格六帧星芒（thinking 行专用）。
pub(crate) fn claude_spinner_glyph(frame: u64) -> &'static str {
    const FRAMES: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
    FRAMES[(frame % FRAMES.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::claude_spinner_glyph;

    #[test]
    fn claude_spinner_cycles_six_frames() {
        let glyphs: Vec<&str> = (0..6).map(claude_spinner_glyph).collect();
        assert_eq!(glyphs, ["·", "✢", "✳", "✶", "✻", "✽"]);
        assert_eq!(claude_spinner_glyph(6), claude_spinner_glyph(0));
    }
}
