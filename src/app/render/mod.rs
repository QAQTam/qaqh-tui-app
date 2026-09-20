//! M1 渲染管线：块级刷新入口 + 回归锁 + 基准（plan §3.2/§3.7）。
//!
//! 已落地：
//! - 块级缓存 [`seg::TranscriptCache`]：视口驱动渲染 + 离屏淘汰（估算高度兜底）；
//! - 键体系：块键（无帧号）+ 回合结构键（无 rev）；
//! - 动画出带（锁 8）：块行内只有占位空格 + [`seg::AnimSlot`]，draw 期按当前帧
//!   覆盖 cell（[`apply_anim_slots`]）；旧路径烘焙字形，等价口径 = 缓存行 +
//!   槽位覆盖 == 烘焙行（锁 1 验证）；
//! - 生产路径已接线（T8）：`ensure_render_caches` → [`refresh`]；旧
//!   `refresh_segments_at`/`SegmentCache` 已删，`render_transcript_with_opts`
//!   降级为锁的等价口径 oracle + draw 兑底（T9 验收后评估退役）。
//!
//! 与旧路径的一致性由两件事保证：
//! 1. 块渲染**单源**——`render_transcript::{render_turn_pre, render_block_lines,
//!    render_turn_post}` 同时被旧整回合渲染与本管线调用；
//! 2. 锁 `block_cache_matches_full_render` 逐行对照旧全量输出。
//!
//! 后续增量：turn/text/tools 拆分、流式尾部增量、生产接线（TT8：切 ensure_render_caches）。

#![allow(dead_code)] // M1 骨架：refresh 由回归锁/基准驱动，生产接线在后续增量。

mod estimates;
mod seg;
mod stream;

pub(crate) use seg::{AnimKind, AnimSlot, TranscriptCache, ViewportSlot};

use std::collections::HashMap;

use crate::app::render_line::RenderLine;
use crate::app::render_transcript::{
    AnimSink, banner_cache_key, render_banner, render_block_lines, render_turn_post,
    render_turn_pre,
};
use crate::app::session::SessionState;
use crate::app::timeline_model::{Block, Turn};
use qaqh_client::{TimelineBlockKind, TimelineBlockState, TimelineToolState};

use seg::{BlockBody, BlockSeg, RenderStats, TurnSeg};

/// 布局不动点迭代上限（与旧 `MAX_LAYOUT_PASSES` 同值，防御性护栏）。
const MAX_LAYOUT_PASSES: usize = 4;

/// 块级刷新：与 `refresh_segments_at` 同语义，粒度为块。
///
/// - 阶段 1（结构对齐）：全部键重算；身体复用旧 Lines，缺失处以估算高度占位；
/// - 阶段 2（视口不动点）：keep 窗口内 Height → 精确渲染；窗口外 Lines → 淘汰
///   为精确高度（几何不变）。渲染改变几何 → 迭代至收敛（实测 1~2 趟）；
/// - 无视口 → 全驻留（等价性锁的口径）。
pub(crate) fn refresh(
    session: &SessionState,
    width: u16,
    viewport: Option<(usize, usize)>,
    cache: &mut TranscriptCache,
) -> RenderStats {
    let width = width.max(20);
    let t0 = std::time::Instant::now();
    if cache.width != width {
        *cache = TranscriptCache::new(width);
    }
    let mut stats = RenderStats::default();

    // ── 阶段 1：结构对齐 ──
    // keep 窗口基于**刷新前**几何（含精确/估算高度）计算；离屏回合若块数未变
    // 则原样保留（零工作——键都不重算），这是流式帧 O(keep) 而非 O(全部块) 的关键。
    // 新增回合（下标 >= 旧长度）强制纳入 keep：follow 模式下它们就在视口里。
    let (lo0, hi0) = keep_turn_range(cache, viewport);
    let old_len = cache.turns.len();
    let hi = hi0
        .max(old_len)
        .min(session.timeline.turns.len().saturating_sub(1));
    let lo = lo0.min(hi);
    let old_turns = std::mem::take(&mut cache.turns);
    let mut old_by_id: HashMap<String, TurnSeg> = old_turns
        .into_iter()
        .map(|t| (t.turn_id.clone(), t))
        .collect();

    let mut new_turns = Vec::with_capacity(session.timeline.turns.len());
    for (idx, turn) in session.timeline.turns.iter().enumerate() {
        let old = old_by_id.remove(&turn.turn_id);
        if idx < lo || idx > hi {
            // 离屏：结构未变（块数一致）→ 原样保留；变了 → 估算重对齐。
            let block_count: usize = turn.rounds.iter().map(|r| r.blocks.len()).sum();
            new_turns.push(match old {
                Some(t) if t.content.len() == block_count => t,
                other => {
                    let num = session.timeline.turn_number(idx);
                    let skey = seg::turn_struct_key(turn, num);
                    align_turn_seg(session, turn, num, skey, width, other, &mut stats)
                }
            });
            continue;
        }
        let num = session.timeline.turn_number(idx);
        let skey = seg::turn_struct_key(turn, num);
        new_turns.push(align_turn_seg(
            session, turn, num, skey, width, old, &mut stats,
        ));
    }
    cache.turns = new_turns;

    // ── 阶段 2：视口不动点 ──
    for _pass in 0..MAX_LAYOUT_PASSES {
        let (lo, hi) = keep_turn_range(cache, viewport);
        let mut rendered = 0usize;
        for (ti, (turn, tseg)) in session
            .timeline
            .turns
            .iter()
            .zip(cache.turns.iter_mut())
            .enumerate()
        {
            let num = session.timeline.turn_number(ti);
            if ti < lo || ti > hi {
                evict_turn(tseg);
                continue;
            }
            if tseg.pre.body.lines().is_none() {
                tseg.pre.body = lines_body(render_turn_pre(turn, num, usize::from(width)));
                stats.rebuilt_blocks += 1;
                rendered += 1;
            }
            let blocks = turn.rounds.iter().flat_map(|r| r.blocks.iter());
            for (block, bseg) in blocks.zip(tseg.content.iter_mut()) {
                // Height(0) = 折叠组中间块（§4.2）：0 行即精确，重渲会泄漏完整卡。
                if !bseg.body.is_rendered() && bseg.body.height() > 0 {
                    render_block_into(bseg, block, width);
                    stats.rebuilt_blocks += 1;
                    rendered += 1;
                }
            }
            if tseg.post.body.lines().is_none() {
                tseg.post.body =
                    lines_body(render_turn_post(turn.failure.as_ref(), usize::from(width)));
                stats.rebuilt_blocks += 1;
                rendered += 1;
            }
        }
        if rendered == 0 {
            break;
        }
    }
    // 收尾保证：视口覆盖到的块必须已持有 Lines（不动点未收敛时无条件补渲一次）。
    if let Some((top, height)) = viewport {
        let (lo, hi) = keep_turn_range(cache, viewport);
        let mut acc = cache.banner.as_ref().map_or(0, |b| b.body.height());
        for (ti, (turn, tseg)) in session
            .timeline
            .turns
            .iter()
            .zip(cache.turns.iter_mut())
            .enumerate()
        {
            let num = session.timeline.turn_number(ti);
            let th = tseg.height();
            let covered = (lo..=hi).contains(&ti) && acc + th > top && acc < top + height;
            acc += th;
            if !covered {
                continue;
            }
            if tseg.pre.body.lines().is_none() {
                tseg.pre.body = lines_body(render_turn_pre(turn, num, usize::from(width)));
                stats.rebuilt_blocks += 1;
            }
            let blocks = turn.rounds.iter().flat_map(|r| r.blocks.iter());
            for (block, bseg) in blocks.zip(tseg.content.iter_mut()) {
                // Height(0) = 折叠组中间块（§4.2）：0 行即精确，重渲会泄漏完整卡。
                if !bseg.body.is_rendered() && bseg.body.height() > 0 {
                    render_block_into(bseg, block, width);
                    stats.rebuilt_blocks += 1;
                }
            }
            if tseg.post.body.lines().is_none() {
                tseg.post.body =
                    lines_body(render_turn_post(turn.failure.as_ref(), usize::from(width)));
                stats.rebuilt_blocks += 1;
            }
        }
    }

    // banner：内容与旧 render_banner 共源（1 行，恒驻留）。
    let bkey = banner_cache_key(session, width);
    let banner_line = render_banner(session);
    let banner_stale = match (&cache.banner, &banner_line) {
        (Some(s), Some(_)) => s.key != bkey,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    };
    if banner_stale {
        if let Some(l) = banner_line {
            stats.rebuilt_blocks += 1;
            cache.banner = Some(BlockSeg {
                key: bkey,
                body: lines_body(vec![l]),
                anim: Vec::new(),
                stream: None,
            });
        } else {
            cache.banner = None;
        }
    }

    // 空会话占位（文案与旧路径同串；等价性由锁 1 的空会话用例锁定）。
    let empty_needed = session.timeline.turns.is_empty();
    match (empty_needed, &cache.empty) {
        (true, None) => {
            stats.rebuilt_blocks += 1;
            cache.empty = Some(empty_seg());
        }
        (false, Some(_)) => cache.empty = None,
        _ => {}
    }

    stats.render_us = t0.elapsed().as_micros() as u64;
    stats.resident_blocks = count_resident(cache);
    cache.stats = stats;
    stats
}

/// keep 窗口（回合下标闭区间）；无视口 → 全驻留。
fn keep_turn_range(cache: &TranscriptCache, viewport: Option<(usize, usize)>) -> (usize, usize) {
    let Some((top, height)) = viewport else {
        return (0, usize::MAX);
    };
    if cache.turns.is_empty() {
        return (0, usize::MAX);
    }
    let banner_h = cache.banner.as_ref().map_or(0, |b| b.body.height());
    let bottom = top.saturating_add(height);
    let mut acc = banner_h;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for (i, t) in cache.turns.iter().enumerate() {
        let h = t.height();
        if first.is_none() && acc + h > top {
            first = Some(i);
        }
        if acc < bottom {
            last = i;
        }
        acc += h;
    }
    let Some(f) = first else {
        return (0, usize::MAX);
    };
    (
        f.saturating_sub(seg::KEEP_MARGIN_TURNS),
        (last + seg::KEEP_MARGIN_TURNS).min(cache.turns.len() - 1),
    )
}

/// 淘汰：Lines → 精确高度（几何不变）；估算高度保持（阶段 1 已按新键更新）。
fn evict_turn(tseg: &mut TurnSeg) {
    let evict = |s: &mut BlockSeg| {
        if s.body.is_rendered() {
            s.body = BlockBody::Height(s.body.height());
        }
        // 动画槽位随 body 同生同灭（plan §3.2）：淘汰态不可见不绘制，
        // 重进视口时由 Slots 出口重新生产。
        s.anim = Vec::new();
        // T7：折行中间态一并丢弃（内存 O(1) 纪律）；重进视口整块重渲一次。
        s.stream = None;
    };
    evict(&mut tseg.pre);
    tseg.content.iter_mut().for_each(evict);
    evict(&mut tseg.post);
}

fn lines_body(lines: Vec<RenderLine>) -> BlockBody {
    BlockBody::Lines(lines.into())
}

/// 渲染一个块并挂上动画槽位（Slots 出带——锁 8 生产端唯一入口）。
fn render_block_into(bseg: &mut BlockSeg, block: &Block, width: u16) {
    // T7：Open text 块走流式增量（纯文本+光标；seal 才上 markdown/syntect）。
    // 走到这里 body 必然未渲染 ⇒ 无可续 stream（增量在对齐期完成、evict 已弃）
    // ⇒ 从全文初始化。
    if block.kind == TimelineBlockKind::Text && block.state == TimelineBlockState::Open {
        stream::init(bseg, block, width);
        return;
    }
    let mut sink = AnimSink::Slots(Vec::new());
    let lines = render_block_lines(block, usize::from(width), &mut sink);
    bseg.body = lines_body(lines);
    bseg.anim = sink.into_vec();
    bseg.stream = None; // 不变式护栏：全量体不携带流式状态
}

/// draw 期动画覆盖（plan §3.3 / 锁 8）：把可见槽位按当前帧字形写进 buffer。
///
/// 只改 symbol 不动 style——占位 cell 的样式已由缓存行的 span 携带（Running
/// 转轮的 ToolRun/权限 Warn 覆盖色等），覆盖字形继承原样式。槽位已由
/// [`TranscriptCache::visible_slots`] 过滤到视口内，此处再做一层防御性
/// 裁剪（滚动条列 / 底边）。
pub(crate) fn apply_anim_slots(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    slots: &[seg::ViewportSlot],
) {
    let content_w = area.width.saturating_sub(1); // 末列留给滚动条
    for slot in slots {
        let Some(glyph) = anim_glyph(slot.kind) else {
            continue;
        };
        if slot.col >= content_w || usize::from(slot.row) >= usize::from(area.height) {
            continue; // 半露/越界防御（visible_slots 已过滤一轮）
        }
        if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(
            area.x + slot.col,
            area.y + slot.row,
        )) {
            cell.set_symbol(glyph);
        }
    }
}

/// 槽位 → 当前帧字形。全部宽 1（plan §3.3：占位 cell 与字形同宽，几何不变）。
fn anim_glyph(kind: AnimKind) -> Option<&'static str> {
    match kind {
        AnimKind::Spinner => Some(crate::app::anim::spinner_glyph(
            crate::app::anim::frame_now(),
        )),
        AnimKind::Thinking => Some(crate::app::anim::thinking_glyph(
            crate::app::anim::frame_now(),
        )),
        AnimKind::Cursor => Some("▌"),
        // 多 cell 区域无法用单 cell 槽位表达；当前无生产者（见 AnimKind 文档）。
        AnimKind::ProgressIndeterminate => None,
    }
}

fn empty_seg() -> BlockSeg {
    BlockSeg {
        key: 1,
        body: lines_body(vec![RenderLine::new().span(
            "（暂无回合——输入消息开始对话）",
            crate::app::render_line::SpanStyle::Dim,
        )]),
        anim: Vec::new(),
        stream: None,
    }
}

fn render_block_seg(block: &Block, width: u16, key: u64) -> BlockSeg {
    let mut seg = BlockSeg {
        key,
        body: BlockBody::Height(0),
        anim: Vec::new(),
        stream: None,
    };
    render_block_into(&mut seg, block, width);
    seg
}

/// 阶段 1：结构对齐——键全部重算；身体按 id+键 **move** 复用（零 clone）；
/// §4.2 运行组渲染：把「连续 T2 工具块」序列落成 content 段。
///
/// - 折叠态：首块 = 组行（key 含组签名）；中间块 = Height(0)；组内最后一张
///   Failed 卡**内联**（错误不许藏，用原块 key 走完整渲染路径）。
/// - 展开态：卡片列表（W-02：卡片正文窗口恒定，无卡片级展开位）。
#[allow(clippy::too_many_arguments)]
fn flush_tool_group(
    turn: &Turn,
    round_num: u32,
    group: &mut Vec<&Block>,
    session: &SessionState,
    width: u16,
    old_by_id: &mut HashMap<String, BlockSeg>,
    content: &mut Vec<BlockSeg>,
    stats: &mut RenderStats,
) {
    if group.is_empty() {
        return;
    }
    let group_expanded = session
        .expanded_groups
        .contains(&(turn.turn_id.clone(), round_num));
    // 组签名：成员 id + 终态序列 + 展开位。
    let mut sig = crate::app::render_transcript::FNV_OFFSET;
    crate::app::render_transcript::h_str(&mut sig, &turn.turn_id);
    crate::app::render_transcript::h_u64(&mut sig, u64::from(round_num));
    for b in group.iter() {
        crate::app::render_transcript::h_str(&mut sig, &b.block_id);
        if let Some(tc) = &b.tool {
            crate::app::render_transcript::h_u64(&mut sig, seg::tool_state_tag(tc.state));
        }
    }
    crate::app::render_transcript::h_u64(&mut sig, u64::from(group_expanded));
    sig = sig.wrapping_add(0x9E37_79B9_7F4A_7C15); // GROUP_TAG salt

    if !group_expanded {
        let states: Vec<TimelineToolState> = group
            .iter()
            .filter_map(|b| b.tool.as_ref().map(|tc| tc.state))
            .collect();
        let last_failed = group.iter().rposition(|b| {
            b.tool
                .as_ref()
                .is_some_and(|tc| tc.state == TimelineToolState::Failed)
        });
        for (i, b) in group.iter().enumerate() {
            if Some(i) == last_failed {
                continue; // 失败例外：单独内联（下方）
            }
            let key = seg::block_cache_key(b, width) ^ sig;
            let body = if i == 0 {
                BlockBody::Lines(
                    vec![crate::app::render_transcript::render_group_line(
                        &states,
                        group_expanded,
                        usize::from(width),
                    )]
                    .into(),
                )
            } else {
                BlockBody::Height(0) // 组中间块：折叠态零行
            };
            let seg = match old_by_id.remove(b.block_id.as_str()) {
                Some(mut s) if s.key == key => {
                    // 组行是 O(1) 的便宜物化：**复用时不得沿用淘汰后的 `Height` 占位**。
                    //
                    // 否则：淘汰把 `Lines(组行, 1 行)` 转成 `Height(1)`（保高度、丢内容）
                    // → 下次对齐按键复用 → 物化循环发现「未渲染且高度>0」，用
                    // `render_block_into` 按**单块**重渲——而它不认识「组」，会把组首块
                    // 渲成整张卡（1 行 → 2 行），总行数随滚动漂移（锁 2 / W-09）。
                    if !s.body.is_rendered() {
                        s.body = body;
                    }
                    s
                }
                _ => BlockSeg {
                    key,
                    body,
                    anim: Vec::new(),
                    stream: None,
                },
            };
            content.push(seg);
        }
        if let Some(i) = last_failed {
            let b = group[i];
            let key = seg::block_cache_key(b, width);
            let seg = match old_by_id.remove(b.block_id.as_str()) {
                Some(s) if s.key == key => s,
                _ => BlockSeg {
                    key,
                    body: BlockBody::Height(estimates::estimate_block_lines(b, usize::from(width))),
                    anim: Vec::new(),
                    stream: None,
                },
            };
            content.push(seg);
        }
    } else {
        for b in group.iter() {
            let key = seg::block_cache_key(b, width);
            let seg = match old_by_id.remove(b.block_id.as_str()) {
                Some(s) if s.key == key => s,
                Some(mut stale) => {
                    if stream::carry(&mut stale, b, width) {
                        stats.rebuilt_blocks += 1;
                        stale.key = key;
                        stale
                    } else {
                        BlockSeg {
                            key,
                            body: BlockBody::Height(estimates::estimate_block_lines(
                                b,
                                usize::from(width),
                            )),
                            anim: Vec::new(),
                            stream: None,
                        }
                    }
                }
                None => BlockSeg {
                    key,
                    body: BlockBody::Height(estimates::estimate_block_lines(b, usize::from(width))),
                    anim: Vec::new(),
                    stream: None,
                },
            };
            content.push(seg);
        }
    }
    group.clear();
}

/// Open text 流式块在对齐期就地增量（T7，几何精确），其余缺失处以估算占位。
#[allow(clippy::too_many_arguments)]
fn align_turn_seg(
    session: &SessionState,
    turn: &Turn,
    num: u64,
    skey: u64,
    width: u16,
    old: Option<TurnSeg>,
    stats: &mut RenderStats,
) -> TurnSeg {
    let pre_key = seg::turn_pre_key(turn, num, width);
    let post_key = seg::turn_post_key(turn.failure.as_ref());
    // 所有权化：content 块按 id remove 出来即可 move——零 clone（T7 的 stream
    // 状态也必须 move：逐 delta clone 会退化为 O(text)）。
    let (old_pre, old_post, mut old_content_by_id) = match old {
        Some(t) => {
            let map: HashMap<String, BlockSeg> = t.content_ids.into_iter().zip(t.content).collect();
            (Some(t.pre), Some(t.post), map)
        }
        None => (None, None, HashMap::new()),
    };

    // pre：键匹配即复用身体（Lines 或精确高度）——淘汰态（Height）不复用会让
    // 几何每帧缩水、keep 窗口漂移，导致全量重渲（基准实测抓到的真 bug）。
    let pre = match old_pre.filter(|o| o.key == pre_key) {
        Some(o) => o,
        None => BlockSeg {
            key: pre_key,
            body: BlockBody::Height(estimates::estimate_pre_lines(turn, usize::from(width))),
            anim: Vec::new(),
            stream: None,
        },
    };

    // content：按 block_id + 键复用（结构与顺序变化都不拖累未变块）。
    // §4.2 运行组：round 内**连续 T2 工具块**默认折叠为一行组行；T1 工具
    // （§4.7 词表）永逐单行不进组；展开态（expanded_groups）为卡片列表。
    // 组内块的 key 统一含「组签名」（成员 id+终态序列 + 展开位）——任一成员
    // seal/fail 都会让整组重渲，而未变组的 key 稳定复用。
    let mut content: Vec<BlockSeg> =
        Vec::with_capacity(turn.rounds.iter().map(|r| r.blocks.len()).sum());
    for round in &turn.rounds {
        let mut group: Vec<&Block> = Vec::new();
        for block in &round.blocks {
            let is_groupable = block.kind == TimelineBlockKind::Tool
                && block
                    .tool
                    .as_ref()
                    .is_some_and(|t| !crate::app::render_transcript::is_t1_tool(&t.name));
            if is_groupable {
                group.push(block);
                continue;
            }
            flush_tool_group(
                turn,
                round.round_num,
                &mut group,
                session,
                width,
                &mut old_content_by_id,
                &mut content,
                stats,
            );
            // 组外单块：T1 工具 / 文本 / reasoning(0行) / notice——原路径。
            let key = seg::block_cache_key(block, width);
            let seg = match old_content_by_id.remove(block.block_id.as_str()) {
                Some(s) if s.key == key => s,
                Some(mut stale) => {
                    if stream::carry(&mut stale, block, width) {
                        stats.rebuilt_blocks += 1;
                        stale.key = key;
                        stale
                    } else {
                        BlockSeg {
                            key,
                            body: BlockBody::Height(estimates::estimate_block_lines(
                                block,
                                usize::from(width),
                            )),
                            anim: Vec::new(),
                            stream: None,
                        }
                    }
                }
                None => BlockSeg {
                    key,
                    body: BlockBody::Height(estimates::estimate_block_lines(
                        block,
                        usize::from(width),
                    )),
                    anim: Vec::new(),
                    stream: None,
                },
            };
            content.push(seg);
        }
        flush_tool_group(
            turn,
            round.round_num,
            &mut group,
            session,
            width,
            &mut old_content_by_id,
            &mut content,
            stats,
        );
    }

    let post = match old_post.filter(|o| o.key == post_key) {
        Some(o) => o,
        None => BlockSeg {
            key: post_key,
            body: BlockBody::Height(estimates::estimate_post_lines(
                turn.failure.as_ref(),
                usize::from(width),
            )),
            anim: Vec::new(),
            stream: None,
        },
    };

    let content_ids: Vec<String> = turn
        .rounds
        .iter()
        .flat_map(|r| r.blocks.iter().map(|b| b.block_id.clone()))
        .collect();

    TurnSeg {
        turn_id: turn.turn_id.clone(),
        struct_key: skey,
        pre,
        content,
        content_ids,
        post,
    }
}

fn count_resident(cache: &TranscriptCache) -> usize {
    let mut n = usize::from(
        cache
            .banner
            .as_ref()
            .is_some_and(|b| b.body.lines().is_some()),
    ) + usize::from(
        cache
            .empty
            .as_ref()
            .is_some_and(|e| e.body.lines().is_some()),
    );
    for t in &cache.turns {
        for seg in std::iter::once(&t.pre)
            .chain(t.content.iter())
            .chain(std::iter::once(&t.post))
        {
            n += usize::from(seg.body.is_rendered());
        }
    }
    n
}

#[cfg(test)]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::anim;
    use crate::app::render_transcript::render_transcript_with_opts;
    use crate::app::timeline_model::{Round, ToolCard};
    use qaqh_client::{
        TimelineBlockKind, TimelineBlockState, TimelineFailure, TimelineToolBody,
        TimelineToolState, TimelineTurnState,
    };
    use ratatui::layout::Rect;
    use ratatui::widgets::{Paragraph, Widget};

    // ── fixture ─────────────────────────────────────────────────────

    fn blk(
        id: &str,
        order: u32,
        kind: TimelineBlockKind,
        state: TimelineBlockState,
        text: &str,
    ) -> Block {
        Block {
            block_id: id.to_string(),
            block_order: order,
            kind,
            state,
            text: text.to_string(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        }
    }

    fn tool_blk(id: &str, order: u32, tool_state: TimelineToolState) -> Block {
        Block {
            block_id: id.to_string(),
            block_order: order,
            kind: TimelineBlockKind::Tool,
            state: TimelineBlockState::Sealed,
            text: String::new(),
            tool: Some(ToolCard {
                tool_call_id: id.to_string(),
                name: "bash".to_string(),
                state: tool_state,
                summary: Some("ls -la".to_string()),
                args_json: None,
                output: Some("total 0".to_string()),
                diff: None,
                progress: String::new(),
                progress_truncated: false,
                progress_bytes_total: 0,
                progress_stream: None,
                failure: None,
                permission: None,
                display: None,
            }),
            last_fragment: 0,
            rev: 1,
        }
    }

    fn turn_of(
        id: &str,
        user: &str,
        state: TimelineTurnState,
        failure: Option<TimelineFailure>,
        offloaded: bool,
        blocks: Vec<Block>,
    ) -> Turn {
        Turn {
            thinking: Default::default(),
            turn_id: id.to_string(),
            turn_index: None,
            user_text: user.to_string(),
            state,
            failure,
            sealed: state != TimelineTurnState::Running,
            offloaded,
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: false,
                blocks,
            }],
        }
    }

    /// 覆盖四类块 + 失败 + offload + 流式（动画）三回合的等价性 fixture。
    fn fixture_session() -> SessionState {
        let mut s = SessionState::new("seed".to_string());
        s.timeline.turns.push(turn_of(
            "t1",
            "你好世界",
            TimelineTurnState::Completed,
            None,
            false,
            vec![
                blk(
                    "b1",
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "普通回答",
                ),
                blk(
                    "b2",
                    1,
                    TimelineBlockKind::Notice,
                    TimelineBlockState::Sealed,
                    "一条通知",
                ),
                tool_blk("b3", 2, TimelineToolState::Succeeded),
                blk(
                    "b4",
                    3,
                    TimelineBlockKind::Reasoning,
                    TimelineBlockState::Sealed,
                    "想一想再答",
                ),
            ],
        ));
        s.timeline.turns.push(turn_of(
            "t2",
            "流式回合",
            TimelineTurnState::Running,
            None,
            false,
            vec![
                blk(
                    "b5",
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Open,
                    "正在生成",
                ),
                tool_blk("b6", 1, TimelineToolState::Running),
            ],
        ));
        s.timeline.turns.push(turn_of(
            "t3",
            "",
            TimelineTurnState::Failed,
            Some(TimelineFailure {
                code: "boom".to_string(),
                message: "炸了".to_string(),
            }),
            true,
            vec![],
        ));
        s
    }

    fn flatten(lines: &[&RenderLine]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn block<'a>(sess: &'a mut SessionState, turn_id: &str, block_id: &str) -> &'a mut Block {
        sess.timeline
            .turns
            .iter_mut()
            .find(|t| t.turn_id == turn_id)
            .expect("turn")
            .rounds
            .iter_mut()
            .flat_map(|r| r.blocks.iter_mut())
            .find(|b| b.block_id == block_id)
            .expect("block")
    }

    // ── 锁 8 辅助：槽位覆盖的测试口径 ─────────────────────────────

    /// 缓存行 + 槽位覆盖（帧 frame）——与旧烘焙渲染同口径（锁 1 的比较基线）。
    fn flatten_with_anim(cache: &TranscriptCache, frame: u64) -> Vec<RenderLine> {
        let mut lines: Vec<RenderLine> =
            cache.flatten_lines().iter().map(|l| (*l).clone()).collect();
        let mut start = 0usize;
        let blocks = cache
            .banner
            .iter()
            .chain(cache.turns.iter().flat_map(|t| {
                std::iter::once(&t.pre)
                    .chain(t.content.iter())
                    .chain(std::iter::once(&t.post))
            }))
            .chain(cache.empty.iter());
        for seg in blocks {
            let h = seg.body.height();
            for slot in &seg.anim {
                let glyph = match slot.kind {
                    AnimKind::Spinner => anim::spinner_glyph(frame),
                    AnimKind::Thinking => anim::thinking_glyph(frame),
                    AnimKind::Cursor => "▌",
                    AnimKind::ProgressIndeterminate => continue,
                };
                let row = start + slot.row as usize;
                write_glyph_at(&mut lines[row], slot.col as usize, glyph);
            }
            start += h;
        }
        lines
    }

    /// 把宽 1 字形写到行的指定显示列（该 cell 必须是占位——否则 panic）。
    fn write_glyph_at(line: &mut RenderLine, col: usize, glyph: &str) {
        use unicode_width::UnicodeWidthChar;
        use unicode_width::UnicodeWidthStr;
        let mut acc = 0usize;
        for span in &mut line.spans {
            let w = span.text.width();
            if col >= acc + w {
                acc += w;
                continue;
            }
            let target = col - acc;
            let mut cur = 0usize;
            let mut out = String::with_capacity(span.text.len() + glyph.len());
            let mut replaced = false;
            for ch in span.text.chars() {
                let cw = ch.width().unwrap_or(0);
                if !replaced && cur == target {
                    out.push_str(glyph);
                    replaced = true;
                } else {
                    out.push(ch);
                }
                cur += cw;
            }
            assert!(replaced, "槽位 col={col} 未命中占位 cell（占位纪律被破坏）");
            span.text = out;
            return;
        }
        panic!("槽位 col={col} 超出行宽 {acc}");
    }

    /// 显示列上的字符与宽度（占位纪律断言用）。
    fn cell_at(line: &RenderLine, col: usize) -> (char, usize) {
        use unicode_width::UnicodeWidthChar;
        let mut acc = 0usize;
        for span in &line.spans {
            for ch in span.text.chars() {
                let w = ch.width().unwrap_or(0);
                if acc == col {
                    return (ch, w);
                }
                acc += w;
            }
        }
        panic!("col {col} 超出行宽 {acc}");
    }

    /// 与 `ui::transcript::draw` 同构的 Buffer 构建（Paragraph 渲染缓存行）。
    fn draw_cache_to_buf(cache: &TranscriptCache, width: u16) -> (ratatui::buffer::Buffer, Rect) {
        let total = cache.total_lines();
        let area = Rect::new(0, 0, width, total as u16);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let rat: Vec<ratatui::text::Line> = cache
            .flatten_lines()
            .iter()
            .map(|rl| {
                ratatui::text::Line::from(
                    rl.spans
                        .iter()
                        .map(|s| ratatui::text::Span::raw(s.text.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        Paragraph::new(rat).render(area, &mut buf);
        (buf, area)
    }

    // ── 锁 1：块级缓存 == 旧全量渲染（含空会话用例） ─────────────────
    #[test]
    fn block_cache_matches_full_render() {
        anim::frame_override::set(7);
        let sess = fixture_session();
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let old = render_transcript_with_opts(&sess, 80);
        // 口径：缓存行（占位）+ 槽位覆盖（帧 7）== 旧烘焙渲染——动画出带后
        // 两者只在槽位 cell 上差一个字形，覆盖后必须逐行一致。
        assert_eq!(
            flatten(&flatten_with_anim(&cache, 7).iter().collect::<Vec<_>>()),
            flatten(&old.iter().collect::<Vec<_>>()),
            "缓存行+槽位覆盖 必须与旧烘焙渲染逐行一致"
        );

        // 空会话：占位行也必须一致。
        let empty = SessionState::new("seed".to_string());
        let mut cache2 = TranscriptCache::new(80);
        refresh(&empty, 80, None, &mut cache2);
        let old_empty = render_transcript_with_opts(&empty, 80);
        assert_eq!(
            flatten(&cache2.flatten_lines()),
            flatten(&old_empty.iter().collect::<Vec<_>>()),
        );
        anim::frame_override::clear();
    }

    // ── 锁 2：**任意滚动位置**的窗口逐行与全量渲染一致（几何不漂移）──

    /// 锁 2 的夹具：回合数够深（能扫出多屏），四种形态轮转——纯文本 / 文本+工具卡 /
    /// 工具卡 / 超长行，并混入归档回合。目的是让「估算高度」与「精确高度」
    /// 有机会不一致（漂移只在这种情况下暴露）。
    fn sweep_fixture(n: usize) -> SessionState {
        let mut s = SessionState::new("seed".to_string());
        for i in 0..n {
            let blocks = match i % 4 {
                0 => vec![blk(
                    &format!("b{i}"),
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "第一行\n第二行\n第三行",
                )],
                1 => vec![
                    blk(
                        &format!("b{i}"),
                        0,
                        TimelineBlockKind::Text,
                        TimelineBlockState::Sealed,
                        "```rust\nfn main() {}\n```",
                    ),
                    tool_blk(&format!("b{i}t"), 1, TimelineToolState::Succeeded),
                ],
                2 => vec![tool_blk(&format!("b{i}t"), 0, TimelineToolState::Failed)],
                _ => vec![blk(
                    &format!("b{i}"),
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "这一行特别长用来测 CJK 宽字符在窄视口下的折行行为是否与全量渲染一致存在差异",
                )],
            };
            let offloaded = i % 7 == 6;
            s.timeline.turns.push(turn_of(
                &format!("t{i}"),
                &format!("问题 {i}"),
                TimelineTurnState::Completed,
                None,
                offloaded,
                blocks,
            ));
        }
        s
    }

    /// ①a 定价：把**工具卡**实渲一遍取行数（= 用渲染器做单一事实源的代价）。
    #[ignore = "W-11 量化诊断（非锁）：手动重跑用 `cargo test --bin qaqh-tui -- --ignored --nocapture`"]
    #[test]
    fn zz_w11_price_card() {
        let sess = sweep_fixture(120);
        let blocks: Vec<_> = sess
            .timeline
            .turns
            .iter()
            .flat_map(|t| t.rounds.iter())
            .flat_map(|r| r.blocks.iter())
            .filter(|b| b.kind == TimelineBlockKind::Tool)
            .collect();
        let n = blocks.len();
        let mut sink = crate::app::render::AnimSink::Slots(Vec::new());
        for b in blocks.iter().take(4) {
            let _ = crate::app::render_transcript::render_block_lines(b, 80, &mut sink);
        }
        let t0 = std::time::Instant::now();
        let mut h = 0usize;
        for b in blocks.iter() {
            h += crate::app::render_transcript::render_block_lines(b, 80, &mut sink).len();
        }
        let us = t0.elapsed().as_micros();
        println!(
            "①a: {} 个工具卡实渲共 {} 行，耗时 {us}µs（{:.2}µs/卡）",
            n,
            h,
            us as f64 / n as f64
        );
    }

    /// ①b 定价：给一个 markdown 文本块跑**真实渲染**要多久（估算若复用渲染器口径，
    /// 每个离屏文本块都要付这个成本）。
    #[ignore = "W-11 量化诊断（非锁）：手动重跑用 `cargo test -- --ignored --nocapture`"]
    #[test]
    fn zz_w11_price_markdown() {
        let sess = sweep_fixture(120);
        let blocks: Vec<_> = sess
            .timeline
            .turns
            .iter()
            .flat_map(|t| t.rounds.iter())
            .flat_map(|r| r.blocks.iter())
            .filter(|b| b.kind == TimelineBlockKind::Text)
            .collect();
        let n = blocks.len();
        // 热身后计时
        let mut sink = crate::app::render::AnimSink::Slots(Vec::new());
        for b in blocks.iter().take(4) {
            let _ = crate::app::render_transcript::render_block_lines(b, 80, &mut sink);
        }
        let t0 = std::time::Instant::now();
        let mut total_lines = 0usize;
        for b in blocks.iter() {
            total_lines +=
                crate::app::render_transcript::render_block_lines(b, 80, &mut sink).len();
        }
        let us = t0.elapsed().as_micros();
        println!(
            "①b: {} 个文本块实渲共 {} 行，耗时 {us}µs（{:.2}µs/块）",
            n,
            total_lines,
            us as f64 / n as f64
        );
    }

    /// **W-11 量化诊断**（不是锁，是测量；已 `#[ignore]`，手动重跑）：首帧（空缓存 + 视口）下估算 total 与
    /// 全量渲染 total 的差，按夹具四种形态拆分，并逐块给出 `est` vs 实渲高度。
    #[ignore = "W-11 量化诊断（非锁）：手动重跑用 `cargo test -- --ignored --nocapture`"]
    #[test]
    fn zz_w11_quantify() {
        let sess = sweep_fixture(120);
        let h = 30usize;
        // 精确基准：无视口 = 全驻留全渲染
        let mut exact = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut exact);
        let exact_total = exact.total_lines();
        // 首帧：空缓存 + 视口（betav2 的路径：用刷新前的 total=0 算 top）
        let mut fresh = TranscriptCache::new(80);
        let top = crate::ui::viewport_top(0, h, true, 0);
        refresh(&sess, 80, Some((top, h)), &mut fresh);
        let first_total = fresh.total_lines();
        println!(
            "W-11 首帧 total={first_total} 精确 total={exact_total} 差={}",
            first_total as i64 - exact_total as i64
        );
        // 按形态聚合
        let mut by_form = [0i64; 4];
        for i in 0..120usize {
            by_form[i % 4] += fresh.turns[i].height() as i64 - exact.turns[i].height() as i64;
        }
        for (f, d) in by_form.iter().enumerate() {
            println!("  form{f}: 合计差 {d}（{} 回合）", 120 / 4);
        }
        // 拆 pre / content / post 定位那 53 行到底在哪一段
        let mut acc = [[0i64; 3]; 4];
        for i in 0..120usize {
            let f = i % 4;
            let (e, c) = (&exact.turns[i], &fresh.turns[i]);
            acc[f][0] += c.pre.body.height() as i64 - e.pre.body.height() as i64;
            acc[f][1] += c.content.iter().map(|b| b.body.height()).sum::<usize>() as i64
                - e.content.iter().map(|b| b.body.height()).sum::<usize>() as i64;
            acc[f][2] += c.post.body.height() as i64 - e.post.body.height() as i64;
        }
        for (f, row) in acc.iter().enumerate() {
            println!(
                "  form{f}: pre {:+} / content {:+} / post {:+}",
                row[0], row[1], row[2]
            );
        }
        // 抽两个具体回合看 pre 与块
        for i in [1usize, 2] {
            let (e, c) = (&exact.turns[i], &fresh.turns[i]);
            println!(
                "  turn{i}: pre 精确 {} 首帧 {} | post 精确 {} 首帧 {}",
                e.pre.body.height(),
                c.pre.body.height(),
                e.post.body.height(),
                c.post.body.height()
            );
            let turn = &sess.timeline.turns[i];
            let blocks: Vec<_> = turn.rounds.iter().flat_map(|r| r.blocks.iter()).collect();
            for (bi, b) in blocks.iter().enumerate() {
                println!(
                    "    block{bi} kind={:?} est={} 精确={} 首帧={}",
                    b.kind,
                    estimates::estimate_block_lines(b, 80),
                    e.content[bi].body.height(),
                    c.content[bi].body.height()
                );
            }
        }
    }

    /// **不变式**：`window()` 取出的每一行都必须真实存在，且**在任意滚动位置上**
    /// 都与一次性全量渲染逐行一致（视口内不得有未渲染块、几何不得漂移）。
    ///
    /// 这是虚拟化最容易破的地方：几何用估算、渲染用精确，两者一旦不同步，
    /// 窗口就会缺行或错位（视觉上表现为「内容突然少了一截」）。
    ///
    /// **2026-09-20 补回**：本锁在 M1 迁移（`4372e11`）中随旧 `SegmentCache` 路径
    /// 一起被删（旧位置 `render_transcript.rs:3775`），此后只剩
    /// `offscreen_eviction_counts_blocks` 的**两个位置**（底/顶）作弱化替代，
    /// 中间滚动位置的漂移无人覆盖。plan §3.7 的锁 2 即此。
    ///
    /// ⚠ **本锁一写出来就是红的**，它抓到的是：折叠组的**首块被淘汰后**，`evict_turn`
    /// 把 `Lines(组行, 1 行)` 转成 `Height(1)`（保高度、丢内容）；下次对齐按键复用这个
    /// 占位，物化循环发现「未渲染且高度>0」后用 `render_block_into` 按**单块**重渲——
    /// 而它不认识「组」，把组首块渲成整张卡（1 行 → 2 行）→ 总行数随滚动漂移。
    /// `sweep_fixture(120)` 实测 **21 个滚动位置全部漂移**（`total` 647 → 649/650/652…）。
    ///
    /// 修法见 `flush_tool_group`：组行是 O(1) 的便宜物化，复用时不得沿用淘汰占位。
    #[test]
    fn window_never_contains_unrendered_segments() {
        let sess = sweep_fixture(120);
        let full = render_transcript_with_opts(&sess, 80);
        let full_flat: Vec<String> = full.iter().map(|l| flatten(&[l])).collect();

        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let total = cache.total_lines();
        assert_eq!(total, full_flat.len(), "无视口总行数必须与全量渲染一致");

        let height = 30usize;
        let mut top = total.saturating_sub(height);
        let mut checked = 0usize;
        loop {
            refresh(&sess, 80, Some((top, height)), &mut cache);
            assert_eq!(cache.total_lines(), total, "淘汰不得改变总行数");
            let win = cache.window(top, height);
            assert!(!win.is_empty() || top >= total, "top={top} 窗口不得为空");
            for (i, line) in win.iter().enumerate() {
                assert_eq!(
                    flatten(&[*line]),
                    full_flat[top + i],
                    "top={top} 第 {i} 行与全量渲染不一致（几何漂移）"
                );
            }
            checked += 1;
            if top == 0 {
                break;
            }
            top = top.saturating_sub(height);
            assert!(checked <= 200, "滚动循环未终止");
        }
        assert!(checked > 1, "应至少检查两屏，实际 {checked}");
    }

    /// **首帧几何（W-11）**：生产首帧是「空缓存 + 视口」——此时视口外的块只有估算
    /// 高度，总行数因此不得偏离全量渲染，否则滚动条与滚动位置随物化漂移。
    ///
    /// W-11（2026-09-20）已把**工具卡**这一类做成恒等：估算直接问渲染器要行数
    /// （单一事实源，实测 5.15 µs/卡、每块只算一次），`sweep_fixture(120)` 里
    /// form2（单张 Failed 卡）由 **+27 → 0**。
    ///
    /// **仍有已知残余：markdown 文本**（form1，每回合 +1、合计 +26——估算按**原始行**
    /// 折行，markdown 会把 fence 合并）。**不追它**的理由是实测成本：让估算跑 markdown
    /// 要 **55 µs/块**（≈估算预算两个数量级，千块会话 55ms/帧），远超它换来的 4% 行数精度。
    ///
    /// 所以本锁**把可控形态钉成精确、把残余钉成有界**（每回合最多 +1）——
    /// 这样任何**新的**漂移源都会让本锁变红，而不是被一个宽松的总量阈值盖住。
    #[test]
    fn first_frame_estimate_matches_full_render() {
        let sess = sweep_fixture(120);
        let mut exact = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut exact);
        let exact_total = exact.total_lines();

        let mut fresh = TranscriptCache::new(80);
        let top = crate::ui::viewport_top(0, 30, true, 0);
        refresh(&sess, 80, Some((top, 30)), &mut fresh);

        // ① 可控形态必须**精确**：逐回合比对（比总量更严——能定位到具体回合）
        let mut drift = [0i64; 4];
        for i in 0..120usize {
            drift[i % 4] += fresh.turns[i].height() as i64 - exact.turns[i].height() as i64;
        }
        assert_eq!(drift[0], 0, "form0（纯文本）必须精确");
        assert_eq!(
            drift[2], 0,
            "form2（单张 Failed 卡）必须精确——W-11 ①a 已做成恒等"
        );
        assert_eq!(drift[3], 0, "form3（超长单行）必须精确");
        // ② markdown 残余：有界且**不得少算**（估算「宁少不多」的反面也不能有）
        assert!(
            (0..=30).contains(&drift[1]),
            "form1（markdown 文本）残余应在 [0, 30]（每回合 +1 以内），实测 {}",
            drift[1]
        );
        // ③ 总量：落在 [精确, 精确 + markdown 回合数]
        let total = fresh.total_lines();
        assert!(
            total >= exact_total && total <= exact_total + 30,
            "首帧 total 应落在 [{exact_total}, {}]，实测 {total}",
            exact_total + 30
        );
    }

    // ── 锁 3：流式 delta 只重渲一个块（且不惊动邻居的 Arc 身份） ────
    #[test]
    fn streaming_delta_renders_single_block() {
        let mut sess = fixture_session();
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        // t1/b1 是已封口文本块；t2/b5 是流式块。delta 打在 b5 上。
        let a_before = cache
            .turns
            .iter()
            .find(|t| t.turn_id == "t1")
            .expect("t1")
            .content[0]
            .body
            .lines()
            .expect("lines")
            .clone();

        for i in 0..10u64 {
            let b = block(&mut sess, "t2", "b5");
            b.text.push('x');
            b.rev += 1; // reducer TextDelta 的效果 = 追加 + touch()
            let stats = refresh(&sess, 80, None, &mut cache);
            assert_eq!(stats.rebuilt_blocks, 1, "第 {i} 个 delta 只允许重渲一个块");
        }
        let a_after = cache
            .turns
            .iter()
            .find(|t| t.turn_id == "t1")
            .expect("t1")
            .content[0]
            .body
            .lines()
            .expect("lines")
            .clone();
        assert!(
            std::sync::Arc::ptr_eq(&a_before, &a_after),
            "未受 delta 影响的块必须复用同一 Arc（零拷贝）"
        );
    }

    // ── 锁 4：帧号变化不得触发任何 rebuild ─────────────────────────
    #[test]
    fn anim_frame_triggers_zero_rebuild() {
        anim::frame_override::set(1);
        let sess = fixture_session();
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        anim::frame_override::set(2);
        let stats = refresh(&sess, 80, None, &mut cache);
        assert_eq!(
            stats.rebuilt_blocks, 0,
            "帧号不属于任何块键：动画新鲜度由 draw 期槽位覆盖承担（M1 后续增量）"
        );
        anim::frame_override::clear();
    }

    // ── 锁 5：block_state 显式进键（Open→Sealed 必须 miss） ─────────
    #[test]
    fn block_state_change_invalidates_key() {
        let mut b = blk(
            "b",
            0,
            TimelineBlockKind::Text,
            TimelineBlockState::Open,
            "x",
        );
        let k_open = seg::block_cache_key(&b, 80);
        b.state = TimelineBlockState::Sealed;
        assert_ne!(seg::block_cache_key(&b, 80), k_open, "state 必须进键");
        // rev / 宽度同理（防回退）。
        let k0 = seg::block_cache_key(&b, 80);
        b.rev += 1;
        assert_ne!(seg::block_cache_key(&b, 80), k0);
        assert_ne!(seg::block_cache_key(&b, 100), k0);

        // 集成：refresh 后 seal 一个流式块 → 恰好一个块重渲。
        let mut sess = fixture_session();
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        block(&mut sess, "t2", "b5").state = TimelineBlockState::Sealed;
        let stats = refresh(&sess, 80, None, &mut cache);
        assert_eq!(stats.rebuilt_blocks, 1);
    }

    // ── 锁 6：流式增量 == 一次性渲染（seal 瞬间不跳变） ─────────────
    #[test]
    fn streamed_block_equals_one_shot() {
        // X：open 块逐段追加 + seal；Y：同内容一次性构造。
        let mut sx = SessionState::new("seed".to_string());
        sx.timeline.turns.push(turn_of(
            "t1",
            "u",
            TimelineTurnState::Completed,
            None,
            false,
            vec![blk(
                "b1",
                0,
                TimelineBlockKind::Text,
                TimelineBlockState::Open,
                "wor",
            )],
        ));
        let mut cx = TranscriptCache::new(80);
        refresh(&sx, 80, None, &mut cx);
        for d in ["ld", "!"] {
            let b = block(&mut sx, "t1", "b1");
            b.text.push_str(d);
            b.rev += 1;
            refresh(&sx, 80, None, &mut cx);
        }
        block(&mut sx, "t1", "b1").state = TimelineBlockState::Sealed;
        refresh(&sx, 80, None, &mut cx);

        let mut sy = SessionState::new("seed".to_string());
        sy.timeline.turns.push(turn_of(
            "t1",
            "u",
            TimelineTurnState::Completed,
            None,
            false,
            vec![blk(
                "b1",
                0,
                TimelineBlockKind::Text,
                TimelineBlockState::Sealed,
                "world!",
            )],
        ));
        let mut cy = TranscriptCache::new(80);
        refresh(&sy, 80, None, &mut cy);

        let lx = flatten(
            &cx.turns[0].content[0]
                .body
                .lines()
                .expect("lines")
                .iter()
                .collect::<Vec<_>>(),
        );
        let ly = flatten(
            &cy.turns[0].content[0]
                .body
                .lines()
                .expect("lines")
                .iter()
                .collect::<Vec<_>>(),
        );
        assert_eq!(lx, ly, "流式增量到达 seal 的结果必须与一次性渲染逐行一致");
    }

    // ── 锁 7：视口淘汰（离屏退化估算，几何/窗口语义不变） ───────────
    #[test]
    fn offscreen_eviction_counts_blocks() {
        let mut sess = SessionState::new("seed".to_string());
        for i in 0..20 {
            sess.timeline.turns.push(turn_of(
                &format!("t{i}"),
                &format!("输入 {i}"),
                TimelineTurnState::Completed,
                None,
                false,
                vec![blk(
                    &format!("b{i}"),
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    &format!("内容 {i}\n第二行"),
                )],
            ));
        }
        let old = render_transcript_with_opts(&sess, 80);
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let total = cache.total_lines();
        assert_eq!(total, old.len(), "总行数必须与旧全量渲染一致");
        let all_blocks = 20 * 3;
        assert_eq!(cache.stats.resident_blocks, all_blocks, "无视口必须全驻留");

        // 底部视口：离屏淘汰为估算高度（精确高度，几何不变），驻留受限。
        let height = 10usize;
        let top = total - height;
        refresh(&sess, 80, Some((top, height)), &mut cache);
        assert_eq!(cache.total_lines(), total, "淘汰不得改变总行数");
        assert!(
            cache.stats.resident_blocks < all_blocks,
            "离屏必须被淘汰，实际 {}",
            cache.stats.resident_blocks
        );
        assert!(
            cache.stats.resident_blocks <= (2 * seg::KEEP_MARGIN_TURNS + 1) * 3 + 2,
            "驻留受 keep 窗口限制，实际 {}",
            cache.stats.resident_blocks
        );
        // 窗口语义：底部视口的行 == 旧全量渲染的对应行。
        let win = cache.window(top, height);
        assert_eq!(win.len(), height);
        for (i, l) in win.iter().enumerate() {
            assert_eq!(
                flatten(&[*l]),
                flatten(&[&old[top + i]]),
                "top+{i} 行不一致（几何漂移）"
            );
        }

        // 滚回顶部：视口外块重渲（估算 → 精确），窗口仍逐行一致。
        let stats_top = refresh(&sess, 80, Some((0, height)), &mut cache);
        assert!(stats_top.rebuilt_blocks > 0, "滚回视口外必须重渲");
        let win_top = cache.window(0, height);
        for (i, l) in win_top.iter().enumerate() {
            assert_eq!(flatten(&[*l]), flatten(&[&old[i]]), "顶部第 {i} 行不一致");
        }
    }

    // ── 锁 8：AnimSlot 占位纪律 + draw 槽位覆盖（plan §3.3/§3.7-8） ────

    /// CJK 紧贴槽位：覆盖落点必须是渲染期预留的占位空格（宽 1），单 cell
    /// 覆盖绝不切裂宽字符；帧推进零 rebuild 下字形仍前进（动画出带收益）。
    #[test]
    fn anim_slot_never_splits_wide_char() {
        anim::frame_override::set(7);
        let mut sess = SessionState::new("seed".to_string());
        // inline 形态：icon 在 `  ` 之后 col=2，右侧紧跟 CJK 工具名
        let mut inline = tool_blk("b1", 0, TimelineToolState::Running);
        let t = inline.tool.as_mut().expect("tool");
        t.name = "压缩".to_string();
        t.summary = Some("进行中清单".to_string());
        // Block 形态（Running + progress）：icon 在 ` ┃ ` 之后 col=3、row=1
        let mut block = tool_blk("b2", 1, TimelineToolState::Running);
        block.tool.as_mut().expect("tool").progress = "扫描中…".to_string();
        // 流式文本：光标贴在 CJK 之后（10 个 CJK 字符 → 显示宽 20，col=20）
        let streaming = blk(
            "b3",
            2,
            TimelineBlockKind::Text,
            TimelineBlockState::Open,
            "压缩压缩压缩压缩压缩",
        );
        // §4.2：Running 工具默认折叠进组行——spinner 槽位属「展开卡」语义，
        // 本锁测槽位几何，故先展开 t1/round0 的组。
        sess.expanded_groups.insert(("t1".to_string(), 0));
        sess.timeline.turns.push(turn_of(
            "t1",
            "用户",
            TimelineTurnState::Running,
            None,
            false,
            vec![inline, block],
        ));
        sess.timeline.turns.push(turn_of(
            "t2",
            "",
            TimelineTurnState::Running,
            None,
            false,
            vec![streaming],
        ));

        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);

        // ── 生产端：槽位坐标 + 占位纪律 ──
        let inline_seg = &cache.turns[0].content[0];
        let block_seg = &cache.turns[0].content[1];
        let text_seg = &cache.turns[1].content[0];
        assert_eq!(
            inline_seg.anim,
            vec![AnimSlot {
                row: 0,
                col: 2,
                kind: AnimKind::Spinner
            }]
        );
        assert_eq!(
            block_seg.anim,
            vec![AnimSlot {
                row: 1,
                col: 3,
                kind: AnimKind::Spinner
            }]
        );
        assert_eq!(
            text_seg.anim,
            vec![AnimSlot {
                row: 0,
                col: 20,
                kind: AnimKind::Cursor
            }]
        );
        for (seg, row, col) in [
            (inline_seg, 0usize, 2usize),
            (block_seg, 1, 3),
            (text_seg, 0, 20),
        ] {
            let line = seg.body.line(row).expect("块内行");
            let (ch, w) = cell_at(line, col);
            assert_eq!((ch, w), (' ', 1), "槽位 cell 必须是宽 1 预留占位空格");
        }

        // ── draw 端：Buffer 覆盖 → 字形就位 + 宽字符零切裂 ──
        let total = cache.total_lines();
        let slots = cache.visible_slots(0, total);
        assert_eq!(slots.len(), 3, "全视口下三个槽位全部可见");
        let (buf, area) = draw_cache_to_buf(&cache, 80);
        let mut buf = buf;
        // 占位纪律（buffer 口径）：覆盖前槽位 cell 必须是占位空格——
        // 本地 ratatui 对宽字符后续 cell 是 reset（symbol 空、无 skip 标记），
        // 若槽位落在 continuation cell 上会在此处直接红。
        for s in &slots {
            assert_eq!(
                buf[(area.x + s.col, area.y + s.row)].symbol(),
                " ",
                "覆盖前槽位 ({},{}) 必须是预留占位空格",
                s.row,
                s.col
            );
        }
        apply_anim_slots(&mut buf, area, &slots);
        for s in &slots {
            let expected = match s.kind {
                AnimKind::Spinner => anim::spinner_glyph(7),
                AnimKind::Cursor => "▌",
                AnimKind::Thinking => anim::thinking_glyph(7),
                AnimKind::ProgressIndeterminate => continue,
            };
            let cell = &buf[(area.x + s.col, area.y + s.row)];
            assert_eq!(cell.symbol(), expected, "槽位 ({},{}) 字形", s.row, s.col);
        }
        // 宽字符完整性：每个宽字符的第二 cell 必须仍是空白（reset 后的空 cell
        // 在本地 ratatui 里 symbol 就是 " "；被单 cell 覆盖切裂时会带字形/字符）。
        for y in 0..area.height {
            for x in 0..area.width - 1 {
                let cell = &buf[(x, y)];
                if unicode_width::UnicodeWidthStr::width(cell.symbol()) == 2 {
                    let next = buf[(x + 1, y)].symbol();
                    assert!(
                        next.is_empty() || next == " ",
                        "宽字符 ({x},{y}) 的第二 cell 非空白——被覆盖切裂"
                    );
                }
            }
        }

        // ── 帧推进：零 rebuild，字形仍前进 ──
        anim::frame_override::set(1);
        let stats = refresh(&sess, 80, None, &mut cache);
        assert_eq!(stats.rebuilt_blocks, 0, "帧号不属于块键（锁 4）");
        let slots1 = cache.visible_slots(0, total);
        let (buf1, area1) = draw_cache_to_buf(&cache, 80);
        let mut buf1 = buf1;
        apply_anim_slots(&mut buf1, area1, &slots1);
        let spin = slots1
            .iter()
            .find(|s| s.kind == AnimKind::Spinner)
            .expect("spinner slot");
        assert_eq!(
            buf1[(area1.x + spin.col, area1.y + spin.row)].symbol(),
            anim::spinner_glyph(1)
        );
        assert_ne!(anim::spinner_glyph(1), anim::spinner_glyph(7));
        anim::frame_override::clear();
    }

    /// 半露槽位：视口外行 / col ≥ 缓存宽 / 底边裁剪一律不产出（plan §3.3）。
    #[test]
    fn anim_slots_skip_half_visible() {
        let seg = |h: usize, anim: Vec<AnimSlot>| BlockSeg {
            key: 0,
            body: BlockBody::Lines((0..h).map(|_| RenderLine::plain("行")).collect()),
            anim,
            stream: None,
        };
        let slot = |row: u16, col: u16, kind: AnimKind| AnimSlot { row, col, kind };
        // 布局：pre[0..2) content[2..7) post[7..8)
        let mut cache = TranscriptCache::new(80);
        cache.turns.push(TurnSeg {
            turn_id: "t".to_string(),
            struct_key: 0,
            pre: seg(2, vec![slot(0, 79, AnimKind::Spinner)]),
            content: vec![seg(
                5,
                vec![
                    slot(1, 80, AnimKind::Spinner), // col == 缓存宽 → 过滤
                    slot(4, 3, AnimKind::Cursor),   // abs = 6
                ],
            )],
            content_ids: vec!["b".to_string()],
            post: seg(1, vec![slot(0, 0, AnimKind::Cursor)]), // abs = 7
        });

        // 视口 [0,4)：pre 槽位可见；col 80 被宽过滤；row4→abs6 越界；post 越界。
        assert_eq!(
            cache.visible_slots(0, 4),
            vec![ViewportSlot {
                row: 0,
                col: 79,
                kind: AnimKind::Spinner
            }]
        );
        // 视口 [1,4)：pre 槽位 abs=0 < top → 半露跳过。
        assert!(cache.visible_slots(1, 4).is_empty());
        // 视口 [6,8)：content 尾槽位 (6,3) + post (7,0)。
        assert_eq!(
            cache.visible_slots(6, 2),
            vec![
                ViewportSlot {
                    row: 0,
                    col: 3,
                    kind: AnimKind::Cursor
                },
                ViewportSlot {
                    row: 1,
                    col: 0,
                    kind: AnimKind::Cursor
                },
            ]
        );
    }

    /// 淘汰清 anim + 弃 stream（plan §3.2：槽位/状态与 body 同生命周期；
    /// 重进视口由 Slots/全渲重产）。
    #[test]
    fn eviction_clears_anim_slots() {
        let mut sess = SessionState::new("seed".to_string());
        for i in 0..20 {
            let blocks = if i == 0 {
                vec![blk(
                    "b0",
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Open,
                    "流式内容",
                )]
            } else {
                vec![blk(
                    &format!("b{i}"),
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "内容",
                )]
            };
            sess.timeline.turns.push(turn_of(
                &format!("t{i}"),
                "用户",
                TimelineTurnState::Completed,
                None,
                false,
                blocks,
            ));
        }
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        assert!(
            !cache.turns[0].content[0].anim.is_empty(),
            "全驻留时流式块持有光标槽位"
        );
        assert!(
            cache.turns[0].content[0].stream.is_some(),
            "全驻留时流式块持有增量状态"
        );
        let total = cache.total_lines();
        let height = 10usize;
        refresh(&sess, 80, Some((total - height, height)), &mut cache);
        assert!(
            cache.turns[0].content[0].anim.is_empty(),
            "离屏淘汰必须清空槽位"
        );
        assert!(
            cache.turns[0].content[0].stream.is_none(),
            "淘汰必须丢弃流式状态（内存 O(1)）"
        );
        assert_eq!(cache.total_lines(), total, "淘汰不得改变总行数");
        refresh(&sess, 80, Some((0, height)), &mut cache);
        assert!(
            !cache.turns[0].content[0].anim.is_empty(),
            "滚回视口必须重新产出槽位"
        );
    }

    // ── T7：流式尾部增量（plan §3.4） ─────────────────────────────

    /// delta 只动尾行：prefix Arc 跨 delta 零拷贝共享（无折行事件），
    /// 折行事件才重建前缀；seal 整块转全量体并弃 stream。
    #[test]
    fn stream_delta_updates_tail_and_shares_prefix() {
        let mut sess = SessionState::new("seed".to_string());
        sess.timeline.turns.push(turn_of(
            "t1",
            "u",
            TimelineTurnState::Running,
            None,
            false,
            vec![blk(
                "b1",
                0,
                TimelineBlockKind::Text,
                TimelineBlockState::Open,
                "hello",
            )],
        ));
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let BlockBody::Streaming { prefix, tail } = &cache.turns[0].content[0].body else {
            panic!("流式块应为 Streaming 体");
        };
        assert_eq!(prefix.len(), 0);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].spans[0].text, "hello "); // Slots：▌ → 占位空格
        let p0 = std::sync::Arc::clone(prefix);

        // 无折行 delta：prefix 同一 Arc（零拷贝），只有尾行变。
        let b = block(&mut sess, "t1", "b1");
        b.text.push_str(" world");
        b.rev += 1;
        refresh(&sess, 80, None, &mut cache);
        let BlockBody::Streaming { prefix, tail } = &cache.turns[0].content[0].body else {
            panic!("流式块应为 Streaming 体");
        };
        assert!(
            std::sync::Arc::ptr_eq(&p0, prefix),
            "无折行事件时 prefix 必须零拷贝共享"
        );
        assert_eq!(tail[0].spans[0].text, "hello world ");

        // 折行事件：前缀增长（新 Arc）。注意 ls 断行 + 硬断各封一行：
        // "hello world" 的空格留下 last_space → 80 个 x 先在空格处断出
        // "hello"，残余 80 列再硬断 —— 共 2 条封口行（与一次性折行一致）。
        let b = block(&mut sess, "t1", "b1");
        b.text.push_str(&"x".repeat(80));
        b.rev += 1;
        refresh(&sess, 80, None, &mut cache);
        let BlockBody::Streaming { prefix, .. } = &cache.turns[0].content[0].body else {
            panic!("流式块应为 Streaming 体");
        };
        assert_eq!(prefix.len(), 2, "折行事件 → 前缀增长");
        assert!(!std::sync::Arc::ptr_eq(&p0, prefix));

        // seal：整块转全量体（markdown 路径），stream 弃。
        let b = block(&mut sess, "t1", "b1");
        b.state = TimelineBlockState::Sealed;
        b.rev += 1;
        refresh(&sess, 80, None, &mut cache);
        let seg = &cache.turns[0].content[0];
        assert!(seg.body.lines().is_some(), "seal 后必须是全量体");
        assert!(seg.stream.is_none(), "seal 弃流式状态");
    }

    /// 结构键不得包含块 rev（否则任一 delta 拖累整回合，块级增量失效）。
    #[test]
    fn struct_key_ignores_block_revs() {
        let sess = fixture_session();
        let t = sess.timeline.turns.first().expect("t1");
        let k0 = seg::turn_struct_key(t, 1);
        // 直接改 rev 而不动结构（块 id/顺序不变）。
        let mut sess2 = fixture_session();
        sess2.timeline.turns[0].rounds[0].blocks[0].rev += 1;
        let k1 = seg::turn_struct_key(&sess2.timeline.turns[0], 1);
        assert_eq!(k0, k1, "rev 不是结构：不得进 struct_key");
        // 结构变化（块增删）必须改键。
        sess2.timeline.turns[0].rounds[0].blocks.pop();
        let k2 = seg::turn_struct_key(&sess2.timeline.turns[0], 1);
        assert_ne!(k0, k2);
    }

    // ── T11a：运行组折叠 / 失败例外 / T1 单行 ──────────────────────

    /// 平化缓存行为字符串向量（行级断言辅助）。
    fn flat(cache: &TranscriptCache) -> Vec<String> {
        cache
            .flatten_lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn tool_group_collapses_to_one_line_with_failure_inline() {
        let mut sess = SessionState::new("s".to_string());
        sess.timeline.turns.push(turn_of(
            "t1",
            "问",
            TimelineTurnState::Completed,
            None,
            false,
            vec![
                tool_blk("g1", 0, TimelineToolState::Succeeded),
                tool_blk("g2", 1, TimelineToolState::Succeeded),
                tool_blk("g3", 2, TimelineToolState::Failed),
            ],
        ));
        // 折叠态：一行组行 + Failed 卡内联（错误不许藏）。
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let lines = flat(&cache);
        let group = lines
            .iter()
            .find(|l| l.contains("tool calls"))
            .expect("折叠态应有组行");
        assert!(
            group.contains("3 tool calls") && group.contains("2✓") && group.contains("1✗"),
            "组行计数：{group}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("tool calls")).count(),
            1,
            "组行恰好一行：{lines:?}"
        );
        // 阶段 2 不许把折叠组中间块渲成完整卡（泄漏回归锁）：折叠态只有
        // Failed 卡一个 bash 标题。
        assert_eq!(
            lines.iter().filter(|l| l.contains("bash")).count(),
            1,
            "折叠态仅 Failed 卡可见：{lines:?}"
        );

        // 展开态：三张卡全部可见（不再有组行）。
        sess.expanded_groups.insert(("t1".to_string(), 0));
        let mut c2 = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut c2);
        let lines2 = flat(&c2);
        assert!(
            !lines2.iter().any(|l| l.contains("tool calls")),
            "展开态无组行：{lines2:?}"
        );
        assert_eq!(
            lines2.iter().filter(|l| l.contains("bash")).count(),
            3,
            "三张卡标题可见：{lines2:?}"
        );
    }

    #[test]
    fn t1_tool_is_single_line_with_args_preview() {
        let mut sess = SessionState::new("s".to_string());
        let mut b = tool_blk("k1", 0, TimelineToolState::Succeeded);
        {
            let t = b.tool.as_mut().expect("tool");
            t.name = "todo_update".to_string();
            t.args_json = Some(String::from("{\"id\":\"T3\"}"));
        }
        sess.timeline.turns.push(turn_of(
            "t1",
            "问",
            TimelineTurnState::Completed,
            None,
            false,
            vec![b],
        ));
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let lines = flat(&cache);
        let n = lines.iter().filter(|l| l.contains("todo_update")).count();
        assert_eq!(n, 1, "T1 恒单行：{lines:?}");
    }

    // ── 契约 P2：运行卡体量标注（progress_bytes_total / progress_stream）──

    /// 契约 §6 P2：Running 卡显示 `↓ 12.3 KB`（累计观测字节）+ 非 stdout 流标注；
    /// 终态不显示（以 metrics 尾注为准，不重复）。
    #[test]
    fn running_card_shows_progress_bytes() {
        let mut sess = SessionState::new("s".to_string());
        // t1：Block 形态（progress 非空）+ stderr 流。
        let mut b1 = tool_blk("b1", 0, TimelineToolState::Running);
        {
            let t = b1.tool.as_mut().expect("tool");
            t.progress = "line\n".to_string();
            t.progress_bytes_total = 12_600;
            t.progress_stream = Some("stderr".to_string());
        }
        // t2：inline 形态（无 progress 文本）。
        let mut b2 = tool_blk("b2", 0, TimelineToolState::Running);
        b2.tool.as_mut().expect("tool").progress_bytes_total = 12_600;
        // t3：终态——不显示。
        let mut b3 = tool_blk("b3", 0, TimelineToolState::Succeeded);
        b3.tool.as_mut().expect("tool").progress_bytes_total = 12_600;
        sess.timeline.turns.push(turn_of(
            "t1",
            "问",
            TimelineTurnState::Running,
            None,
            false,
            vec![b1],
        ));
        sess.timeline.turns.push(turn_of(
            "t2",
            "问",
            TimelineTurnState::Running,
            None,
            false,
            vec![b2],
        ));
        sess.timeline.turns.push(turn_of(
            "t3",
            "问",
            TimelineTurnState::Completed,
            None,
            false,
            vec![b3],
        ));
        for tid in ["t1", "t2", "t3"] {
            sess.expanded_groups.insert((tid.to_string(), 0));
        }
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let lines = flat(&cache);
        let all = lines.join("\n");
        assert!(all.contains("↓ 12.3 KB · stderr"), "Block 形态标注：{all}");
        assert_eq!(
            all.matches("↓ 12.3 KB").count(),
            2,
            "仅两张 Running 卡显示（终态不重复）：{all}"
        );
    }

    // ── T12：投影优先（M3）────────────────────────────────────

    /// T1 行：display.summary 非空时优先于 args 提炼预览（M3 双写收敛）。
    #[test]
    fn t1_line_prefers_display_summary() {
        let mut sess = SessionState::new("s".to_string());
        let mut b = tool_blk("k1", 0, TimelineToolState::Succeeded);
        {
            let t = b.tool.as_mut().expect("tool");
            t.name = "todo_write".to_string();
            t.args_json = Some(String::from(r#"{"todos":"..."}"#));
            t.display = Some(crate::app::render_transcript::test_display(
                Some("5 todos · 2 done"),
                None,
            ));
        }
        sess.timeline.turns.push(turn_of(
            "t1",
            "问",
            TimelineTurnState::Completed,
            None,
            false,
            vec![b],
        ));
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let lines = flat(&cache);
        let t1 = lines
            .iter()
            .find(|l| l.contains("todo_write"))
            .expect("T1 行");
        assert!(t1.contains("5 todos"), "摘要优先：{t1}");
        assert!(!t1.contains('['), "不再叠 args 预览：{t1}");
    }

    /// T2 卡：结构化 body 存在时无 args 提炼段；display=None（H16）保留。
    #[test]
    fn t2_args_preview_skipped_only_with_structured_body() {
        let mut sess = SessionState::new("s".to_string());
        let mut with_body = tool_blk("b1", 0, TimelineToolState::Succeeded);
        {
            let t = with_body.tool.as_mut().expect("tool");
            t.name = "grep".to_string();
            t.args_json = Some(String::from(r#"{"pattern":"needle"}"#));
            t.display = Some(crate::app::render_transcript::test_display(
                None,
                Some(TimelineToolBody::Text {
                    text: "hit
"
                    .to_string(),
                    truncated: false,
                }),
            ));
        }
        let mut legacy = tool_blk("b2", 1, TimelineToolState::Succeeded);
        {
            let t = legacy.tool.as_mut().expect("tool");
            t.name = "grep".to_string();
            t.args_json = Some(String::from(r#"{"pattern":"needle"}"#));
        }
        sess.timeline.turns.push(turn_of(
            "t1",
            "问",
            TimelineTurnState::Completed,
            None,
            false,
            vec![with_body, legacy],
        ));
        // 组展开态（否则卡片被 T11a 组折叠）。
        sess.expanded_groups.insert(("t1".to_string(), 0));
        let mut cache = TranscriptCache::new(80);
        refresh(&sess, 80, None, &mut cache);
        let lines = flat(&cache);
        let args_lines: Vec<&String> = lines.iter().filter(|l| l.contains('⌗')).collect();
        assert_eq!(args_lines.len(), 1, "仅 H16 旧卡保留 args 段：{lines:?}");
        assert!(args_lines[0].contains("needle"), "{args_lines:?}");
    }

    // ── T13a：「查看更早」悬幅三态 ──────────────────────────────

    #[test]
    fn banner_reflects_loading_and_more_states() {
        fn one_line(l: &RenderLine) -> String {
            l.spans.iter().map(|s| s.text.as_str()).collect()
        }
        let mut sess = SessionState::new("s".to_string());
        assert!(render_banner(&sess).is_none(), "无历史即无悬幅");
        sess.timeline.has_more = true;
        let l = render_banner(&sess).expect("has_more 悬幅");
        assert!(
            one_line(&l).contains("PgUp"),
            "按钮键位提示：{}",
            one_line(&l)
        );
        sess.loading_older = true;
        let l = render_banner(&sess).expect("loading 悬幅");
        assert!(
            one_line(&l).contains("正在加载"),
            "加载中状态优先：{}",
            one_line(&l)
        );
        sess.loading_older = false;
        sess.timeline.has_more = false;
        sess.timeline.truncated_before = true;
        let l = render_banner(&sess).expect("truncated 悬幅");
        assert!(
            one_line(&l).contains("无法翻到"),
            "截断诚实标注：{}",
            one_line(&l)
        );
    }

    // ── T9 验收：M1 等价矩阵 ───────────────────────────────────
    /// 底部视口（与 bench 同口径）。
    fn bottom_vp(total: usize) -> Option<(usize, usize)> {
        Some((total.saturating_sub(40), 40))
    }

    /// 首个差异行的高质量报错（逐行 diff；全量 assert_eq 在长会话下不可读）。
    fn assert_lines_eq(got: &[RenderLine], want: &[RenderLine], ctx: &str) {
        assert_eq!(
            got.len(),
            want.len(),
            "{ctx}: 行数不一致 got={} want={}",
            got.len(),
            want.len()
        );
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            let gs = flatten(&[g]);
            let ws = flatten(&[w]);
            assert_eq!(gs, ws, "{ctx}: 第 {i} 行不一致（got/want）");
        }
    }

    /// 丰富 fixture：CJK 长行 + markdown 围栏 + 长工具输出 + offloaded +
    /// failure + Running 流式回合（覆盖渲染管线的全部分支）。
    fn fixture_rich() -> SessionState {
        let mut s = fixture_session();
        s.timeline.turns.push(turn_of(
            "t3",
            "这一行特别长用来测试 CJK 宽字符在窄视口下的折行行为是否与旧渲染一致存在差异",
            TimelineTurnState::Completed,
            None,
            false,
            vec![
                blk(
                    "b6",
                    0,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "```rust\nfn main() {\n    println!(\"你好，世界\"); // 中文注释也参与折行\n}\n```",
                ),
                tool_blk("b7", 1, TimelineToolState::Failed),
                blk(
                    "b8",
                    2,
                    TimelineBlockKind::Text,
                    TimelineBlockState::Sealed,
                    "短答案。\n\n第二段含列表：\n- 项目一\n- 项目二\n  - 嵌套项",
                ),
            ],
        ));
        // offloaded 回合：只渲染归档头，不渲染正文。
        s.timeline.turns.push(turn_of(
            "t4",
            "被归档的旧回合",
            TimelineTurnState::Completed,
            None,
            true,
            vec![blk(
                "b9",
                0,
                TimelineBlockKind::Text,
                TimelineBlockState::Sealed,
                "正文不应出现",
            )],
        ));
        // failure 回合。
        s.timeline.turns.push(turn_of(
            "t5",
            "失败的回合",
            TimelineTurnState::Failed,
            Some(TimelineFailure {
                code: "timeout".to_string(),
                message: "网络超时".to_string(),
            }),
            false,
            vec![blk(
                "b10",
                0,
                TimelineBlockKind::Text,
                TimelineBlockState::Sealed,
                "超时前的部分输出",
            )],
        ));
        s
    }

    /// T9 验收主体：宽 × F3 × 动画帧 的等价矩阵 + 淘汰态窗口对照。
    /// 失败 = M1 管线与旧全量渲染不再等价，必须先修后行。
    ///
    /// W-02：原矩阵还有一个「工具展开」维度（`expanded_tools`）——卡片级展开态
    /// 已删除、正文窗口恒定，该维度随之消失。
    #[test]
    fn m1_acceptance_equivalence_matrix() {
        for w in [40usize, 100] {
            for frame in [0u64, 7] {
                let ww = w as u16;
                let sess = fixture_rich();
                let ctx = format!("w={w} frame={frame}");
                // oracle 烘焙与槽位覆盖必须同帧（锁 1 同款前提）。
                anim::frame_override::set(frame);

                // 全量几何口径：缓存行（占位）+ 槽位覆盖 == oracle。
                let mut cache = TranscriptCache::new(ww);
                refresh(&sess, ww, None, &mut cache);
                let old = render_transcript_with_opts(&sess, ww);
                assert_lines_eq(&flatten_with_anim(&cache, frame), &old, &ctx);

                // 淘汰态窗口对照：先建全量几何，再模拟滚到底淘汰离屏，
                // 窗口行必须与 oracle 尾部区间逐行一致。
                let total = cache.total_lines();
                refresh(&sess, ww, bottom_vp(total), &mut cache);
                let win = flatten_with_anim(&cache, frame);
                let top = total - win.len();
                assert_lines_eq(
                    &win,
                    &old[top..total],
                    &format!("{ctx} [淘汰窗口 {top}..{total}]"),
                );
                anim::frame_override::clear();
            }
        }
    }
}
