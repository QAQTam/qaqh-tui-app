//! 粘贴护栏：识别"按键洪流"并抑制 Enter 自动发送。
//!
//! 背景：crossterm 的 Windows 事件源不解析括号粘贴标记（unix 路径才有
//! `parse_csi_bracketed_paste`），在不支持的终端里粘贴多行文本会以按键
//! 事件流到达，其中的回车逐个触发发送。人手打字不可能在窗口期内达到
//! 洪流阈值，因此用节拍启发式区分"打字"与"粘贴"；支持括号粘贴的终端
//! 走 `Event::Paste` 路径，完全不经过此护栏。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 判定窗口：仅统计最近该时长内的按键。
pub(crate) const PASTE_BURST_WINDOW: Duration = Duration::from_millis(200);
/// 洪流阈值：窗口内按键数达到该值即判定为粘贴流（人手极限约 25 键/s）。
pub(crate) const PASTE_BURST_KEYS: usize = 40;

#[derive(Debug, Default)]
pub(crate) struct PasteGuard {
    taps: VecDeque<Instant>,
    /// 当前洪流片段是否已提示过（避免 toast 刷屏）。
    toasted: bool,
}

impl PasteGuard {
    /// 记录一次按键；先淘汰窗口外的旧记录。
    pub(crate) fn observe(&mut self, now: Instant) {
        while let Some(t) = self.taps.front() {
            if now.duration_since(*t) > PASTE_BURST_WINDOW {
                self.taps.pop_front();
            } else {
                break;
            }
        }
        self.taps.push_back(now);
        if self.taps.len() < PASTE_BURST_KEYS {
            self.toasted = false;
        }
    }

    /// 是否处于粘贴洪流（在 `observe` 之后调用）。
    pub(crate) fn flooding(&self) -> bool {
        self.taps.len() >= PASTE_BURST_KEYS
    }

    /// 洪流期首次命中时返回 true（用于只提示一次）。
    pub(crate) fn take_toast(&mut self) -> bool {
        if self.flooding() && !self.toasted {
            self.toasted = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对测试基准：从 now 起偏移 ms 毫秒的时间点。
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn human_typing_never_floods() {
        let mut g = PasteGuard::default();
        let base = Instant::now();
        // 80ms/键 ≈ 12.5 键/s，远低于人手极限之上界。
        for i in 0..200 {
            g.observe(at(base, i * 80));
            assert!(!g.flooding());
        }
    }

    #[test]
    fn burst_crosses_threshold_at_40() {
        let mut g = PasteGuard::default();
        let base = Instant::now();
        for i in 0..PASTE_BURST_KEYS - 1 {
            g.observe(at(base, i as u64));
        }
        assert!(!g.flooding());
        g.observe(at(base, PASTE_BURST_KEYS as u64));
        assert!(g.flooding());
    }

    #[test]
    fn flood_toast_fires_once_per_episode() {
        let mut g = PasteGuard::default();
        let base = Instant::now();
        for i in 0..=PASTE_BURST_KEYS {
            g.observe(at(base, i as u64));
        }
        assert!(g.take_toast());
        assert!(!g.take_toast());
        assert!(!g.take_toast());
    }

    #[test]
    fn window_expiry_resets_guard() {
        let mut g = PasteGuard::default();
        let base = Instant::now();
        for i in 0..=PASTE_BURST_KEYS {
            g.observe(at(base, i as u64));
        }
        assert!(g.flooding());
        // 洪流停止超过窗口时长后恢复。
        g.observe(at(base, PASTE_BURST_KEYS as u64 + 250));
        assert!(!g.flooding());
        // 再次洪流可重新提示。
        for i in 0..=PASTE_BURST_KEYS {
            g.observe(at(base, 1_000 + i as u64));
        }
        assert!(g.take_toast());
    }
}
