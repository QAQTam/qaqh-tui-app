//! 块级渲染缓存：类型与键（plan §3.2，M1 骨架）。
//!
//! 粒度从「回合」降为「块」：
//! - [`TurnSeg::struct_key`] 是回合的**结构身份**（回合级字段 + 块 id/顺序），
//!   **不含**块 `rev`——否则任一 delta 都会拖累整回合复用；
//! - [`BlockSeg::key`] 是块的渲染身份，**显式含 `block_state`**（Open→Sealed
//!   切换渲染路径，reviewer §四.5）、宽度与工具展开态，**不含帧号**
//!   （动画出带 [`AnimSlot`]：Slots 出口行内只有占位空格，字形 draw 期覆盖；
//!   Bake 出口仅剩 oracle `render_transcript_with_opts`（锁 1 口径）与测试
//!   直呼入口使用——锁 8）。
//!
//! M1 骨架期全部块驻留（虚拟化/估算在后续增量，锁 7）。

#![allow(dead_code)] // M1 骨架：生产接线（ensure_render_caches 切换）在后续增量。

use std::sync::Arc;

use crate::app::render_line::RenderLine;
use crate::app::render_transcript::{FNV_OFFSET, h_str, h_u64};
use crate::app::timeline_model::{Block, Turn};
use qaqh_client::{
    TimelineBlockKind, TimelineBlockState, TimelineFailure, TimelineToolState, TimelineTurnState,
};

/// 视口外仍驻留的回合余量（与旧 `session::KEEP_MARGIN_SEGMENTS` 同语义）。
pub(crate) const KEEP_MARGIN_TURNS: usize = crate::app::session::KEEP_MARGIN_SEGMENTS;

/// 动画字形种类（draw 期按当前帧取字形；Slots 出口的缓存行内只有占位）。
/// 字形一律宽 1 = 占位 cell 宽 1（plan §3.3：几何与烘焙模式逐 cell 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimKind {
    /// 运行中工具卡的 braille 转轮。
    Spinner,
    /// 流式尾部光标 `▌`。
    Cursor,
    /// 思考流式头标四帧圆环（◐◑◒◓；M2 思考退出 transcript 后随之删除）。
    Thinking,
    /// 不确定态进度条（乒乓）。**暂无生产者**：info 行的进度条每帧独立渲染
    /// 不入缓存，且 10 格多 cell 区域无法用单 cell 槽位表达——M2 进度条
    /// 重设计时再定槽位形态；draw 期对它跳过（不覆盖）。
    ProgressIndeterminate,
}

/// 视口相对坐标的动画槽位（[`TranscriptCache::visible_slots`] 产出，draw 直用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportSlot {
    pub row: u16,
    pub col: u16,
    pub kind: AnimKind,
}

/// 段内动画槽位：`(row, col)` 为块内行/列坐标，draw 期平移到视口并裁剪。
///
/// 占位纪律（reviewer §四.2）：`col` 所在 cell 必须是折行/渲染阶段**预留的
/// 空格**——字形本身宽 1 不代表被覆盖的 cell 安全，压着 CJK 半边的单 cell
/// 覆盖会把宽字符切裂。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimSlot {
    pub row: u16,
    pub col: u16,
    pub kind: AnimKind,
}

/// 块渲染体：精确行（视口附近驻留）、流式增量体（T7）或估算高度（离屏退化）。
#[derive(Debug, Clone)]
pub enum BlockBody {
    Lines(Arc<[RenderLine]>),
    /// 流式增量体（T7 / plan §3.4）：prefix 行跨 delta 零拷贝共享（仅折行
    /// 事件重建切片），tail 1–2 行（▌ 折出至多 1 行封口 + 当前行）每 delta
    /// 重建；seal 时整块转 `Lines`（此时才上 markdown/syntect）。
    Streaming {
        prefix: Arc<[RenderLine]>,
        tail: Vec<RenderLine>,
    },
    Height(usize),
}

impl BlockBody {
    pub fn height(&self) -> usize {
        match self {
            BlockBody::Lines(l) => l.len(),
            BlockBody::Streaming { prefix, tail } => prefix.len() + tail.len(),
            BlockBody::Height(n) => *n,
        }
    }

    /// 全量行切片（仅 `Lines`；流式体的逐行访问走 [`BlockBody::line`]）。
    pub fn lines(&self) -> Option<&Arc<[RenderLine]>> {
        match self {
            BlockBody::Lines(l) => Some(l),
            _ => None,
        }
    }

    /// 是否持有精确渲染结果（Lines / Streaming 均算；Height = 未渲染）。
    pub fn is_rendered(&self) -> bool {
        !matches!(self, BlockBody::Height(_))
    }

    /// 第 i 行（i < height()）；未渲染返回 None。
    pub fn line(&self, i: usize) -> Option<&RenderLine> {
        match self {
            BlockBody::Lines(l) => l.get(i),
            BlockBody::Streaming { prefix, tail } => prefix
                .get(i)
                .or_else(|| tail.get(i.saturating_sub(prefix.len()))),
            BlockBody::Height(_) => None,
        }
    }

    /// 依序把已渲染行推进 out（flatten 用）。
    pub fn push_rendered_lines<'a>(&'a self, out: &mut Vec<&'a RenderLine>) {
        if !self.is_rendered() {
            return;
        }
        for i in 0..self.height() {
            if let Some(l) = self.line(i) {
                out.push(l);
            }
        }
    }
}

/// 一个块的缓存项：内容键 + 渲染体 + 动画槽位 + 流式增量状态。
#[derive(Debug, Clone)]
pub struct BlockSeg {
    pub key: u64,
    pub body: BlockBody,
    pub anim: Vec<AnimSlot>,
    /// 流式折行状态（T7）：仅 Open text 块持有；seal / 淘汰即弃（§3.4）。
    pub stream: Option<super::stream::StreamState>,
}

/// 一个回合的缓存项：前置装饰 + 内容块 + 后置装饰。
///
/// `pre`（回合头/offload 提示/用户输入）与 `post`（失败详情/尾部空行）是
/// 装饰块，与内容块同样参与键控增量；三者按序拼接必须与旧整回合渲染
/// **逐行一致**（lock `block_cache_matches_full_render`）。
#[derive(Debug, Clone)]
pub struct TurnSeg {
    pub turn_id: String,
    /// 结构身份：回合级字段 + 块 id/顺序，**不含**块 rev。
    pub struct_key: u64,
    pub pre: BlockSeg,
    pub content: Vec<BlockSeg>,
    /// 与 `content` 同序的 block_id（struct_key 变化时按 id 部分复用）。
    pub content_ids: Vec<String>,
    pub post: BlockSeg,
}

impl TurnSeg {
    /// 回合总行数（估算与精确同价计入——滚动几何不需要渲染）。
    pub fn height(&self) -> usize {
        self.pre.body.height()
            + self.content.iter().map(|b| b.body.height()).sum::<usize>()
            + self.post.body.height()
    }
}

/// 块级渲染缓存（取代回合粒度的 `SegmentCache`）。
#[derive(Debug, Clone)]
pub struct TranscriptCache {
    pub width: u16,
    pub banner: Option<BlockSeg>,
    pub empty: Option<BlockSeg>,
    pub turns: Vec<TurnSeg>,
    pub stats: RenderStats,
}

/// 一次 refresh 的观测（M0 Stats 的块级后继；debug 段展示用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderStats {
    pub rebuilt_blocks: usize,
    pub resident_blocks: usize,
    /// 本次 refresh 耗时（µs；M0 `render_us` 口径的块级后继）。
    pub render_us: u64,
}

impl TranscriptCache {
    pub fn new(width: u16) -> Self {
        Self {
            width,
            banner: None,
            empty: None,
            turns: Vec::new(),
            stats: RenderStats::default(),
        }
    }

    /// 总行数（视窗定位/滚动条需要；估算高度同价计入）。
    pub fn total_lines(&self) -> usize {
        self.banner.as_ref().map_or(0, |b| b.body.height())
            + self.turns.iter().map(TurnSeg::height).sum::<usize>()
            + self.empty.as_ref().map_or(0, |e| e.body.height())
    }

    /// 取可见窗口 `[top, top+height)` 的行切片（与旧 `SegmentCache::window`
    /// 同语义同不变式：视口覆盖到的段必须已持有 Lines）。
    pub fn window(&self, top: usize, height: usize) -> Vec<&RenderLine> {
        let mut out: Vec<&RenderLine> = Vec::with_capacity(height);
        let mut skip = top;
        for seg in self
            .banner
            .iter()
            .chain(self.turns.iter().flat_map(|t| {
                std::iter::once(&t.pre)
                    .chain(t.content.iter())
                    .chain(std::iter::once(&t.post))
            }))
            .chain(self.empty.iter())
        {
            if out.len() >= height {
                break;
            }
            let h = seg.body.height();
            if !seg.body.is_rendered() {
                if skip >= h {
                    skip -= h;
                    continue;
                }
                debug_assert!(false, "窗口内出现未渲染的估算块（虚拟化不变式被破坏）");
                skip = 0;
                continue;
            }
            if skip >= h {
                skip -= h;
                continue;
            }
            let start = skip;
            skip = 0;
            let take = (height - out.len()).min(h - start);
            for i in start..start + take {
                if let Some(l) = seg.body.line(i) {
                    out.push(l);
                }
            }
        }
        out
    }

    /// 视口 `[top, top+height)` 内的动画槽位（行坐标平移为视口相对）。
    ///
    /// 与 [`TranscriptCache::window`] 同一几何口径；只对持有 Lines 的块产出
    /// （淘汰态不可见，槽位也已随淘汰清空）；`col ≥ 缓存宽` 与视口外的
    /// 槽位一律过滤——半露槽位不画（plan §3.3 平移裁剪）。
    pub fn visible_slots(&self, top: usize, height: usize) -> Vec<ViewportSlot> {
        let bottom = top.saturating_add(height);
        let mut out = Vec::new();
        let mut start = 0usize;
        for seg in self
            .banner
            .iter()
            .chain(self.turns.iter().flat_map(|t| {
                std::iter::once(&t.pre)
                    .chain(t.content.iter())
                    .chain(std::iter::once(&t.post))
            }))
            .chain(self.empty.iter())
        {
            let h = seg.body.height();
            if start >= bottom {
                break;
            }
            if start + h > top && seg.body.is_rendered() {
                for slot in &seg.anim {
                    debug_assert!(
                        (slot.row as usize) <= h,
                        "槽位行超出块行数（槽位必须随 body 同生同灭）"
                    );
                    let abs = start + slot.row as usize;
                    if abs >= top && abs < bottom && (slot.col as usize) < self.width as usize {
                        out.push(ViewportSlot {
                            row: (abs - top) as u16,
                            col: slot.col,
                            kind: slot.kind,
                        });
                    }
                }
            }
            start += h;
        }
        out
    }

    /// 依序平铺全部行（测试等价性用：与旧全量渲染对照）。
    pub fn flatten_lines(&self) -> Vec<&RenderLine> {
        let mut out = Vec::new();
        if let Some(b) = &self.banner {
            b.body.push_rendered_lines(&mut out);
        }
        for t in &self.turns {
            for seg in std::iter::once(&t.pre)
                .chain(t.content.iter())
                .chain(std::iter::once(&t.post))
            {
                seg.body.push_rendered_lines(&mut out);
            }
        }
        if let Some(e) = &self.empty {
            e.body.push_rendered_lines(&mut out);
        }
        out
    }
}

fn block_kind_tag(k: TimelineBlockKind) -> u64 {
    match k {
        TimelineBlockKind::Text => 0,
        TimelineBlockKind::Reasoning => 1,
        TimelineBlockKind::Tool => 2,
        TimelineBlockKind::Notice => 3,
    }
}

fn block_state_tag(s: TimelineBlockState) -> u64 {
    match s {
        TimelineBlockState::Open => 0,
        TimelineBlockState::Sealed => 1,
    }
}

fn turn_state_tag(s: TimelineTurnState) -> u64 {
    match s {
        TimelineTurnState::Running => 0,
        TimelineTurnState::Completed => 1,
        TimelineTurnState::Failed => 2,
        TimelineTurnState::Cancelled => 3,
    }
}

/// 单块的渲染缓存键：`(block_id, rev, block_state, kind, width, 展开态, 工具名)`。
///
/// 只读 `rev` 计数与**枚举标签**，不哈希正文——O(块) 而非 O(文本)。
/// 工具名承担「ToolView 分派」的角色（M2 引入 `ToolViewKind` 后替换）。
pub(crate) fn block_cache_key(block: &Block, width: u16, expanded: bool) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x626c_6b30); // "blk0" 域分隔
    h_str(&mut h, &block.block_id);
    h_u64(&mut h, block.rev);
    h_u64(&mut h, block_state_tag(block.state));
    h_u64(&mut h, block_kind_tag(block.kind));
    h_u64(&mut h, u64::from(width));
    if let Some(t) = &block.tool {
        h_u64(&mut h, u64::from(expanded));
        h_str(&mut h, &t.name);
    }
    h
}

/// 回合结构键：结构身份变化（块增删/回合级字段）才整体重排块；
/// 内容变化（rev）不进此键——那是块键的职责。
/// 工具终态进键（§4.2 组签名：成员终态变化 → 组行随之更新）。
pub(crate) fn tool_state_tag(s: TimelineToolState) -> u64 {
    match s {
        TimelineToolState::Prepared => 0,
        TimelineToolState::Running => 1,
        TimelineToolState::Succeeded => 2,
        TimelineToolState::Failed => 3,
        TimelineToolState::Cancelled => 4,
        TimelineToolState::Backgrounded => 5,
    }
}

pub(crate) fn turn_struct_key(turn: &Turn, num: u64) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x7473_7430); // "tst0"
    h_str(&mut h, &turn.turn_id);
    h_u64(&mut h, num);
    h_u64(&mut h, turn_state_tag(turn.state));
    h_u64(&mut h, u64::from(turn.sealed));
    h_u64(&mut h, u64::from(turn.offloaded));
    // §4.6：思考聚合是 seal 时的语义变更，纳入结构键（plan 原话）。
    h_u64(&mut h, u64::from(turn.thinking.segments));
    h_u64(&mut h, turn.thinking.lines);
    h_str(&mut h, &turn.user_text);
    if let Some(f) = &turn.failure {
        h_str(&mut h, &f.code);
        h_str(&mut h, &f.message);
    }
    for round in &turn.rounds {
        h_u64(&mut h, u64::from(round.round_num));
        for b in &round.blocks {
            h_str(&mut h, &b.block_id);
            h_u64(&mut h, u64::from(b.block_order));
        }
    }
    h
}

/// 前置装饰（回合头/offload/用户输入）的键。
pub(crate) fn turn_pre_key(turn: &Turn, num: u64, width: u16) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x7072_6530); // "pre0"
    h_u64(&mut h, turn_state_tag(turn.state));
    h_u64(&mut h, u64::from(turn.offloaded));
    h_u64(&mut h, num);
    h_str(&mut h, &turn.user_text);
    // §4.6：聚合在 seal 时写入 → 头行随之变化，必须进键。
    h_u64(&mut h, u64::from(turn.thinking.segments));
    h_u64(&mut h, turn.thinking.lines);
    h_u64(&mut h, u64::from(width));
    h
}

/// 后置装饰（失败详情/尾部空行）的键。
pub(crate) fn turn_post_key(failure: Option<&TimelineFailure>) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x7073_7430); // "pst0"
    if let Some(f) = failure {
        h_str(&mut h, &f.code);
        h_str(&mut h, &f.message);
    }
    h
}
