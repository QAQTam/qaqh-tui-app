//! 廉价估算（plan §3.2，锁 7）：离屏块退化为估算高度。
//!
//! 刻意**保守偏小**（宁少不多）：估小 → 视口覆盖的段集合偏大 → 多渲几段无害；
//! 估大 → 窗口内出现未渲染段，破坏 `window` 的不变式。与旧 `estimate_turn_lines`
//! 同一取向（工具块按折叠态 3 行估——刚展开的块必然刚进视口）。
//!
//! 不建字符串、不跑 markdown/syntect/normalize：O(字符显示宽度)。

use unicode_width::UnicodeWidthStr;

use crate::app::timeline_model::{Block, Turn};
use qaqh_client::{TimelineBlockKind, TimelineFailure};

/// 按显示宽度推换行行数（与 `wrap_text` 的贪心折行在「行数」层一致）。
pub(crate) fn estimate_wrapped_lines(text: &str, width: usize) -> usize {
    let w = width.max(1);
    let mut total = 0usize;
    for para in text.split('\n') {
        // 空段落仍占 1 行（与 wrap_text 一致）。
        let pw = UnicodeWidthStr::width(para);
        total += pw.div_ceil(w).max(1);
    }
    total
}

/// 单内容块的估算行数。
pub(crate) fn estimate_block_lines(block: &Block, width: usize) -> usize {
    let w = width.max(1);
    match block.kind {
        TimelineBlockKind::Text | TimelineBlockKind::Reasoning => {
            estimate_wrapped_lines(&block.text, w)
        }
        TimelineBlockKind::Notice => estimate_wrapped_lines(&block.text, w.saturating_sub(2)),
        // §4.7：T1 工具恒单行；T2 折叠卡仍按 3 行估（阶段 2 精确化）。
        TimelineBlockKind::Tool => {
            if block
                .tool
                .as_ref()
                .is_some_and(|t| crate::app::render_transcript::is_t1_tool(&t.name))
            {
                1
            } else {
                3
            }
        }
    }
}

/// 前置装饰（回合头 + offload 提示 + 用户输入）的估算行数。
pub(crate) fn estimate_pre_lines(turn: &Turn, width: usize) -> usize {
    let w = width.max(1);
    let mut lines = 1usize; // 回合头
    if turn.offloaded {
        lines += 1;
    }
    if !turn.user_text.is_empty() {
        lines += estimate_wrapped_lines(&turn.user_text, w.saturating_sub(2));
    }
    lines
}

/// 后置装饰（失败详情 + 尾部空行）的估算行数。
pub(crate) fn estimate_post_lines(failure: Option<&TimelineFailure>, width: usize) -> usize {
    let w = width.max(1);
    let mut lines = 1usize; // 尾部空行
    if let Some(f) = failure {
        lines += estimate_wrapped_lines(&format!("{}: {}", f.code, f.message), w.saturating_sub(4));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::Block;
    use qaqh_client::TimelineBlockState;

    fn text_block(text: &str) -> Block {
        Block {
            block_id: "b".into(),
            block_order: 0,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Sealed,
            text: text.into(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        }
    }

    #[test]
    fn estimates_are_conservative_vs_wrap_text() {
        // 估算行数必须 == wrap_text 实际行数（同源语义），且不得更少到缺行。
        let long_cjk = "一".repeat(200);
        for text in ["短", "hello world foo", long_cjk.as_str(), "a\nb"] {
            for width in [20usize, 40, 80] {
                let est = estimate_block_lines(&text_block(text), width);
                let real = crate::app::render_line::wrap_text(text, width).len();
                assert!(
                    est >= real.saturating_sub(1) && est <= real + 1,
                    "text={text:?} w={width}: est={est} real={real}"
                );
            }
        }
    }
}
