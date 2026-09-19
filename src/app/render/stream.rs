//! 流式尾部增量（plan §3.4 / T7）：Open text 块跨 delta 只重折尾行。
//!
//! - **单源**：折行走 [`WrapState`]（wrap_text 同一内核），增量与一次性折行
//!   的逐行等价是结构性的（回归锁 `wrap_stream_incremental_matches_one_shot`；
//!   端到端推论 = 锁 6 `streamed_block_equals_one_shot`）；
//! - **共享**：已封口前缀以 `Arc<[RenderLine]>` 跨 delta 零拷贝，仅折行事件
//!   重建切片；尾行（1–2 行）每 delta 重建；
//! - **光标**：canonical 状态机只吃正文，▌ 由 [`WrapState::cursor_view`]
//!   按同一规则虚拟喂入——`wrap_text(text + "▌")` ≡ sealed 前缀 ++ ▌ 尾部
//!   （若把 ▌ 喂进 canonical 状态机再回退，硬断行后会与纯文本状态分叉，
//!   实测 case：`"abc d"` @ width 3）；Slots 出带时 ▌ 就地换同宽占位；
//! - **seal**：整块重渲一次（此时才上 markdown/syntect），stream 弃；
//! - **淘汰**：evict 丢 stream（内存 O(1) 纪律），重进视口整块重渲一次。

use std::sync::Arc;

use qaqh_client::{TimelineBlockKind, TimelineBlockState};
use unicode_width::UnicodeWidthStr;

use crate::app::render_line::{RenderLine, WrapState};
use crate::app::timeline_model::Block;

use super::seg::{AnimKind, AnimSlot, BlockBody, BlockSeg};

/// Open text 块的跨 delta 折行状态（随块缓存驻留；seal / 淘汰即弃）。
#[derive(Debug, Clone)]
pub(crate) struct StreamState {
    wrap: WrapState,
    /// 已封口行的渲染体（长度恒等于 wrap.sealed_len()；仅折行事件重建）。
    prefix: Arc<[RenderLine]>,
    /// 已消费的正文**字节**数（append-only 契约的 O(1) 防御锚点；
    /// 字节而非字符：增量切片 `&text[consumed..]` 无需逐字符 skip）。
    consumed_bytes: usize,
}

impl StreamState {
    /// 全量首折（首次渲染 / 不可续算后的重入）。
    fn new(text: &str, width: usize) -> Self {
        let mut wrap = WrapState::new(width);
        wrap.push_str(text);
        let mut st = Self {
            wrap,
            prefix: Arc::from([]),
            consumed_bytes: text.len(),
        };
        st.sync_prefix();
        st
    }

    /// prefix ← wrap.sealed 全量重建（仅折行事件调用，摊还 O(1)/行）。
    fn sync_prefix(&mut self) {
        self.prefix = self
            .wrap
            .sealed_slice()
            .iter()
            .map(|s| RenderLine::plain(s.as_str()))
            .collect();
    }

    /// 增量喂入（delta = text[consumed_bytes..]，append-only 契约）。
    /// 返回 false = 不可续算（宽度变 / 字节回退 / 非字符边界），调用方整块重渲。
    fn feed(&mut self, text: &str, width: usize) -> bool {
        if width != self.wrap.width() || text.len() < self.consumed_bytes {
            return false;
        }
        if !text.is_char_boundary(self.consumed_bytes) {
            return false;
        }
        let delta = &text[self.consumed_bytes..];
        if !delta.is_empty() {
            self.wrap.push_str(delta);
            self.consumed_bytes = text.len();
            if self.prefix.len() != self.wrap.sealed_len() {
                self.sync_prefix();
            }
        }
        true
    }

    /// 由当前状态推导 Streaming 体 + 光标槽位（Slots 出带，锁 8 占位纪律）。
    fn body_and_anim(&self) -> (BlockBody, Vec<AnimSlot>) {
        let (overflow, cur) = self.wrap.cursor_view();
        let mut tail = Vec::with_capacity(2);
        if let Some(head) = overflow {
            tail.push(RenderLine::plain(head));
        }
        // ▌ → 同宽占位 + 槽位（与 push_text_block 的哨兵纪律一致）。
        let mut cur = cur;
        let col = cur.width() - 1; // ▌ 宽 1，位于行尾
        cur.pop();
        cur.push(' ');
        let row = (self.prefix.len() + tail.len()) as u16;
        tail.push(RenderLine::plain(cur));
        (
            BlockBody::Streaming {
                prefix: Arc::clone(&self.prefix),
                tail,
            },
            vec![AnimSlot {
                row,
                col: col as u16,
                kind: AnimKind::Cursor,
            }],
        )
    }
}

/// 首次渲染（阶段 2）：从全文初始化并落块。
pub(crate) fn init(bseg: &mut BlockSeg, block: &Block, width: u16) {
    let st = StreamState::new(&block.text, usize::from(width));
    let (body, anim) = st.body_and_anim();
    bseg.body = body;
    bseg.anim = anim;
    bseg.stream = Some(st);
}

/// 结构对齐期的增量携带：seg 的 stream 可续算 → **就地**更新 body/anim
/// （stream 留在 seg 内，零 move——take/回填式接口在 bench 首轮就踩了坑）。
/// 返回 false = 不可续算（调用方整块重渲）。
pub(crate) fn carry(seg: &mut BlockSeg, block: &Block, width: u16) -> bool {
    if block.kind != TimelineBlockKind::Text || block.state != TimelineBlockState::Open {
        return false; // seal → 整块重渲一次（§3.4）
    }
    let Some(st) = seg.stream.as_mut() else {
        return false;
    };
    if !st.feed(&block.text, usize::from(width)) {
        return false;
    }
    let (body, anim) = st.body_and_anim();
    seg.body = body;
    seg.anim = anim;
    true
}
