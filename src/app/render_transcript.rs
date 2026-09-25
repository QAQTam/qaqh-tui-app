//! transcript 渲染器：TimelineModel → Vec<RenderLine>（预折行，缓存友好）。

use crate::app::render::{AnimKind, AnimSlot};
use crate::app::render_line::{RenderLine, SpanStyle, wrap_text};
use crate::app::session::SessionState;
use crate::app::timeline_model::{Block, Turn};
use qaqh_client::{
    TimelineBlockKind, TimelineFailure, TimelineToolBody, TimelineToolDisplay, TimelineToolHeader,
    TimelineToolState, TimelineTurnState,
};

/// 推理块折叠时保留的尾部行数（历史常量，当前默认展开路径不再截尾，保留供 hide 回退）。
#[allow(dead_code)]
const REASONING_TAIL: usize = 2;
/// 单个 markdown 块渲染出的行数上限。
///
/// 原文注释指向 `docs/markdown-plan.md`，该文档已于 `18463b3` 删除（T-05）；
/// 理据就地写在这里：markdown 富化会把表格/代码块栅格化成 `RenderLine`，超大块
/// 在预折行缓存里成倍放大，故设上限并在末尾如实标注省略行数（截断必须可见，
/// 与 B1「丢弃可观测」同一设计原则）。
const MD_BLOCK_LINE_CAP: usize = 500;

/// 卡片正文窗口（W-02 裁决，2026-09-20）：**卡片正文不展开**，高度恒 ≤ 7 行。
///
/// - Running：固定 [`TOOL_BODY_RUNNING_TAIL`] 行尾窗（流式进度滚动窗口）。
/// - 结束（Sealed ✓ / Failed ✗，含 Cancelled / Backgrounded）：前
///   [`TOOL_BODY_EDGE`] 行 + 折叠标注 + 后 [`TOOL_BODY_EDGE`] 行。
/// - 总行数 ≤ 2×EDGE：全显且**不加标注**（否则首尾窗口会重叠）。
///
/// 计数一律按**源文本行**（与 `wc -l` 一致）——折行只影响屏幕行数。旧实现同时用
/// 「源行数」（`.take(24)`）与「折行后行数」（`shown_lines > 24`）两个不同的 24，
/// 同一处帽子连单位都不自洽；本裁决把 24 / 8 / 500 三种行帽一并取消。
///
/// **登记的代价**（plan §4.3 修订说明）：中段内容在 TUI 内没有入口，`…折叠 n 行…`
/// 只报数不给内容，全文只剩 `/export` 一条路。按 B1 合规（丢弃有计数），但可达性
/// 下降一档——错误信息若恰在中段，用户看不到。
const TOOL_BODY_EDGE: usize = 3;
/// Running 卡的尾窗行数（流式进度滚动窗口）。
const TOOL_BODY_RUNNING_TAIL: usize = 6;

/// 卡片正文窗口的计算结果。
struct ToolBodyWindow {
    /// 要渲染的源行：`(源行号, 文本)`，已按渲染顺序排好（首窗在前、尾窗在后）。
    lines: Vec<(usize, String)>,
    /// 折叠标注的插入点与行数：`(lines 下标, 被折叠的源行数)`；`None` = 不标注。
    fold: Option<(usize, usize)>,
}

/// 按 W-02 裁决切出卡片正文窗口（详见 [`TOOL_BODY_EDGE`]）。
fn tool_body_window(text: &str, running: bool) -> ToolBodyWindow {
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    if running {
        let start = total.saturating_sub(TOOL_BODY_RUNNING_TAIL);
        return ToolBodyWindow {
            lines: (start..total).map(|i| (i, all[i].to_string())).collect(),
            fold: None,
        };
    }
    if total <= TOOL_BODY_EDGE * 2 {
        return ToolBodyWindow {
            lines: all
                .iter()
                .enumerate()
                .map(|(i, s)| (i, (*s).to_string()))
                .collect(),
            fold: None,
        };
    }
    let mut lines: Vec<(usize, String)> = all
        .iter()
        .take(TOOL_BODY_EDGE)
        .enumerate()
        .map(|(i, s)| (i, (*s).to_string()))
        .collect();
    let fold_at = lines.len();
    lines.extend(
        all.iter()
            .enumerate()
            .skip(total - TOOL_BODY_EDGE)
            .map(|(i, s)| (i, (*s).to_string())),
    );
    ToolBodyWindow {
        lines,
        fold: Some((fold_at, total - TOOL_BODY_EDGE * 2)),
    }
}

/// 折叠标注行：`…折叠 n 行…`（W-02 裁决的固定文案）。
fn tool_body_fold_line(prefix: &str, folded: usize) -> RenderLine {
    RenderLine::new()
        .span(prefix.to_string(), SpanStyle::Dim)
        .span(format!("…折叠 {folded} 行…"), SpanStyle::Dim)
}
/// 工具输出保留的尾部行数（已由折叠逻辑替代，保留作历史阈值参考）。
/// 单元格截断宽度。
const ARG_PREVIEW: usize = 96;

/// 解析 hunk 头 `@@ -a,b +c,d @@` 取 a,c 起始行
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    // 形如 @@ -1,3 +1,4 @@ 可选 ,b
    let header = header.trim();
    if !header.starts_with("@@") {
        return None;
    }
    let inner = header.trim_start_matches('@').trim();
    // 取两段
    let mut parts = inner.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let old_num = old
        .trim_start_matches('-')
        .split(',')
        .next()?
        .parse::<u32>()
        .ok()?;
    let new_num = new
        .trim_start_matches('+')
        .split(',')
        .next()?
        .parse::<u32>()
        .ok()?;
    Some((old_num, new_num))
}
fn fmt_ln(n: u32, w: usize) -> String {
    // 右对齐 w 宽
    format!("{n:>width$}", width = w)
}

#[allow(dead_code)]
pub fn render_transcript(session: &SessionState, width: u16) -> Vec<RenderLine> {
    render_transcript_with_opts(session, width)
}

pub fn render_transcript_with_opts(session: &SessionState, width: u16) -> Vec<RenderLine> {
    let width = width.max(20) as usize;
    let mut lines: Vec<RenderLine> = Vec::new();

    for (turn_idx, turn) in session.timeline.turns.iter().enumerate() {
        lines.extend(render_turn(session, turn, turn_idx, width));
    }

    if session.timeline.turns.is_empty() {
        lines.push(RenderLine::new().span("（暂无回合——输入消息开始对话）", SpanStyle::Dim));
    }
    if let Some(banner) = render_banner(session) {
        lines.insert(0, banner);
    }
    lines
}

/// 头部横幅：更早回合是否被折叠 / 是否根本够不到（T-08）。
///
/// M4（T13a）：「查看更早」按钮的 TUI 形态——无鼠标基础设施，按钮 =
/// 显式键位提示 + 加载中状态反馈（PgUp 已可直接触发，见
/// [`crate::app::transcript_ops`] 的 `page_up`）。
pub(crate) fn render_banner(session: &SessionState) -> Option<RenderLine> {
    if session.loading_older {
        return Some(RenderLine::new().span("⋯ 正在加载更早回合…", SpanStyle::Dim));
    }
    if session.timeline.has_more {
        return Some(RenderLine::new().span("↑ 更早回合已折叠 — PgUp 加载更早", SpanStyle::Accent));
    }
    if session.timeline.truncated_before {
        // T-08：翻到头了，但历史并不止于此——服务端的物化窗口（timeline 从
        // messages 重建时只物化最近若干轮）覆盖不到开头，且当前**没有**深翻页
        // 接口能取到更早的回合。如实说明，别让用户以为「就这么多」而反复按 PgUp
        // 干等一个永远不会来的页。
        return Some(RenderLine::new().span(
            "⚠ 更早的回合未包含在本窗口（仅存于 daemon 归档，当前无法翻到）",
            SpanStyle::Warn,
        ));
    }
    None
}

/// 渲染**单个回合**（头部 + 正文 + 尾部空行）。
///
/// 这是分段渲染缓存的粒度单位：回合内任一块内容变化都会使其 `rev` 变化，
/// 进而使 [`turn_cache_key`] 变化 → 只重渲这一段。
/// 回合前置装饰：分隔头（状态/编号）+ offload 预览提示 + 用户输入。
///
/// 从 render_turn 内联段提取为独立函数：块级管线（`render/mod.rs`）与本文件的
/// 整回合渲染**必须**逐行一致（lock `block_cache_matches_full_render`），
/// 单一事实源是唯一可靠保证。
pub(crate) fn render_turn_pre(turn: &Turn, num: u64, width: usize) -> Vec<RenderLine> {
    let mut lines: Vec<RenderLine> = Vec::new();
    // ── 回合分隔 ──
    let state_tag = match turn.state {
        TimelineTurnState::Running => "… running".to_string(),
        TimelineTurnState::Completed => String::new(),
        TimelineTurnState::Failed => turn
            .failure
            .as_ref()
            .map(|f| format!("✗ {}", f.code))
            .unwrap_or_else(|| "✗ failed".into()),
        TimelineTurnState::Cancelled => "⊘ cancelled".into(),
    };
    // 编号用**稳定值**（见 `turn_number`）。
    //
    // ⚠ **不得**把 `turn_total()` 放进头部：它每新增一回合就变，而头部属于
    // 段内容 → 每段 key 都会变 → 增量复用彻底失效（实测 cap 边界重渲 21/30）。
    // 这就是「全局状态混进分段键」的反模式。总数改在会话信息行展示（不缓存）。
    let mut header = RenderLine::new().span("──── ", SpanStyle::Dim);
    header = header.span(format!("turn {num}"), SpanStyle::Dim);
    if !state_tag.is_empty() {
        let style = match turn.state {
            TimelineTurnState::Failed => SpanStyle::Error,
            TimelineTurnState::Cancelled => SpanStyle::Warn,
            _ => SpanStyle::Dim,
        };
        header = header.span(format!(" · {state_tag}"), style);
    }
    // §4.6 聚合（B1 载体）：丢弃有计数——思考 body 不驻留，但段/行数进头。
    if turn.thinking.segments > 0 {
        header = header.span(
            format!(
                " · 思考 {} 段/{} 行",
                turn.thinking.segments, turn.thinking.lines
            ),
            SpanStyle::Dim,
        );
    }
    lines.push(header);

    // T-06：offload 后常驻内存里只剩「预览壳」（正文截到 512 字符、
    // tool.output/diff 清空）。不说的话用户会把残缺内容当成完整回合——
    // 与 B1「丢弃必须可见」同一设计原则。
    if turn.offloaded {
        lines.push(
            RenderLine::new()
                .span("  ◌ ", SpanStyle::Warn)
                .span("已归档：以下内容为预览", SpanStyle::Warn),
        );
    }

    // ── 用户输入 ──
    if !turn.user_text.is_empty() {
        let wrapped = wrap_text(&turn.user_text, width.saturating_sub(2));
        for (i, seg) in wrapped.into_iter().enumerate() {
            let mut line = RenderLine::new();
            line = line.span(if i == 0 { "❯ " } else { "  " }, SpanStyle::Accent);
            line = line.span(seg, SpanStyle::User);
            lines.push(line);
        }
    }
    lines
}

/// 动画字形出口（plan §3.3 动画出带；锁 8）。
///
/// 共用渲染器（[`render_block_lines`] 一族是新旧路径的单源）按此出口决定
/// 帧变字形的去向：
/// - [`AnimSink::Bake`]：字形直接烘焙进行内——旧 `refresh_segments_at` 路径
///   用（其缓存键含 `frame_now()`，动画段每帧重渲，行为与 M1 前一致）；
/// - [`AnimSink::Slots`]：行内只写**占位空格**，字形坐标记入 [`AnimSlot`]，
///   draw 期按当前帧覆盖 cell（块键无帧号 → 帧推进零 rebuild，锁 4）。
///
/// 占位纪律（reviewer §四.2 / plan §3.3）：槽位 cell 必须是渲染期预留的
/// 占位空格；字形一律宽 1 = 占位宽 1，几何与烘焙模式逐 cell 对齐——draw
/// 只覆盖这些 cell，绝不覆盖可能压着 CJK 半边的位置（锁 8）。
pub(crate) enum AnimSink {
    Bake,
    Slots(Vec<AnimSlot>),
}

impl AnimSink {
    /// 槽位字符：Bake → 当前帧字形；Slots → 占位空格并记录坐标。
    /// `row`/`col` 为块内坐标（col 是该 cell 的起始显示列）。
    fn cell(&mut self, kind: AnimKind, row: u16, col: u16) -> &'static str {
        match self {
            AnimSink::Bake => match kind {
                AnimKind::Spinner => crate::app::anim::spinner_glyph(crate::app::anim::frame_now()),
                AnimKind::Thinking => {
                    crate::app::anim::thinking_glyph(crate::app::anim::frame_now())
                }
                AnimKind::Cursor => "▌",
                // 无生产者（见 AnimKind 文档）；Bake 下不可达。
                AnimKind::ProgressIndeterminate => "█",
            },
            AnimSink::Slots(out) => {
                out.push(AnimSlot { row, col, kind });
                " "
            }
        }
    }

    /// 取走槽位（Bake → 空）。
    pub(crate) fn into_vec(self) -> Vec<AnimSlot> {
        match self {
            AnimSink::Bake => Vec::new(),
            AnimSink::Slots(v) => v,
        }
    }
}

/// 单个内容块 → 行（Text/Reasoning/Tool/Notice 四分派）。
///
/// M2（D1）：**Reasoning 恒 0 行**——思考退出 transcript 管线，活动回合的 body
/// 由 Ctrl+T 浮层回放（§4.5），封口后聚合进回合头（§4.6）+ ActivityBar 承接实时。
///
/// W-02：卡片级展开态（`expanded_tools`）已删除，正文窗口由
/// [`tool_body_window`] 唯一决定。
pub(crate) fn render_block_lines(
    block: &Block,
    width: usize,
    anim: &mut AnimSink,
) -> Vec<RenderLine> {
    let mut lines: Vec<RenderLine> = Vec::new();
    match block.kind {
        TimelineBlockKind::Text => {
            push_text_block(&mut lines, &block.text, width, block.is_streaming(), anim);
        }
        TimelineBlockKind::Reasoning => {}
        TimelineBlockKind::Tool => {
            if let Some(tool) = &block.tool {
                // §4.7 分层：T1 工具永远单行（信息从 args 提炼），无 body。
                if crate::app::render_transcript::is_t1_tool(&tool.name) {
                    lines.push(crate::app::render_transcript::render_t1_line(
                        &tool.name,
                        tool.args_json.as_deref(),
                        tool.display.as_ref().and_then(|d| d.summary.as_deref()),
                        width,
                    ));
                    return lines;
                }
                push_tool_card_anim(&mut lines, tool, width, anim);
            }
        }
        TimelineBlockKind::Notice => {
            for seg in wrap_text(&block.text, width.saturating_sub(2)) {
                lines.push(
                    RenderLine::new()
                        .span("· ", SpanStyle::Dim)
                        .span(seg, SpanStyle::Dim),
                );
            }
        }
    }
    lines
}

/// 回合后置装饰：失败详情 + 尾部空行（无条件，保持旧行为）。
pub(crate) fn render_turn_post(failure: Option<&TimelineFailure>, width: usize) -> Vec<RenderLine> {
    let mut lines: Vec<RenderLine> = Vec::new();
    if let Some(f) = failure {
        for seg in wrap_text(
            &format!("{}: {}", f.code, f.message),
            width.saturating_sub(4),
        ) {
            lines.push(
                RenderLine::new()
                    .span("  ✗ ", SpanStyle::Error)
                    .span(seg, SpanStyle::Error),
            );
        }
    }
    lines.push(RenderLine::new());
    lines
}

fn render_turn(
    session: &SessionState,
    turn: &Turn,
    turn_idx: usize,
    width: usize,
) -> Vec<RenderLine> {
    let num = session.timeline.turn_number(turn_idx);
    let mut lines = render_turn_pre(turn, num, width);
    // §4.2 运行组：与管线 `flush_tool_group` 同语义（锁 1 的等价口径前提）——
    // 连续 T2 工具块折叠为组行（最后 Failed 卡内联），展开态为卡片列表。
    for round in &turn.rounds {
        let mut group: Vec<&Block> = Vec::new();
        let group_expanded = session
            .expanded_groups
            .contains(&(turn.turn_id.clone(), round.round_num));
        let flush = |group: &mut Vec<&Block>, lines: &mut Vec<RenderLine>| {
            if group.is_empty() {
                return;
            }
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
                for (i, _b) in group.iter().enumerate() {
                    if Some(i) == last_failed {
                        continue; // 失败例外：组行后内联
                    }
                    if i == 0 {
                        lines.push(render_group_line(&states, group_expanded, width));
                    }
                    // 中间块折叠态零行
                }
                if let Some(i) = last_failed {
                    let b = group[i];
                    if let Some(tool) = &b.tool {
                        let mut sink = AnimSink::Bake;
                        push_tool_card_anim(lines, tool, width, &mut sink);
                    }
                }
            } else {
                for b in group.iter() {
                    if let Some(tool) = &b.tool {
                        let mut sink = AnimSink::Bake;
                        push_tool_card_anim(lines, tool, width, &mut sink);
                    }
                }
            }
            group.clear();
        };
        for block in &round.blocks {
            let is_groupable = block.kind == TimelineBlockKind::Tool
                && block.tool.as_ref().is_some_and(|t| !is_t1_tool(&t.name));
            if is_groupable {
                group.push(block);
                continue;
            }
            flush(&mut group, &mut lines);
            lines.extend(render_block_lines(block, width, &mut AnimSink::Bake));
        }
        flush(&mut group, &mut lines);
    }
    lines.extend(render_turn_post(turn.failure.as_ref(), width));
    lines
}

// ── 分段渲染缓存的键 ────────────────────────────────────────────────

pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
pub(crate) const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[inline]
pub(crate) fn h_u64(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(FNV_PRIME);
}

#[inline]
pub(crate) fn h_str(h: &mut u64, s: &str) {
    for b in s.as_bytes() {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
    // 长度参与：防止 "ab"+"c" 与 "a"+"bc" 拼接后同哈希。
    h_u64(h, s.len() as u64);
}

/// Banner 段的缓存键：loading_older / has_more / truncated_before / 宽度。
pub(crate) fn banner_cache_key(session: &SessionState, width: u16) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x6261_6e6e); // "bann"
    h_u64(&mut h, u64::from(session.loading_older));
    h_u64(&mut h, u64::from(session.timeline.has_more));
    h_u64(&mut h, u64::from(session.timeline.truncated_before));
    h_u64(&mut h, u64::from(width));
    h
}

fn push_text_block(
    lines: &mut Vec<RenderLine>,
    text: &str,
    width: usize,
    streaming: bool,
    anim: &mut AnimSink,
) {
    if streaming {
        // 流式：纯文本低开销，避免半截 markdown 抖动与 syntect 重算。
        // 光标必须以 ▌ 参与**折行输入**（wrap_text 会丢弃换行点的行尾空格，
        // 若以占位空格折行，两种出口的行数会分叉）；Slots 出口在折行产物上
        // 把末行末尾的 ▌ 就地换成同宽占位空格并记槽位（plan §3.3 占位纪律）。
        let shown = format!("{text}▌");
        let wrapped = wrap_text(&shown, width);
        let last = wrapped.len().saturating_sub(1);
        for (i, seg) in wrapped.into_iter().enumerate() {
            let is_cursor = i == last && seg.ends_with('▌');
            let shown = match (is_cursor, &mut *anim) {
                (true, AnimSink::Slots(out)) => {
                    let col = unicode_width::UnicodeWidthStr::width(seg.as_str()) - 1; // ▌ 宽 1，行尾
                    let mut seg = seg;
                    seg.pop(); // ▌ → 同宽占位
                    seg.push(' ');
                    out.push(AnimSlot {
                        row: lines.len() as u16,
                        col: col as u16,
                        kind: AnimKind::Cursor,
                    });
                    seg
                }
                (_, AnimSink::Bake) | (false, AnimSink::Slots(_)) => seg,
            };
            lines.push(RenderLine::plain(shown));
        }
        return;
    }
    if crate::app::markdown::is_markdown(text) {
        // 落盘后富化：表格/代码块栅格化，保持单 Paragraph 滚动链路
        let mut md_lines = crate::app::markdown::render_markdown(text, width);
        // 批处理保护：单块超 [`MD_BLOCK_LINE_CAP`] 行即截断（防单块渲染把
        // 预折行缓存撑爆），并在末尾如实标注省略了多少行。
        if md_lines.len() > MD_BLOCK_LINE_CAP {
            let omitted = md_lines.len() - MD_BLOCK_LINE_CAP;
            md_lines.truncate(MD_BLOCK_LINE_CAP);
            md_lines.push(
                RenderLine::new().span(format!("  （内容省略 {omitted} 行）"), SpanStyle::Dim),
            );
        }
        lines.extend(md_lines);
        return;
    }
    for seg in wrap_text(text, width) {
        lines.push(RenderLine::plain(seg));
    }
}

// ── §4.7 渲染分层：T1 调用行 / T2 可展开卡 ─────────────────────────

/// T1 工具词表：**永远单行**、无 body（§4.7）——信息从 args 提炼。
/// 这些工具要么无输出（todo_list 空转）、要么结果由专属面板/活动区承载。
pub(crate) fn is_t1_tool(name: &str) -> bool {
    matches!(
        name,
        "todo_write" | "todo_update" | "todo_list" | "skills" | "spawn_subagent" | "web_search"
    )
}

/// T1 调用行：`⚙ {name} · {摘要}`（§4.7）。
///
/// M3（T12）：摘要优先取 `display.summary`（投影规范摘要）；
/// display=None（H16 老会话）时回退 args 提炼预览。
pub(crate) fn render_t1_line(
    name: &str,
    args_json: Option<&str>,
    display_summary: Option<&str>,
    width: usize,
) -> RenderLine {
    let mut text = format!("  {} {name}", tool_icon(name));
    let summary = display_summary
        .filter(|s| !s.is_empty())
        .map(|s| s.replace('\n', " "));
    if let Some(summary) = summary {
        text.push_str(" · ");
        text.push_str(&summary);
    } else if let Some(a) = args_json {
        let preview = format_args_preview(a);
        if !preview.is_empty() {
            text.push_str(" · ");
            text.push_str(&preview);
        }
    }
    let _ = width; // 单行不折；超宽由 draw 期截断（终端天然裁剪）
    RenderLine::new().span(text, SpanStyle::Dim)
}

/// 测试构造：投影 display（T12 锁用）。
#[cfg(test)]
pub(crate) fn test_display(
    summary: Option<&str>,
    body: Option<qaqh_client::TimelineToolBody>,
) -> qaqh_client::TimelineToolDisplay {
    qaqh_client::TimelineToolDisplay {
        summary: summary.map(str::to_string),
        diff: None,
        header: None,
        body,
        metrics: None,
        outcome: None,
    }
}

/// §4.2 运行组折叠行：`┃ ⚙ N tool calls · a✓ b✗ c⊘ · F7 展开`。
///
/// 计数来自组内各卡的终态（Running 算「运行中」不计入三态）。
pub(crate) fn render_group_line(
    states: &[TimelineToolState],
    expanded: bool,
    width: usize,
) -> RenderLine {
    let ok = states
        .iter()
        .filter(|s| **s == TimelineToolState::Succeeded)
        .count();
    let failed = states
        .iter()
        .filter(|s| **s == TimelineToolState::Failed)
        .count();
    let cancelled = states
        .iter()
        .filter(|s| **s == TimelineToolState::Cancelled)
        .count();
    let running = states
        .iter()
        .filter(|s| **s == TimelineToolState::Running)
        .count();
    let mut text = format!("  ┃ ⚙ {} tool calls", states.len());
    if ok > 0 {
        text.push_str(&format!(" · {ok}✓"));
    }
    if failed > 0 {
        text.push_str(&format!(" · {failed}✗"));
    }
    if cancelled > 0 {
        text.push_str(&format!(" · {cancelled}⊘"));
    }
    if running > 0 {
        text.push_str(&format!(" · {running} 运行中"));
    }
    let _ = width;
    let hint = if expanded { "F7 收起" } else { "F7 展开" };
    text.push_str(&format!(" · {hint}"));
    RenderLine::new().span(text, SpanStyle::Dim)
}

/// opencode 式工具图标（对齐 `toolDisplay` 集合） `packages/tui/src/routes/session/index.tsx:2638`
pub(crate) fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" | "exec" | "shell" | "pwsh" | "powershell" => "$",
        "write" => "←",
        "edit" => "←",
        "glob" => "✱",
        "grep" => "✱",
        "read" => "→",
        "web_fetch" | "webfetch" => "%",
        "web_search" | "websearch" => "◈",
        "apply_patch" => "%",
        "todo" => "⚙",
        "ask" | "question" => "→",
        "skill" => "→",
        "task" => "│",
        _ => "⚙",
    }
}

/// 从 args_json 提炼可读预览（仅保留 primitives，去除 filePath 重复等）。
///
/// M3（T12）gate 豁免：display 投影覆盖后本函数仅剩两个合法入口——
/// ① T1 行 / T2 参段的 **H16 回退**（display=None 老会话，见
/// `render_t1_line` 与 `push_tool_card` 的 skip_args_preview 条件）；
/// ② 无结构化 body 的 display 卡的参数补充。gate 口径（禁止从 args JSON
/// 提炼用户可见内容）在此两处之外仍生效。
fn format_args_preview(args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        let one = args_json.replace('\n', " ");
        return if one.chars().count() > ARG_PREVIEW {
            format!("{}…", one.chars().take(ARG_PREVIEW).collect::<String>())
        } else {
            one
        };
    };
    if let serde_json::Value::Object(map) = v {
        let mut parts: Vec<String> = Vec::new();
        for (k, val) in map.iter() {
            if matches!(k.as_str(), "filePath" | "path" | "file_path") {
                continue; // 路径由标题单独展示，避免重复
            }
            match val {
                serde_json::Value::String(s) if !s.is_empty() => {
                    let short = if s.chars().count() > 40 {
                        format!("{}…", s.chars().take(40).collect::<String>())
                    } else {
                        s.clone()
                    };
                    parts.push(format!("{k}={short}"));
                }
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                    parts.push(format!("{k}={val}"))
                }
                _ => {}
            }
            if parts.len() >= 3 {
                break;
            }
        }
        if parts.is_empty() {
            return String::new();
        }
        let joined = format!("[{}]", parts.join(", "));
        if joined.chars().count() > ARG_PREVIEW {
            format!("{}…", joined.chars().take(ARG_PREVIEW).collect::<String>())
        } else {
            joined
        }
    } else {
        String::new()
    }
}

fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "exec" | "shell" | "pwsh" | "powershell")
}

/// 运行期体量标注（契约 §5.1 / §6 P2）：`↓ 12.3 KB` + 非 stdout 流标注。
/// 仅 Running 态显示——终态以 metrics 尾注（output_bytes）为准，不重复。
fn progress_bytes_note(tool: &crate::app::timeline_model::ToolCard) -> Option<String> {
    if tool.state != TimelineToolState::Running || tool.progress_bytes_total == 0 {
        return None;
    }
    let mut s = format!(" ↓ {}", human_bytes(tool.progress_bytes_total));
    if let Some(stream) = tool
        .progress_stream
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "stdout")
    {
        s.push_str(" · ");
        s.push_str(stream);
    }
    Some(s)
}

/// 人类可读字节（契约 P2 展示口径；export.rs 复用同一格式）。
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// 专属面板已完整承载结果的工具：transcript 只留一行调用轨迹。
///
/// 判据是“UI 是否存在权威的专属投影”，而非输出是否好看。工具名对齐后端
/// `qaqh-workspace/src/registration.rs` 的正式词表（18 个内置工具；
/// `spawn_subagent` 由 qaqh-subagent 经 extra_registrars 注入）。
///
/// - `todo_write`/`todo_update`/`todo_list` → 右侧 workspace 面板
///   （`DashboardSnapshot` 驱动：计数/进度条/聚焦项/最近改动，事件驱动即时推送）；
/// - `ask` → 交互弹窗（答案经 `InteractionResolved` 归入会话）；
/// - `skills` → 技能面板 + 状态栏；activate 后指令以 `skill_context_envelope`
///   系统消息注入，结果本体只是 `{status:"ok",resources:[...]}` 回执；
/// - `spawn_subagent` → 标签栏 `↳N` 徽标 + Ctrl+↑ 只读观测视图。
///
/// 这些工具的 `output` 是给模型的机器回执（`json_ok` 形态，含 `timeis`/`status`
/// 等与用户无关的时间戳字段），用户在别处已看到更好的版本，再打一遍纯属噪声。
///
/// 注意：历史/自研前端里的 `todo` 单名工具已在 Todo v3 拆分为三件套，
/// 后端词表中不存在 `todo`；此处仍兼容旧名以免历史 timeline 回放出现漏网。
pub fn is_panel_owned_tool(name: &str) -> bool {
    matches!(
        name,
        "todo_write" | "todo_update" | "todo_list" | "todo" | "ask" | "skills" | "spawn_subagent"
    )
}

/// 状态专用语：让“调用了什么”在无输出时依然可读。
fn panel_owned_note(tool: &crate::app::timeline_model::ToolCard) -> &'static str {
    match tool.name.as_str() {
        "ask" => "已回答",
        "spawn_subagent" => "已派发",
        "skills" => "已载入",
        "todo_list" => "已读取清单",
        _ => "已更新面板",
    }
}

/// 尝试将工具的 `output` JSON 外壳剥离，仅取内部 `output` 字段。
/// 若不是 ExecOutput JSON，则回退为原文；不引入额外错误提示，保持视觉干净。
///
/// M3（T12）gate 豁免：H16 专用——display=None（老 daemon/未投影工具）时
/// 的 legacy 输出剥离；display=Some 走 `TimelineToolBody` 投影，不经过此处。
fn extract_shell_output_text(raw: &str) -> Option<String> {
    let s = raw.trim();
    if !s.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    // ExecOutput 形状：{status, command, exit_code, output, truncated, timed_out, cancelled, process_id}
    // backgrounded 时 output 为内层 JSON 字符串，仍以字符串形式透出
    if let Some(out) = obj.get("output").and_then(|x| x.as_str()) {
        return Some(out.to_string());
    }
    // 非字符串 output（如意外对象）则序列化回文本
    if let Some(out) = obj.get("output")
        && !out.is_null()
    {
        // 保持可读：若是对象则 pretty-free json
        if out.is_string() {
            return Some(out.as_str().unwrap_or("").to_string());
        } else {
            return Some(out.to_string());
        }
    }
    None
}

/// v2 exec display: keep stdout/stderr distinguishable when both are present.
fn render_streams_body(stdout: &str, stderr: &str) -> Option<String> {
    let stdout = stdout.trim_end();
    let stderr = stderr.trim_end();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => None,
        (false, true) => Some(stdout.to_string()),
        (true, false) => Some(format!("stderr:\n{stderr}")),
        (false, false) => Some(format!("stdout:\n{stdout}\nstderr:\n{stderr}")),
    }
}

/// 工具进度被截断时的可见标注（B1「丢弃必须可见」）。
///
/// `ToolCard::progress_truncated` 为真 = **进度缓冲的前段已被丢弃**（wire 语义见
/// `qaqh-domain/src/timeline.rs:120`：*"True once the writer discarded an older
/// prefix of `progress`"*；本侧在 `apply_bash_progress` / `retain_utf8_tail`
/// 超 `MAX_PROGRESS_LEN`(8KB) 时同样丢头保尾，见 `timeline_model.rs:13/154/585`）。
/// 不说的话用户会把「只剩尾巴」当成完整输出——与回合级的
/// `◌ 已归档：以下内容为预览`（见 `render_turn`）同一条设计原则。
///
/// 措辞不用方位词「以下」：回合级那条的「以下」指**可见内容**，这里指**缺口**
/// （丢掉的在视野之外），同词异义容易读成「下面这段是残缺的」。
///
/// 每张卡**只出一行**、内容不随帧变化（不刷屏，稳定可读）；位置在正文与折叠
/// hint **之后**（footer 挨着 footer，读者不必跨正文拼读）；且仅在进度**确实
/// 上屏**时给出（否则就是在为看不见的东西报警）。
const PROGRESS_TRUNCATED_MARK: &str = "进度前段已丢弃（仅保留末尾）";

/// 进度截断标注行（前缀沿用卡片内的注记风格：`┃ ✗` / `┃ ⚠` → `┃ ◌`）。
///
/// ◌ 只由前缀给出：曾把 ◌ 也写进 `PROGRESS_TRUNCATED_MARK`，渲染成
/// ` ┃ ◌ ◌ 进度前段已丢弃…`（`truncation_mark_renders_single_glyph` 锁住）。
fn progress_truncated_line(is_block: bool) -> RenderLine {
    let pfx = if is_block { " ┃ ◌ " } else { "    ◌ " };
    RenderLine::new()
        .span(pfx, SpanStyle::Warn)
        .span(PROGRESS_TRUNCATED_MARK, SpanStyle::Warn)
}

/// display 投影取值助手（Phase C）。全部按域回落 None，调用方各自回退旧字段。
fn display_path_title(d: &TimelineToolDisplay) -> Option<String> {
    match d.header.as_ref()? {
        TimelineToolHeader::Path { path, .. } => Some(path.clone()),
        _ => None,
    }
}

fn display_shell_command(d: &TimelineToolDisplay) -> Option<String> {
    match d.header.as_ref()? {
        TimelineToolHeader::Shell { command } => Some(command.clone()),
        _ => None,
    }
}

#[cfg(test)]
fn push_tool_card(
    lines: &mut Vec<RenderLine>,
    tool: &crate::app::timeline_model::ToolCard,
    width: usize,
) {
    // 测试专用入口（旧直呼签名）：字形烘焙，行为与 M1 前一致。
    let mut anim = AnimSink::Bake;
    push_tool_card_anim(lines, tool, width, &mut anim);
}

fn push_tool_card_anim(
    lines: &mut Vec<RenderLine>,
    tool: &crate::app::timeline_model::ToolCard,
    width: usize,
    anim: &mut AnimSink,
) {
    // W-02（2026-09-20 裁决）：卡片级展开态已删除——`expanded_raw` /
    // `is_default_expanded` / 24 行帽全部取消，正文窗口由 [`tool_body_window`]
    // 唯一决定；F7 只剩 §4.2 的运行组折叠语义。
    // 动画帧：Running 时用八帧 braille 转轮（200ms/帧，Tick 驱动重绘；帧源=墙钟，无状态）
    let (icon_raw, base_style, is_running) = match tool.state {
        TimelineToolState::Prepared => (tool_icon(&tool.name), SpanStyle::Dim, false),
        TimelineToolState::Running => (
            crate::app::anim::spinner_glyph(crate::app::anim::frame_now()),
            SpanStyle::ToolRun,
            true,
        ),
        TimelineToolState::Succeeded => ("●", SpanStyle::ToolOk, false),
        TimelineToolState::Failed => ("✗", SpanStyle::ToolFail, false),
        // 后端把这两个列为**终态但非失败**：取消是「无输出或被中断」，后台化是
        // 「调用已返回、任务仍在跑」。共用警告色而非失败色——把它们画成失败会
        // 谎报工具出错（这正是后端单独分出这两个变体的原因）。
        TimelineToolState::Cancelled => ("⊘", SpanStyle::Warn, false),
        TimelineToolState::Backgrounded => ("◐", SpanStyle::Warn, false),
    };
    // 权限覆盖色（对齐 opencode InlineTool fg: warning）
    let (icon, style) = if tool.permission.is_some() {
        (icon_raw, SpanStyle::Warn)
    } else if tool.failure.is_some() && tool.state == TimelineToolState::Failed {
        (icon_raw, SpanStyle::ToolFail)
    } else {
        (icon_raw, base_style)
    };

    // ── 标题行：InlineTool 形态（单行 icon + name + 路径/摘要） `opencode InlineToolRow:1967`
    // Phase C：display 投影优先（契约 §3.3）；None → 旧字段回退（H16）。
    let d = tool.display.as_ref();
    let path: Option<String> = d.and_then(display_path_title);
    let exec_summary: Option<String> = d.and_then(display_shell_command);
    let header_extra = if let Some(p) = &path {
        let short = crate::app::truncate_str(p, 36);
        format!(" {short}")
    } else if let Some(cmd) = exec_summary.as_deref().filter(|s| !s.is_empty()) {
        let one = cmd.replace('\n', " ").chars().take(64).collect::<String>();
        format!(" {one}")
    } else if let Some(summary) = d
        .and_then(|x| x.summary.as_deref())
        .filter(|s| !s.is_empty())
        .or(tool.summary.as_deref().filter(|s| !s.is_empty()))
    {
        let one = summary
            .replace('\n', " ")
            .chars()
            .take(48)
            .collect::<String>();
        format!(" {one}")
    } else {
        String::new()
    };

    // Block 判定：含 diff / 长输出 / 诊断即用 BlockTool 左线
    let has_diff = tool.diff.as_deref().is_some_and(|d| !d.trim().is_empty());
    let output_len = tool
        .output
        .as_deref()
        .map(|s| s.lines().count())
        .unwrap_or(0)
        + tool.progress.lines().count();
    let display_body = d.and_then(|d| d.body.as_ref());
    let is_block = has_diff
        || output_len > 4
        || tool.state == TimelineToolState::Running && !tool.progress.is_empty()
        || matches!(
            display_body,
            Some(TimelineToolBody::Shell { .. })
                | Some(TimelineToolBody::Streams { .. })
                | Some(TimelineToolBody::Diff { .. })
                | Some(TimelineToolBody::Text { .. })
        );

    if is_block {
        // BlockTool 标题：`# name path` 灰底左线（复刻 `BlockTool 1995 border left ┃ bg panel`）
        let title = if let Some(p) = path {
            let display = crate::app::truncate_str(&p, width.saturating_sub(10));
            format!("# {} {display}", tool.name)
        } else if let Some(cmd) = exec_summary.as_deref().filter(|s| !s.is_empty()) {
            let display = crate::app::truncate_str(cmd, width.saturating_sub(10));
            format!("# {} {display}", tool.name)
        } else {
            format!("# {}", tool.name)
        };
        // 左线用 ┃（SplitBorder vertical）
        let mut title_line = RenderLine::new()
            .span(" ┃ ", SpanStyle::Dim)
            .span(title, SpanStyle::Dim);
        // Phase C：display.summary 进 Block 标题行（inline 形态经 header_extra 已覆盖；
        // Block 形态此前会把它丢掉——skills resource 摘要即此缺口）。
        if let Some(sum) = d
            .and_then(|x| x.summary.as_deref())
            .filter(|s| !s.is_empty())
        {
            title_line = title_line.span(
                format!(" {}", crate::app::truncate_str(sum, 48)),
                SpanStyle::Plain,
            );
        }
        lines.push(title_line);
        // 状态行：icon + 状态 + 动画尾
        let state_label = match tool.state {
            TimelineToolState::Running => " running",
            TimelineToolState::Succeeded => " completed",
            TimelineToolState::Failed => " failed",
            TimelineToolState::Prepared => " prepared",
            // 见上方图标处：终态但非失败，标签也必须分开，否则用户看到的
            // 是一次并不存在的工具错误。
            TimelineToolState::Cancelled => " cancelled",
            TimelineToolState::Backgrounded => " backgrounded",
        };
        // Running 的 icon 是帧变字形 → 动画出带（Bake 烘焙 / Slots 占位+槽位；
        // col = 推入时的起始显示列，字形宽 1 = 占位宽 1，几何不变）。
        let state_line = RenderLine::new().span(" ┃ ", SpanStyle::Dim);
        let icon_text = if is_running {
            let col = state_line.display_width();
            format!(
                "{} ",
                anim.cell(AnimKind::Spinner, lines.len() as u16, col as u16)
            )
        } else {
            format!("{icon} ")
        };
        lines.push({
            let mut state_row = state_line
                .span(icon_text, style)
                .span(tool.name.clone(), SpanStyle::Accent)
                .span(state_label, style);
            if let Some(note) = progress_bytes_note(tool) {
                state_row = state_row.span(note, SpanStyle::Dim);
            }
            state_row.span(if is_running { " ⋯" } else { "" }, SpanStyle::Dim)
        });
    } else {
        // InlineTool 单行
        let state_suffix = match tool.state {
            TimelineToolState::Running => " ⋯",
            _ => "",
        };
        let mut header = RenderLine::new().span("  ", SpanStyle::Dim);
        let icon_text = if is_running {
            let col = header.display_width();
            format!(
                "{} ",
                anim.cell(AnimKind::Spinner, lines.len() as u16, col as u16)
            )
        } else {
            format!("{icon} ")
        };
        header = header.span(icon_text, style).span(
            tool.name.clone(),
            if tool.permission.is_some() {
                SpanStyle::Warn
            } else {
                SpanStyle::Accent
            },
        );
        if !header_extra.trim().is_empty() {
            header = header.span(
                header_extra.clone(),
                if tool.state == TimelineToolState::Succeeded {
                    SpanStyle::Dim
                } else {
                    SpanStyle::Plain
                },
            );
        }
        header = header.span(state_suffix, SpanStyle::Dim);
        if let Some(note) = progress_bytes_note(tool) {
            header = header.span(note, SpanStyle::Dim);
        }
        // 专属面板工具：用状态专用语补齐语义（无输出时标题不至于干瘪）
        if is_panel_owned_tool(&tool.name) {
            let note = panel_owned_note(tool);
            let style = if tool.state == TimelineToolState::Failed {
                SpanStyle::ToolFail
            } else {
                SpanStyle::Dim
            };
            header = header.span(format!(" · {note}"), style);
        }
        // 权限/失败的额外内联提示
        if tool.permission.is_some() {
            header = header.span(" ⚠ 需授权", SpanStyle::Warn);
        } else if let Some(err) = &tool.failure {
            let hint = crate::app::truncate_str(&err.message, 28);
            header = header.span(format!(" ✗ {hint}"), SpanStyle::ToolFail);
        }
        lines.push(header);
        // Inline 下若无 Block，不再展开 diff/输出，返回
        if !has_diff && output_len == 0 && tool.args_json.is_none() {
            return;
        }
    }

    // ── 参预览（非路径部分）─ 对齐 opencode `input()` 过滤
    // exec 的命令已进入标题，此处再列 `[command=..., shell=...]` 属于重复。
    // M3（T12）：display 结构化 body（Shell/Diff/Text）已承载输出时，args 提炼段
    // 不再叠加（与投影双写收敛）；body=None / display=None（H16）保留作参数补充。
    let skip_args_preview = exec_summary.is_some() || display_body.is_some();
    if !skip_args_preview
        && let Some(args) = tool
            .args_json
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "{}")
    {
        let preview = format_args_preview(args);
        if !preview.is_empty() {
            let one_line = preview.replace('\n', " ");
            for seg in wrap_text(&one_line, width.saturating_sub(6)) {
                let prefix = if is_block { " ┃ ⌗ " } else { "    ⌗ " };
                lines.push(
                    RenderLine::new()
                        .span(prefix, SpanStyle::Dim)
                        .span(seg, SpanStyle::Dim),
                );
            }
        }
    }

    // ── Diff 块：行级着色 + 自适应 split/unified + 行号 gutter（opencode 2401/2595）
    let diff_src = display_body.and_then(|b| match b {
        TimelineToolBody::Diff { unified, .. } => Some(unified.as_str()),
        _ => None,
    });
    if let Some(diff) = diff_src.or(tool.diff.as_deref()) {
        let added = diff
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        let removed = diff
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count();
        let prefix = if is_block { " ┃ Δ " } else { "    Δ " };
        lines.push(
            RenderLine::new()
                .span(prefix, SpanStyle::Dim)
                .span(format!("+{added}"), SpanStyle::DiffAdd)
                .span(" ", SpanStyle::Dim)
                .span(format!("−{removed}"), SpanStyle::DiffDel)
                .span(
                    if width > 120 && is_block {
                        "  (split)"
                    } else {
                        ""
                    },
                    SpanStyle::Dim,
                ),
        );
        if width > 120 && is_block {
            // split 双栏：左旧/右新 各含 3宽行号 + 内容
            let inner = width.saturating_sub(7);
            let left_w = inner / 2;
            let right_w = inner - left_w;
            let ln_w = 3usize; // 行号宽
            let left_content_w = left_w.saturating_sub(ln_w + 1);
            let right_content_w = right_w.saturating_sub(ln_w + 1);
            let mut pending_removed: Vec<(String, u32)> = Vec::new();
            let mut pending_added: Vec<(String, u32)> = Vec::new();
            let mut shown = 0usize;
            let mut old_ln: u32 = 1;
            let mut new_ln: u32 = 1;
            for raw in diff.lines().take(80) {
                if raw.starts_with("---") || raw.starts_with("+++") {
                    // 刷 pending
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 {
                                break;
                            }
                            let (l_txt, l_no) = pending_removed
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 {
                                fmt_ln(l_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            let r_num = if r_no != 0 {
                                fmt_ln(r_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        format!("{:<width$}", l_txt, width = left_content_w),
                                        if pending_removed.get(i).is_some() {
                                            SpanStyle::DiffDel
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    )
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        r_txt,
                                        if pending_added.get(i).is_some() {
                                            SpanStyle::DiffAdd
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    ),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(raw.to_owned(), SpanStyle::Dim),
                    );
                } else if raw.starts_with("@@") {
                    if let Some((o, n)) = parse_hunk_header(raw) {
                        old_ln = o;
                        new_ln = n;
                    }
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 {
                                break;
                            }
                            let (l_txt, l_no) = pending_removed
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 {
                                fmt_ln(l_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            let r_num = if r_no != 0 {
                                fmt_ln(r_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        format!("{:<width$}", l_txt, width = left_content_w),
                                        if pending_removed.get(i).is_some() {
                                            SpanStyle::DiffDel
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    )
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        r_txt,
                                        if pending_added.get(i).is_some() {
                                            SpanStyle::DiffAdd
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    ),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(raw.to_owned(), SpanStyle::Dim),
                    );
                } else if let Some(body) = raw.strip_prefix('-') {
                    pending_removed.push((body.to_owned(), old_ln));
                    old_ln += 1;
                } else if let Some(body) = raw.strip_prefix('+') {
                    pending_added.push((body.to_owned(), new_ln));
                    new_ln += 1;
                } else {
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 {
                                break;
                            }
                            let (l_txt, l_no) = pending_removed
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added
                                .get(i)
                                .map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n))
                                .unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 {
                                fmt_ln(l_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            let r_num = if r_no != 0 {
                                fmt_ln(r_no, ln_w)
                            } else {
                                "   ".into()
                            };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        format!("{:<width$}", l_txt, width = left_content_w),
                                        if pending_removed.get(i).is_some() {
                                            SpanStyle::DiffDel
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    )
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim)
                                    .span(" ", SpanStyle::Dim)
                                    .span(
                                        r_txt,
                                        if pending_added.get(i).is_some() {
                                            SpanStyle::DiffAdd
                                        } else {
                                            SpanStyle::Dim
                                        },
                                    ),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    if raw.trim().is_empty() {
                        old_ln += 1;
                        new_ln += 1;
                        continue;
                    }
                    let txt = raw.strip_prefix(' ').unwrap_or(raw);
                    let l_no = old_ln;
                    let r_no = new_ln;
                    old_ln += 1;
                    new_ln += 1;
                    let l = crate::app::truncate_str(txt, left_content_w);
                    let r = crate::app::truncate_str(txt, right_content_w);
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(fmt_ln(l_no, ln_w), SpanStyle::Dim)
                            .span(" ", SpanStyle::Dim)
                            .span(
                                format!("{:<width$}", l, width = left_content_w),
                                SpanStyle::Dim,
                            )
                            .span(" │ ", SpanStyle::Dim)
                            .span(fmt_ln(r_no, ln_w), SpanStyle::Dim)
                            .span(" ", SpanStyle::Dim)
                            .span(r, SpanStyle::Dim),
                    );
                    shown += 1;
                    if shown >= 60 {
                        break;
                    }
                }
                if shown >= 60 {
                    break;
                }
            }
            {
                let max = pending_removed.len().max(pending_added.len());
                for i in 0..max {
                    if shown >= 60 {
                        break;
                    }
                    let (l_txt, l_no) = pending_removed
                        .get(i)
                        .map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n))
                        .unwrap_or((String::new(), 0));
                    let (r_txt, r_no) = pending_added
                        .get(i)
                        .map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n))
                        .unwrap_or((String::new(), 0));
                    let l_num = if l_no != 0 {
                        fmt_ln(l_no, ln_w)
                    } else {
                        "   ".into()
                    };
                    let r_num = if r_no != 0 {
                        fmt_ln(r_no, ln_w)
                    } else {
                        "   ".into()
                    };
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(l_num, SpanStyle::Dim)
                            .span(" ", SpanStyle::Dim)
                            .span(
                                format!("{:<width$}", l_txt, width = left_content_w),
                                if pending_removed.get(i).is_some() {
                                    SpanStyle::DiffDel
                                } else {
                                    SpanStyle::Dim
                                },
                            )
                            .span(" │ ", SpanStyle::Dim)
                            .span(r_num, SpanStyle::Dim)
                            .span(" ", SpanStyle::Dim)
                            .span(
                                r_txt,
                                if pending_added.get(i).is_some() {
                                    SpanStyle::DiffAdd
                                } else {
                                    SpanStyle::Dim
                                },
                            ),
                    );
                    shown += 1;
                }
            }
            if diff.lines().count() > 80 {
                lines.push(RenderLine::new().span(
                    format!(" ┃   … {} 行未展示", diff.lines().count() - 80),
                    SpanStyle::Dim,
                ));
            }
        } else {
            // unified + 行号 gutter 3宽
            let mut shown = 0usize;
            let mut old_ln: u32 = 1;
            let mut new_ln: u32 = 1;
            for raw in diff.lines().take(80) {
                if raw.starts_with("---") || raw.starts_with("+++") {
                    lines.push(
                        RenderLine::new()
                            .span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim)
                            .span(raw.to_owned(), SpanStyle::Dim),
                    );
                } else if raw.starts_with("@@") {
                    if let Some((o, n)) = parse_hunk_header(raw) {
                        old_ln = o;
                        new_ln = n;
                    }
                    lines.push(
                        RenderLine::new()
                            .span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim)
                            .span(raw.to_owned(), SpanStyle::Dim),
                    );
                } else if let Some(txt) = raw.strip_prefix('+') {
                    let ln = fmt_ln(new_ln, 3);
                    new_ln += 1;
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(
                        RenderLine::new()
                            .span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim)
                            .span(ln, SpanStyle::Dim)
                            .span(" +", SpanStyle::DiffAdd)
                            .span(seg, SpanStyle::DiffAdd),
                    );
                } else if let Some(txt) = raw.strip_prefix('-') {
                    let ln = fmt_ln(old_ln, 3);
                    old_ln += 1;
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(
                        RenderLine::new()
                            .span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim)
                            .span(ln, SpanStyle::Dim)
                            .span(" -", SpanStyle::DiffDel)
                            .span(seg, SpanStyle::DiffDel),
                    );
                } else if !raw.trim().is_empty() {
                    let ln = fmt_ln(new_ln, 3);
                    old_ln += 1;
                    new_ln += 1;
                    let txt = raw.strip_prefix(' ').unwrap_or(raw);
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(
                        RenderLine::new()
                            .span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim)
                            .span(ln, SpanStyle::Dim)
                            .span("  ", SpanStyle::Dim)
                            .span(seg, SpanStyle::Dim),
                    );
                } else {
                    old_ln += 1;
                    new_ln += 1;
                }
                shown += 1;
                if shown >= 60 {
                    break;
                }
            }
            if diff.lines().count() > 80 {
                lines.push(RenderLine::new().span(
                    format!(
                        "{}   … {} 行未展示",
                        if is_block { " ┃ " } else { "    " },
                        diff.lines().count() - 80
                    ),
                    SpanStyle::Dim,
                ));
            }
        }
    }

    // ── 输出：shell 8 行流动 + JSON 剥壳，视觉一致无 [stderr] 前缀 ──
    // 专属面板工具（todo/ask/spawn_subagent）在标题行已给出状态，输出是给
    // 模型的机器回执，此处不重复投影；失败信息仍由下方 `tool.failure` 渲染。
    if is_panel_owned_tool(&tool.name) && tool.failure.is_none() {
        // 仍消费 permission/失败等收尾逻辑（见函数末尾）。
    } else if is_shell_tool(&tool.name) {
        let raw_output = tool.output.as_deref().unwrap_or("");
        let unwrapped = extract_shell_output_text(raw_output);
        let shell_meta: Option<(Option<i32>, bool, String)> = display_body.and_then(|b| match b {
            TimelineToolBody::Shell {
                exit_code,
                truncated,
                ..
            }
            | TimelineToolBody::Streams {
                exit_code,
                truncated,
                ..
            } => Some((*exit_code, *truncated, String::new())),
            _ => None,
        });
        // 正文来源在**取值处**一并确定，不用 `src == tool.progress` 事后做整串
        // 值比较：那种判据在 `src` 被任何规整（trim / 换行归一）后都会静默失配，
        // 标注随之消失（PR #18 二轮复审建议 2）。
        //
        // 截断标注只在进度**真的上屏**时给——工具已结束且拿到了完整 `output` 时，
        // progress 只是被取代的中间态，此时标注它「前段已丢弃」是噪音。
        let display_shell_output = display_body.and_then(|b| match b {
            TimelineToolBody::Shell { output, .. } => Some(output.clone()),
            TimelineToolBody::Streams { stdout, stderr, .. } => render_streams_body(stdout, stderr),
            _ => None,
        });
        let (src, src_from_progress) =
            if let Some(out) = display_shell_output.filter(|o| !o.trim().is_empty()) {
                (out, false)
            } else if tool.state == TimelineToolState::Running {
                if !tool.progress.trim().is_empty() {
                    (tool.progress.clone(), true)
                } else {
                    (unwrapped.clone().unwrap_or_default(), false)
                }
            } else if let Some(ref inner) = unwrapped {
                if !inner.trim().is_empty() {
                    (inner.clone(), false)
                } else if !tool.progress.trim().is_empty() {
                    (tool.progress.clone(), true)
                } else {
                    (String::new(), false)
                }
            } else if !tool.progress.trim().is_empty() {
                (tool.progress.clone(), true)
            } else if !raw_output.trim().is_empty() {
                (raw_output.to_string(), false)
            } else {
                (String::new(), false)
            };
        if !src.trim().is_empty() {
            // W-02 裁决：正文窗口由 [`tool_body_window`] 唯一决定——Running 取
            // 6 行尾窗；结束取 head3 + `…折叠 n 行…` + tail3；总行数 ≤ 6 时全显。
            // 24 / 8 / 500 三种行帽与卡片级 `expanded` 一并删除。
            let window = tool_body_window(&src, is_running);
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let body_start = lines.len();
            for (i, (_src_idx, out)) in window.lines.iter().enumerate() {
                if let Some((at, n)) = window.fold
                    && i == at
                {
                    lines.push(tool_body_fold_line(line_prefix, n));
                }
                if out.is_empty() {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span("", SpanStyle::Dim),
                    );
                    continue;
                }
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span(seg, SpanStyle::Dim),
                    );
                }
            }
            // 正文到此为止。`▌`（流式实时光标）必须落回**最后一行正文**（落在
            // footer 上会把注记画成流内容）。
            let body_end = lines.len();
            // shell 终态元数据（exit / 后端截断标注）——与折叠标注是两回事：
            // 折叠是**显示**窗口，这里是**数据**层面的状态。
            if !is_running && let Some((exit, truncated, _)) = shell_meta {
                let mut foot: Vec<String> = Vec::new();
                if let Some(code) = exit
                    && code != 0
                {
                    foot.push(format!("exit {code}"));
                }
                if truncated {
                    foot.push("截断".to_string());
                }
                if !foot.is_empty() {
                    lines.push(
                        RenderLine::new()
                            .span(format!("{}  ", line_prefix), SpanStyle::Dim)
                            .span(foot.join(" · "), SpanStyle::Dim),
                    );
                }
            }
            if src_from_progress && tool.progress_truncated {
                lines.push(progress_truncated_line(is_block));
            }
            if is_running
                && body_end > body_start
                && let Some(last) = lines.get_mut(body_end - 1)
                && let Some(span) = last.spans.last_mut()
            {
                span.text.push('▌');
            }
        }
    } else {
        let mut combined = String::new();
        let display_text = display_body.and_then(|b| match b {
            TimelineToolBody::Text { text, .. } => Some(text.clone()),
            _ => None,
        });
        if let Some(text) = display_text {
            combined.push_str(&text);
        } else if let Some(output) = tool.output.as_deref().filter(|s| !s.is_empty()) {
            // display 缺失（旧 daemon / 未投影工具）→ 原样透出（H16 回退）。
            combined.push_str(output);
            if !tool.progress.is_empty() {
                combined.push('\n');
            }
        }
        combined.push_str(&tool.progress);
        let body_truncated = display_body.is_some_and(|b| {
            matches!(
                b,
                TimelineToolBody::Text {
                    truncated: true,
                    ..
                } | TimelineToolBody::Shell {
                    truncated: true,
                    ..
                } | TimelineToolBody::Streams {
                    truncated: true,
                    ..
                }
            )
        });
        // progress 在 `combined` 里的起始行号（源行号口径）。窗口按源行号取，
        // 故判据仍是「渲染出的源行号是否落在 progress 段内」。
        let progress_start_line = combined
            .lines()
            .count()
            .saturating_sub(tool.progress.lines().count());
        if !combined.trim().is_empty() {
            // W-02 裁决：正文窗口由 [`tool_body_window`] 唯一决定（与 shell 分支
            // 同一规则）——Running 取 6 行尾窗；结束取 head3 + `…折叠 n 行…` +
            // tail3；总行数 ≤ 6 时全显。24 / 4 行帽与卡片级 `expanded` 一并删除。
            let window = tool_body_window(&combined, is_running);
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let body_start = lines.len();
            // 「进度真的上屏」= 正文里确实渲染出了属于 progress 段的行。
            //
            // **取舍**：折叠丢的是**显示**（`…折叠 n 行…` 已把丢弃画出来），不属
            // B1 要盯的**不可逆丢弃**；而本标注盯的 `progress_truncated` 是不可逆的
            // （缓冲前段已从内存里丢掉）。`progress` 为空时 `progress_start_line`
            // 落在行号范围之外，永远为 false。
            let mut progress_shown = false;
            for (i, (src_idx, out)) in window.lines.iter().enumerate() {
                if let Some((at, n)) = window.fold
                    && i == at
                {
                    lines.push(tool_body_fold_line(line_prefix, n));
                }
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span(seg, SpanStyle::Dim),
                    );
                }
                if *src_idx >= progress_start_line {
                    progress_shown = true;
                }
            }
            // 正文到此为止。`▌`（流式实时光标）必须落回**最后一行正文**。
            let body_end = lines.len();
            if tool.progress_truncated && progress_shown {
                lines.push(progress_truncated_line(is_block));
            }
            if body_truncated {
                lines.push(
                    RenderLine::new()
                        .span("    ◌ ", SpanStyle::Warn)
                        .span("正文已截断（output_bytes 见 metrics）", SpanStyle::Warn),
                );
            }
            if is_running
                && body_end > body_start
                && let Some(last) = lines.get_mut(body_end - 1)
                && let Some(span) = last.spans.last_mut()
            {
                span.text.push('▌');
            }
        }
    }

    if let Some(err) = &tool.failure {
        for seg in wrap_text(
            &format!("{}: {}", err.code, err.message),
            width.saturating_sub(6),
        ) {
            let pfx = if is_block { " ┃ ✗ " } else { "    ✗ " };
            lines.push(
                RenderLine::new()
                    .span(pfx, SpanStyle::Error)
                    .span(seg, SpanStyle::Error),
            );
        }
    }
    if let Some(perm) = &tool.permission {
        let pfx = if is_block { " ┃ ⚠ " } else { "    ⚠ " };
        lines.push(RenderLine::new().span(pfx, SpanStyle::Warn).span(
            format!("等待权限：{}（risk {}）", perm.category, perm.risk),
            SpanStyle::Warn,
        ));
    }
    // Block 底部收口留白（对齐 BlockTool paddingBottom 1）
    if is_block {
        lines.push(RenderLine::new().span(" ┃", SpanStyle::Dim));
    }

    // Phase C：metrics 尾注（契约 §3.3；耗时/体量仅在投影提供时可见）。
    if let Some(m) = d.and_then(|d| d.metrics.as_ref())
        && m.elapsed_ms > 0
    {
        let mut foot = format!("  ◷ {:.1}s", m.elapsed_ms as f64 / 1000.0);
        if m.output_bytes > 0 {
            foot.push_str(&format!(" · {:.1} KB", m.output_bytes as f64 / 1024.0));
        }
        lines.push(RenderLine::new().span(foot, SpanStyle::Dim));
    }
}

/// 会话信息行（标签栏下方）。
pub fn render_session_info(session: &SessionState, width: u16) -> Vec<RenderLine> {
    let mut spans: Vec<(String, SpanStyle)> = Vec::new();
    if let Some(model) = session.display_model() {
        spans.push((model, SpanStyle::Accent));
    }
    match session.mode {
        qaqh_client::ConversationMode::Plan => {
            spans.push(("plan".into(), SpanStyle::Warn));
        }
        qaqh_client::ConversationMode::Code => {
            spans.push(("code".into(), SpanStyle::Dim));
        }
    }
    if session.code_added > 0 || session.code_removed > 0 {
        spans.push((format!("+{}", session.code_added), SpanStyle::DiffAdd));
        spans.push((format!("−{}", session.code_removed), SpanStyle::DiffDel));
    }
    if let Some(anim) = &session.compact_anim {
        // 伪进度（≈）+ 协议真实信息（turns/delta）；info 行每帧独立渲染 → 自然逐帧动画。
        let ratio = crate::app::anim::pseudo_progress(anim.started_at.elapsed());
        let bar = crate::app::anim::bar(Some(ratio), 10, crate::app::anim::frame_now());
        let delta = anim.last_delta.as_deref().unwrap_or("估算");
        spans.push((
            format!(
                "≈{bar} 压缩中 {}/{} · {delta}",
                anim.turns_keeping, anim.turns_total
            ),
            SpanStyle::Warn,
        ));
    }
    if let Some(err) = &session.last_error {
        spans.push((format!("err:{}", err.code), SpanStyle::Error));
    }
    let seed_label = format!("#{}", session.seed);
    // 回合总数放这里（**不经段缓存**，每帧独立渲染）：段头部只留编号，
    // 否则总数一变就会污染每一段的缓存键（见 `render_turn` 的注释）。
    if session.timeline.turns.len() < session.timeline.turn_total() as usize {
        spans.push((
            format!(
                "{}/{}",
                session.timeline.turns.len(),
                session.timeline.turn_total()
            ),
            SpanStyle::Dim,
        ));
    }
    let mut line = RenderLine::new().span(" ", SpanStyle::Dim);
    let mut used = 2usize;
    for (text, style) in spans {
        let w = text.chars().count() + 2;
        if used + w > width as usize {
            break;
        }
        line = line.span(format!("{text}  "), style);
        used += w;
    }
    // 右侧 seed。
    let seed_w = seed_label.chars().count() + 1;
    if used + seed_w <= width as usize {
        let pad = width as usize - used - seed_w;
        line = line.span(" ".repeat(pad), SpanStyle::Dim);
        line = line.span(seed_label, SpanStyle::Dim);
    }
    vec![line]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::SessionState;
    use crate::app::timeline_model::{Block, Round, ToolCard, Turn};
    use qaqh_client::{
        TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState,
    };

    fn flatten(lines: &[RenderLine]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn turn_with_offload(offloaded: bool) -> SessionState {
        let mut sess = SessionState::new("seed-1".into());
        sess.timeline.turns.push(Turn {
            thinking: Default::default(),
            turn_index: None,
            turn_id: "t1".into(),
            user_text: "hi".into(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded,
            rounds: Vec::new(),
        });
        sess
    }

    /// T-06：offload 后常驻内存里只剩「预览壳」（block 正文截到 512 字符、
    /// `tool.output`/`tool.diff` 清空）。必须在 transcript 里显式标注——否则
    /// 用户会把残缺内容当成完整回合，读完还以为「模型就答了这么点」。
    /// 与 B1「丢弃必须可见」同一设计原则。
    ///
    /// 破坏验证：去掉 `if turn.offloaded` 那段渲染分支 → 本测试红。
    #[test]
    fn offloaded_turn_is_marked_as_preview() {
        let flat = flatten(&render_transcript(&turn_with_offload(true), 100));
        assert!(flat.contains("已归档"), "offload 必须可见，实测：{flat}");
        assert!(flat.contains("预览"));
    }

    /// 反向闸：未 offload 的回合不得挂这条警告，否则每个正常回合都被标成残缺。
    #[test]
    fn normal_turn_has_no_preview_warning() {
        let flat = flatten(&render_transcript(&turn_with_offload(false), 100));
        assert!(
            !flat.contains("已归档"),
            "未 offload 不得报警告，实测：{flat}"
        );
    }

    /// 回归：ask 的机器回执 JSON 不得出现在 transcript（答案由交互弹窗承载）。
    #[test]
    fn ask_result_json_is_not_dumped() {
        let ask = ToolCard {
            tool_call_id: "c-ask".into(),
            name: "ask".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"questions":[{"id":"q1","question":"想做什么"}]}"#.into()),
            output: Some(
                r#"{"status":"ok","data":{"answers":[{"question_id":"q1","answer":"其他"}]}}"#
                    .into(),
            ),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &ask, 100);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("ask"), "仍应显示调用轨迹");
        assert!(flat.contains("已回答"), "应给出状态专用语");
        assert!(
            !flat.contains("\"status\""),
            "不得 dumping 机器回执: {flat}"
        );
        assert!(!flat.contains("question_id"), "不得 dumping 答案结构");
        assert!(!flat.contains("F7"), "无输出可折叠时不应提示 F7");
    }

    /// 回归：todo 三件套的回执由 workspace 面板承载，transcript 只留调用轨迹。
    ///
    /// 后端 `todo_write` 的真实输出形如
    /// `{"timeis":"...","status":"ok","created":[...],"count":2,"message":"..."}`
    /// —— `timeis`/`status` 对纯属机器字段，面板已有权威投影。
    #[test]
    fn todo_result_json_is_not_dumped() {
        for name in ["todo_write", "todo_update", "todo_list"] {
            let todo = ToolCard {
                tool_call_id: "c-todo".into(),
                name: name.into(),
                state: TimelineToolState::Succeeded,
                summary: None,
                args_json: Some(r#"{"items":[{"title":"分析项目"}]}"#.into()),
                output: Some(
                    r#"{"timeis":"UTC+8 2026-09-09 00:00","status":"ok","created":[],"count":1,"message":"Created 1 todo(s): T1"}"#
                        .into(),
                ),
                diff: None,
                progress: String::new(),
                progress_truncated: false,
                progress_bytes_total: 0,
                progress_stream: None,
                failure: None,
                permission: None,
            display: None,
            };
            let mut lines = Vec::new();
            push_tool_card(&mut lines, &todo, 100);
            let flat: String = lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(flat.contains(name), "{name} 仍应显示调用轨迹");
            assert!(
                !flat.contains("\"status\""),
                "{name} 不得 dumping 回执: {flat}"
            );
            assert!(!flat.contains("timeis"), "{name} 不得显示机器时间戳");
            assert!(!flat.contains("F7"), "{name} 无输出可折叠时不应提示 F7");
        }
    }

    /// skills 无独立面板承载结果，且 activate 后指令以系统消息注入；
    /// 结果本体只是回执，不应占据 transcript。
    #[test]
    fn skills_result_json_is_not_dumped() {
        let skills = ToolCard {
            tool_call_id: "c-skills".into(),
            name: "skills".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"action":"activate","name":"foo"}"#.into()),
            output: Some(
                r#"{"status":"ok","skill":"foo","resources":["a.md"],"content":"[OK] skill 'foo' activated."}"#
                    .into(),
            ),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
        display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &skills, 100);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("skills"), "仍应显示调用轨迹");
        assert!(flat.contains("已载入"));
        assert!(!flat.contains("\"resources\""), "不得 dumping 回执: {flat}");
    }

    /// spawn_subagent：seed JSON 不得外泄（子代理由标签栏徽标/只读视图承载）。
    #[test]
    fn spawn_subagent_result_json_is_not_dumped() {
        let spawn = ToolCard {
            tool_call_id: "c-spawn".into(),
            name: "spawn_subagent".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"agent_name":"explore"}"#.into()),
            output: Some(r#"{"status":"ok","seed":"abc-123","process_id":42}"#.into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &spawn, 100);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("spawn_subagent"));
        assert!(flat.contains("已派发"));
        assert!(!flat.contains("abc-123"), "seed 属于身份锚点而非展示文本");
    }

    /// 失败态必须仍然可见：抑制输出不得吞掉错误。
    #[test]
    fn panel_owned_tool_failure_still_rendered() {
        let failed = ToolCard {
            tool_call_id: "c-todo-fail".into(),
            name: "todo".into(),
            state: TimelineToolState::Failed,
            summary: None,
            args_json: None,
            output: Some(r#"{"status":"error"}"#.into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: Some(qaqh_client::TimelineFailure {
                code: "invalid_input".into(),
                message: "items 为空".into(),
            }),
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &failed, 100);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("invalid_input"), "失败码必须可见");
        assert!(flat.contains("items 为空"), "失败原因必须可见");
    }

    /// read/bash 等真正输出即结果的工具不受影响（防止过度抑制）。
    #[test]
    fn non_panel_tools_still_render_output() {
        let read = ToolCard {
            tool_call_id: "c-read2".into(),
            name: "read".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some("fn main() {}\n".into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &read, 100);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("fn main()"), "read 输出仍须可见");
    }

    #[test]
    fn panel_owned_tool_classification() {
        // 后端正式词表（registration.rs）中的专属面板工具
        assert!(is_panel_owned_tool("todo_write"));
        assert!(is_panel_owned_tool("todo_update"));
        assert!(is_panel_owned_tool("todo_list"));
        assert!(is_panel_owned_tool("ask"));
        assert!(is_panel_owned_tool("skills"));
        // 历史 timeline 回放兼容（Todo v3 之前的单名工具）
        assert!(is_panel_owned_tool("todo"));
        // 输出即结果的工具必须保持可见
        assert!(!is_panel_owned_tool("exec"));
        assert!(!is_panel_owned_tool("read"));
        assert!(!is_panel_owned_tool("edit"));
        assert!(!is_panel_owned_tool("grep"));
        assert!(!is_panel_owned_tool("glob"));
        assert!(!is_panel_owned_tool("journal"));
        assert!(!is_panel_owned_tool("process"));
        assert!(!is_panel_owned_tool("web_fetch"));
        assert!(!is_panel_owned_tool("apply_patch"));
        assert!(!is_panel_owned_tool("write"));
        assert!(!is_panel_owned_tool("copy_range"));
        assert!(!is_panel_owned_tool("confirm_apply"));
    }

    // ── 回归锁：`serde_json` 保序（preserve_order）对渲染路径的影响 ──────────
    //
    // 背景：`qaqh-client` 声明 `serde_json = { version = "1", features =
    // ["preserve_order"] }`；cargo 的 feature 相加把它统一到本仓共用的同一个
    // `serde_json` 节点上——于是 TUI 的 JSON map 从 `BTreeMap`（按 key 字典序）
    // 变成 `IndexMap`（按 JSON 原文顺序）。
    //
    // 依据：`docs/handoff/2026-09-15-TUI镜像层修复与qaqh-client迁移阶段一-handoff.md:90`
    // 明写「属行为变化，迁移后需回归一遍依赖 key 顺序的渲染路径」；此后两仓 docs
    // 再无第二次出现该条，也没有任何测试覆盖。下面这组测试即那条缺失的回归锁。
    //
    // 本文件里「整表遍历」只有两处：`format_args_preview`（`map.iter()`）与
    // `pretty_json_output`（`obj.iter()`）；其余全部是 `obj.get(...)` 定点取值，
    // 天然与顺序无关。

    /// 根因探针：直接钉住「map 迭代顺序 = JSON 原文顺序」。
    ///
    /// 若 `qaqh-client` 哪天去掉 `preserve_order`（或本仓改回显式排序），本测试
    /// 先红，提示下面两条渲染断言需要重新决策。
    #[test]
    fn serde_json_map_iteration_order_is_source_order() {
        let v: serde_json::Value = serde_json::from_str(r#"{"zeta":1,"alpha":2,"mid":3}"#).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["zeta", "alpha", "mid"],
            "本仓的渲染顺序由 qaqh-client 的 serde_json `preserve_order` 经 feature \
             相加传递而来（不是本仓自选）。此处变红=**上游 feature 变了**，不是本仓\
             写错：请重新决策下面两条渲染断言——要么接受字典序并同步改断言，要么\
             让 qaqh-client 保留 preserve_order。字典序下这里会得到 [alpha, mid, zeta]。"
        );
    }

    /// 回归锁：`format_args_preview` 的 `[k=v, …]` 顺序同样跟随原文 key 顺序。
    ///
    /// 该预览用于非 shell 工具卡的 `⌗` 参数行。
    #[test]
    fn format_args_preview_follows_source_key_order() {
        let preview = format_args_preview(r#"{"zeta":"z","alpha":"a","mid":"m"}"#);
        assert_eq!(preview, "[zeta=z, alpha=a, mid=m]");
    }

    /// 回归锁（PR #16 审查建议 1）：`format_args_preview` 的 **3-key 截断边界**同样
    /// 受保序支配——循环里 `parts.len() >= 3` 就 `break`，key 顺序一变，被截掉的
    /// 就是另一个 key。这是该函数里唯一有副作用的边界，而上一条 3-key 用例正好
    /// 用满 3 个 key，绕开了它。
    ///
    /// 判别力：原文顺序下保留前 3 个（zeta/alpha/mid）并丢弃第 4 个 beta；字典序下
    /// 会保留 alpha/beta/mid 而丢弃 zeta——两者不同，故下面三条断言在字典序下必红。
    #[test]
    fn format_args_preview_truncates_to_first_three_keys_in_source_order() {
        let preview = format_args_preview(r#"{"zeta":"z","alpha":"a","mid":"m","beta":"b"}"#);
        assert_eq!(preview, "[zeta=z, alpha=a, mid=m]");
        assert!(
            !preview.contains("beta"),
            "第 4 个 key 应被截断丢弃（保序下 beta 排在第 4）：{preview}"
        );
        assert_ne!(
            preview, "[alpha=a, beta=b, mid=m]",
            "字典序下会保留 alpha/beta/mid——本断言用于证明用例确有判别力"
        );
    }

    /// Phase C：exec 标题来自 display.header（Shell 命令），不再解析 args_json；
    /// display 缺失 → 回退渲染不带命令（诚实降级，绝不臆造命令行）。
    #[test]
    fn exec_title_from_display_and_honest_fallback() {
        let mk = |display: Option<TimelineToolDisplay>| ToolCard {
            tool_call_id: "c-exec".into(),
            name: "exec".into(),
            state: TimelineToolState::Succeeded,
            // 后端真实行为：summary 就是整坨 ExecOutput JSON
            summary: Some(
                r#"{"status":"completed","command":"cargo ...","exit_code":0,"output":""}"#.into(),
            ),
            args_json: Some(r#"{"command":"cargo build"}"#.into()),
            output: Some(r#"{"status":"completed","exit_code":0,"output":"l1"}"#.into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display,
        };
        let d = TimelineToolDisplay {
            summary: None,
            diff: None,
            header: Some(TimelineToolHeader::Shell {
                command: "cargo build".into(),
            }),
            body: Some(TimelineToolBody::Shell {
                output: "l1\nl2\nl3".into(),
                exit_code: Some(0),
                truncated: false,
            }),
            metrics: Some(qaqh_client::TimelineToolMetrics {
                elapsed_ms: 1500,
                output_bytes: 4096,
                retry_count: 0,
                effective_tool_name: None,
                user_initiated: false,
            }),
            outcome: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &mk(Some(d)), 80);
        let flat = flatten(&lines);
        assert!(
            flat.contains("cargo build"),
            "display Shell 标题必须携带真实命令：{flat}"
        );
        assert!(
            !flat.contains("status"),
            "不得把 ExecOutput JSON 糊进标题：{flat}"
        );
        assert!(flat.contains("1.5s"), "metrics 耗时必须可见：{flat}");
        assert!(flat.contains("4.0 KB"), "metrics 体量必须可见：{flat}");
        assert!(
            flat.contains("l1"),
            "Shell body 输出必须直接上屏（无需 JSON 剥壳）：{flat}"
        );

        // H16 回退：display 与参数都缺失 → 只显示工具名，不臆造命令行。
        let mut fallback = mk(None);
        fallback.args_json = None;
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &fallback, 80);
        let flat = flatten(&lines);
        // flatten 以 span 为界 join，"# exec" 是两个 span——按语义断言。
        assert!(flat.contains("\nexec\n"), "回退标题仍是工具名：{flat}");
        assert!(!flat.contains("cargo build"), "回退不得臆造命令：{flat}");
    }

    /// Phase C：ask/skills 投影（6b08e14）——面板工具单行消费 display.summary。
    #[test]
    fn panel_owned_tools_consume_display_summary() {
        let mk = |name: &str, summary: &str, body: TimelineToolBody| ToolCard {
            tool_call_id: "c".into(),
            name: name.into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: Some(TimelineToolDisplay {
                summary: Some(summary.into()),
                diff: None,
                header: Some(TimelineToolHeader::Other { label: name.into() }),
                body: Some(body),
                metrics: None,
                outcome: None,
            }),
        };
        // ask：Body::None + 投影摘要。
        let mut lines = Vec::new();
        push_tool_card(
            &mut lines,
            &mk("ask", "asked 2 questions", TimelineToolBody::None),
            80,
        );
        let flat = flatten(&lines);
        assert!(
            flat.contains("asked 2 questions"),
            "display.summary 必须可见：{flat}"
        );
        assert!(
            flat.contains("已回答"),
            "投影摘要存在时不再需要状态补语：{flat}"
        );

        // skills resource：Body::Text —— 面板工具不渲染正文（面板是权威投影）。
        let mut lines = Vec::new();
        push_tool_card(
            &mut lines,
            &mk(
                "skills",
                "resource · rust-style-guide",
                TimelineToolBody::Text {
                    text: "指南全文".into(),
                    truncated: false,
                },
            ),
            80,
        );
        let flat = flatten(&lines);
        assert!(flat.contains("resource · rust-style-guide"), "{flat}");
        assert!(
            !flat.contains("指南全文"),
            "面板工具正文不得重复渲染：{flat}"
        );
    }

    /// Phase C：Path 头 + Text 体（read 族）与截断标注（B1）。
    #[test]
    fn display_path_header_and_text_body() {
        let card = ToolCard {
            tool_call_id: "c-read".into(),
            name: "read".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: Some(TimelineToolDisplay {
                summary: Some("42 行".into()),
                diff: None,
                header: Some(TimelineToolHeader::Path {
                    path: "src/app/mod.rs".into(),
                    op: qaqh_client::TimelinePathOp::Read,
                }),
                body: Some(TimelineToolBody::Text {
                    text: "use ratatui::Frame;".into(),
                    truncated: true,
                }),
                metrics: None,
                outcome: None,
            }),
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &card, 80);
        let flat = flatten(&lines);
        assert!(flat.contains("src/app/mod.rs"), "Path 头必须进标题：{flat}");
        assert!(
            flat.contains("use ratatui::Frame;"),
            "Text 体必须直接上屏：{flat}"
        );
        assert!(
            flat.contains("正文已截断"),
            "body.truncated 必须可见（B1）：{flat}"
        );
    }

    #[test]
    fn hunk_header_parsing() {
        assert_eq!(parse_hunk_header("@@ -1,3 +1,4 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -10 +20 @@"), Some((10, 20)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1 @@"), Some((0, 1)));
        assert!(parse_hunk_header("--- a/file").is_none());
    }

    #[test]
    fn tool_card_renders_inline_and_block() {
        let tool_inline = ToolCard {
            tool_call_id: "c1".into(),
            name: "read".into(),
            state: TimelineToolState::Succeeded,
            summary: Some("ok".into()),
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some("content".into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool_inline, 80);
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("read")))
        );

        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        let tool_block = ToolCard {
            tool_call_id: "c2".into(),
            name: "edit".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some("ok".into()),
            diff: Some(diff.into()),
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines2 = Vec::new();
        push_tool_card(&mut lines2, &tool_block, 130); // wide -> split
        assert!(
            lines2
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("Δ")))
        );
        assert!(
            lines2
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("split")))
        );
        let mut lines3 = Vec::new();
        push_tool_card(&mut lines3, &tool_block, 80); // narrow -> unified
        assert!(
            lines3
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("Δ")))
        );
    }

    #[test]
    fn shell_output_unwrap_and_streaming_slice() {
        let raw = r#"{"status":"completed","command":"bash ...","exit_code":0,"output":"line1\nline2\nline3","truncated":false,"timed_out":false,"cancelled":false}"#;
        let inner = extract_shell_output_text(raw).unwrap();
        assert_eq!(inner, "line1\nline2\nline3");
        // streaming 8 行尾
        let mut long = String::new();
        for i in 1..=20 {
            long.push_str(&format!("line{i}\n"));
        }
        let tool = ToolCard {
            tool_call_id: "c1".into(),
            name: "bash".into(),
            state: TimelineToolState::Running,
            summary: None,
            args_json: None,
            output: Some(raw.to_string()),
            diff: None,
            progress: long.clone(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        // 应包含尾行 line20，且不含 JSON 外壳
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("line20")))
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("\"command\"")))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("▌")))
        );
    }

    /// 单行拼接（**不加分隔符**）：`flatten` 把每个 span 用 `\n` 连起来，
    /// 会把「前缀里的 ◌」和「文案里的 ◌」分开，从而看不见重复字形。
    fn joined(l: &RenderLine) -> String {
        l.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// 流式 bash 卡：progress 20 行（>8 行 → 走折叠），截断标志可控。
    /// wire 语义：`progress_truncated` = writer 已丢弃过 progress 的前段。
    fn streaming_bash_card(id: &str, truncated: bool) -> ToolCard {
        let mut progress = String::new();
        for i in 1..=20 {
            progress.push_str(&format!("line{i}\n"));
        }
        ToolCard {
            tool_call_id: id.into(),
            name: "bash".into(),
            state: TimelineToolState::Running,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress,
            progress_truncated: truncated,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        }
    }

    /// CNB issue #4 缺陷 2（B1「丢弃必须可见」）：`progress_truncated` 此前是**死字段**
    /// ——`timeline_model.rs` 只写、全仓无生产读取，于是进度被丢头保尾时用户看到
    /// 的「只剩尾巴」和完整输出长得一模一样。本测试锁住消费面：截断必须上屏。
    ///
    /// 两张卡而非一张：`flatten` 拍平整棵树，单卡 fixture 分不清「每卡一行」和
    /// 「全局一行」（PR #18 审查指出旧断言可被绕过）。
    ///
    /// 变异验证（实测）：删掉 `push_tool_card` 里 `progress_truncated_line` 的推送
    /// → 本测试红；把标注提到卡片循环外只出一行 → 也红。
    #[test]
    fn truncated_progress_is_marked_per_card() {
        let a = streaming_bash_card("c-trunc-a", true);
        let b = streaming_bash_card("c-trunc-b", true);
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &a, 80);
        push_tool_card(&mut lines, &b, 80);
        let marked = lines
            .iter()
            .filter(|l| joined(l).contains(PROGRESS_TRUNCATED_MARK))
            .count();
        assert_eq!(
            marked,
            2,
            "两张截断卡各出一行（不是全局一行），实测 {marked} 行：{}",
            flatten(&lines)
        );
    }

    /// 标注行只能有一个 ◌：它由前缀给出。曾把 ◌ 也写进 `PROGRESS_TRUNCATED_MARK`，
    /// 渲染成 ` ┃ ◌ ◌ 进度前段已丢弃…`（PR #18 审查实测；旧断言用 `flatten`，
    /// span 间的 `\n` 恰好把两个 ◌ 分开了，所以漏检）。
    ///
    /// 变异验证（实测）：把 `◌ ` 加回 `PROGRESS_TRUNCATED_MARK` 开头 → 本测试红。
    #[test]
    fn truncation_mark_renders_single_glyph() {
        let tool = streaming_bash_card("c-glyph", true);
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let line = lines
            .iter()
            .map(joined)
            .find(|t| t.contains(PROGRESS_TRUNCATED_MARK))
            .unwrap_or_else(|| panic!("应有截断标注行，实测：{}", flatten(&lines)));
        assert!(
            line.contains("◌ 进度前段已丢弃（仅保留末尾）"),
            "标注应为「◌ + 文案」，实测：{line}"
        );
        assert!(!line.contains("◌ ◌"), "◌ 重复了，实测：{line}");
    }

    /// 标注排在正文**之后**：它是这张卡的 footer，读者不必跨正文拼读
    /// （PR #18 审查：旧版把标注夹在头部、正文留在尾部）。
    ///
    /// W-02：卡片上原来的 per-card「F7 收起 / 展开」已删除（F7 是 §4.2 的组级
    /// 开关），故本锁改为钉「标注在最后一行正文（`▌`）之后」。
    ///
    /// 变异验证（实测）：把标注推回正文之前 → 本测试红。
    #[test]
    fn truncation_mark_follows_body() {
        let tool = streaming_bash_card("c-order", true);
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let body_idx = lines
            .iter()
            .position(|l| joined(l).contains('▌'))
            .unwrap_or_else(|| panic!("Running 卡应有实时光标：{}", flatten(&lines)));
        let mark_idx = lines
            .iter()
            .position(|l| joined(l).contains(PROGRESS_TRUNCATED_MARK))
            .expect("截断标注应上屏");
        assert!(
            mark_idx > body_idx,
            "标注应在正文之后：正文@{body_idx} 标注@{mark_idx}\n{}",
            flatten(&lines)
        );
    }

    /// `▌`（流式实时光标）必须贴在**最后一行正文**上：footer（折叠 hint / 截断
    /// 标注）是注记，光标落在注记行上会把注记画成流内容（PR #18 审查指出旧版
    /// `▌` 留在末行）。
    ///
    /// 变异验证（实测）：把 `▌` 的落点改回 `lines.last_mut()` → 光标落到标注行 → 红。
    #[test]
    fn streaming_cursor_stays_on_last_body_line() {
        let tool = streaming_bash_card("c-cursor", true);
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let idx = lines
            .iter()
            .position(|l| joined(l).contains('▌'))
            .unwrap_or_else(|| panic!("Running 卡应有实时光标，实测：{}", flatten(&lines)));
        let text = joined(&lines[idx]);
        assert!(text.contains("line20"), "▌ 应贴最后一行正文，实测：{text}");
        assert!(
            !text.contains("F7") && !text.contains(PROGRESS_TRUNCATED_MARK),
            "▌ 不得落在 footer 行上，实测：{text}"
        );
    }

    /// 反向闸 1：未截断的进度不得挂标注——否则每个正常流式工具都被标成残缺。
    #[test]
    fn complete_progress_has_no_truncation_mark() {
        let tool = ToolCard {
            tool_call_id: "c-full".into(),
            name: "bash".into(),
            state: TimelineToolState::Running,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: "line1\nline2\n".into(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let flat = flatten(&lines);
        assert!(
            !flat.contains(PROGRESS_TRUNCATED_MARK),
            "未截断不得标注，实测：{flat}"
        );
    }

    /// 反向闸 2：工具已结束且拿到完整 `output` 时，progress 只是被取代的中间态，
    /// 正文并非残缺 → 不得把「前段已丢弃」挂在完整输出上（那是假警报）。
    #[test]
    fn finished_shell_output_supersedes_truncated_progress_mark() {
        let raw = r#"{"status":"completed","command":"bash ...","exit_code":0,"output":"full result","truncated":false,"timed_out":false,"cancelled":false}"#;
        let tool = ToolCard {
            tool_call_id: "c-done".into(),
            name: "bash".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: None,
            output: Some(raw.to_string()),
            diff: None,
            progress: "tail only\n".into(),
            progress_truncated: true,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let flat = flatten(&lines);
        assert!(flat.contains("full result"), "正文应是完整输出：{flat}");
        assert!(
            !flat.contains(PROGRESS_TRUNCATED_MARK),
            "进度未上屏时不得标注截断，实测：{flat}"
        );
    }

    /// 非 shell 分支同样要接上消费面：快照里 `progress_truncated` 为真、且进度
    /// 短到能直接看见（wire 侧丢头后只剩尾部片段）时，标注必须出现。
    #[test]
    fn truncated_progress_is_marked_for_non_shell_tool() {
        let tool = ToolCard {
            tool_call_id: "c-read".into(),
            name: "read".into(),
            state: TimelineToolState::Running,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: "tail of a long stream\n".into(),
            progress_truncated: true,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let flat = flatten(&lines);
        assert!(
            flat.contains(PROGRESS_TRUNCATED_MARK),
            "非 shell 工具的截断进度也必须可见，实测：{flat}"
        );
    }

    /// PR #18 二轮复审**阻断 2**：非 shell 分支的闸门必须看「进度是否真的上屏」，
    /// 不能是 `progress_truncated` 这类字段的布尔组合。
    ///
    /// W-02 后正文是固定窗口（head3 + `…折叠 n 行…` + tail3，≤6 行全显），判据仍是
    /// 「渲染出的**源行号**是否落在 progress 段内」——progress 拼在正文末尾，故非空
    /// 时必落在 tail3 窗口内；`progress` 为空时 `progress_start_line` 落在行号范围
    /// 之外，永远为 false。
    ///
    /// | 组合（`read`） | 进度可见 | 期望标注 |
    /// |---|---|---|
    /// | 30 行 output + 1 行 progress | 是（落在 tail3） | 有 |
    /// | 6 行 output + 1 行 progress | 是（tail3 覆盖末行） | 有 |
    /// | 短 output + 1 行 progress | 是（≤6 行全显） | 有 |
    /// | **30 行 output + 空 progress** | **否** | **无** |
    ///
    /// 最后一行是关键：若闸门退化成「只看 `progress_truncated`」，这条会误报
    /// （B1 意义上的假警），本测试红。
    #[test]
    fn non_shell_truncation_mark_follows_what_is_actually_shown() {
        let long = (1..=30)
            .map(|i| format!("out line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let six = (1..=6)
            .map(|i| format!("out line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cases: [(&str, &str, bool); 4] = [
            (long.as_str(), "tail only\n", true),
            (six.as_str(), "tail only\n", true),
            ("short output", "tail only\n", true),
            (long.as_str(), "", false),
        ];
        for (output, progress, expect_mark) in cases {
            let tool = ToolCard {
                tool_call_id: "c-read-gate".into(),
                name: "read".into(),
                state: TimelineToolState::Succeeded,
                summary: None,
                args_json: None,
                output: Some(output.to_string()),
                diff: None,
                progress: progress.into(),
                progress_truncated: true,
                progress_bytes_total: 0,
                progress_stream: None,
                failure: None,
                permission: None,
                display: None,
            };
            let mut lines = Vec::new();
            push_tool_card(&mut lines, &tool, 80);
            let flat = flatten(&lines);
            assert_eq!(
                flat.contains(PROGRESS_TRUNCATED_MARK),
                expect_mark,
                "output={}行 progress={}字 期望标注={expect_mark}\n{flat}",
                output.lines().count(),
                progress.chars().count()
            );
        }
    }

    /// §4.6：聚合元数据上回合头（B1 载体）。
    #[test]
    fn turn_header_shows_thinking_aggregate() {
        use crate::app::timeline_model::ThinkingStats;
        let mut sess = SessionState::new("s".into());
        let t = Turn {
            turn_id: "t1".into(),
            turn_index: None,
            user_text: "问".into(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            thinking: ThinkingStats {
                segments: 6,
                lines: 412,
            },
            rounds: Vec::new(),
        };
        sess.timeline.turns.push(t);
        let lines = render_transcript_with_opts(&sess, 80);
        let header: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(
            header.contains("思考 6 段/412 行"),
            "回合头应含聚合：{header}"
        );
    }

    #[test]
    fn shell_done_shows_unwrapped_tail_and_no_stderr_prefix() {
        let raw = r#"{"status":"completed","command":"bash ...","exit_code":1,"output":"out line\nerr line","truncated":false,"timed_out":false,"cancelled":false}"#;
        let tool = ToolCard {
            tool_call_id: "c2".into(),
            name: "bash".into(),
            state: TimelineToolState::Failed,
            summary: None,
            args_json: None,
            output: Some(raw.to_string()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("out line"));
        assert!(flat.contains("err line"));
        assert!(!flat.contains("[stderr]"));
        assert!(!flat.contains("\"status\""));
    }

    #[test]
    fn streams_body_keeps_stdout_and_stderr_distinguishable() {
        let tool = ToolCard {
            tool_call_id: "c-streams".into(),
            name: "bash".into(),
            state: TimelineToolState::Failed,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: Some(test_display(
                None,
                Some(TimelineToolBody::Streams {
                    stdout: "out line".into(),
                    stderr: "err line".into(),
                    exit_code: Some(1),
                    truncated: false,
                    interleaved: false,
                }),
            )),
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        let stdout_at = flat.find("stdout:").expect("stdout label");
        let stderr_at = flat.find("stderr:").expect("stderr label");
        assert!(stdout_at < flat.find("out line").expect("stdout body"));
        assert!(stderr_at < flat.find("err line").expect("stderr body"));
        assert!(
            flat.contains("exit 1"),
            "exit code must remain visible: {flat}"
        );
    }

    /// W-02 裁决（2026-09-20）：**卡片正文不展开**，正文高度恒 ≤ 7 行。
    ///
    /// - Running：固定 6 行尾窗（流式进度滚动窗口）；
    /// - 结束（Sealed / Failed / Cancelled / Backgrounded）：head3 + `…折叠 n 行…`
    ///   + tail3，`n = 总行数 − 6`；
    /// - 总行数 ≤ 6：全显、**不加标注**。
    ///
    /// 旧行为（本锁钉死的回归）：展开态被 `.take(24)` 截断——30 行只剩前 24 行且
    /// **无任何标注**，尾部既不可达也无提示；卡片上还挂着指向不存在动作的
    /// 「F7 收起 / 展开」。
    #[test]
    fn tool_body_window_matrix() {
        let card = |rows: usize, state: TimelineToolState| -> ToolCard {
            let out = (1..=rows)
                .map(|i| format!("ROW{i:02}"))
                .collect::<Vec<_>>()
                .join("\n");
            ToolCard {
                tool_call_id: "c-bash".into(),
                name: "bash".into(),
                state,
                summary: None,
                args_json: None,
                output: Some(format!(
                    r#"{{"status":"completed","output":{out:?},"exit_code":0}}"#
                )),
                diff: None,
                progress: String::new(),
                progress_truncated: false,
                progress_bytes_total: 0,
                progress_stream: None,
                failure: None,
                permission: None,
                display: None,
            }
        };
        // 正文行 = 含 `│` 的行（标题 / footer / 失败行都不含）。
        let body = |t: &ToolCard| -> Vec<String> {
            let mut lines = Vec::new();
            push_tool_card(&mut lines, t, 80);
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
                .filter(|r| r.contains('│'))
                .collect()
        };

        // 态 1（≤6 行）：全显、无标注。
        let r6 = body(&card(6, TimelineToolState::Succeeded));
        assert_eq!(r6.len(), 6, "6 行应全显：{r6:?}");
        assert!(
            !r6.iter().any(|r| r.contains("折叠")),
            "≤6 行不加标注：{r6:?}"
        );

        // 态 2（7 行）：head3 + `…折叠 1 行…` + tail3。
        let r7 = body(&card(7, TimelineToolState::Succeeded));
        assert_eq!(r7.len(), 7, "head3+标注+tail3 = 7 行：{r7:?}");
        assert!(r7[0].contains("ROW01") && r7[1].contains("ROW02") && r7[2].contains("ROW03"));
        assert!(r7[3].contains("…折叠 1 行…"), "实测：{r7:?}");
        assert!(r7[4].contains("ROW05") && r7[6].contains("ROW07"));

        // 态 3（30 行）：head3 + `…折叠 24 行…` + tail3 —— 旧实现在此丢 ROW25..ROW30。
        let r30 = body(&card(30, TimelineToolState::Succeeded));
        assert_eq!(r30.len(), 7, "正文高度恒 ≤ 7：{r30:?}");
        assert!(r30[0].contains("ROW01") && r30[2].contains("ROW03"));
        assert!(r30[3].contains("…折叠 24 行…"), "实测：{r30:?}");
        assert!(
            r30[4].contains("ROW28") && r30[6].contains("ROW30"),
            "尾部 3 行必须可见：{r30:?}"
        );

        // 态 4（Running）：恒 6 行尾窗，不标注。
        for n in [10usize, 100] {
            let r = body(&card(n, TimelineToolState::Running));
            assert_eq!(r.len(), 6, "Running 恒 6 行尾窗（n={n}）：{r:?}");
            assert!(
                r[5].contains(&format!("ROW{n:02}")),
                "尾窗最后一行应是源末行（n={n}）：{r:?}"
            );
            assert!(
                !r.iter().any(|x| x.contains("折叠")),
                "Running 不加标注：{r:?}"
            );
        }

        // 卡片级 F7 文案已删除（F7 只剩 §4.2 的组折叠语义）。
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &card(30, TimelineToolState::Succeeded), 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(
            !flat.contains("F7"),
            "卡片不应再挂 per-card F7 文案：{flat}"
        );
    }

    // ── A3：分段渲染缓存的增量语义 ───────────────────────────────

    fn sess_with_turns(n: usize) -> SessionState {
        let mut sess = SessionState::new("seg".into());
        for i in 0..n {
            sess.timeline.turns.push(Turn {
                thinking: Default::default(),
                turn_index: None,
                turn_id: format!("t{i}"),
                user_text: format!("问题 {i}"),
                state: TimelineTurnState::Completed,
                failure: None,
                sealed: true,
                offloaded: false,
                rounds: vec![Round {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![Block {
                        block_id: format!("b{i}"),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: format!("回答 {i} 的正文"),
                        tool: None,
                        last_fragment: 0,
                        rev: 1,
                    }],
                }],
            });
        }
        sess
    }

    #[test]
    fn turn_number_is_stable_across_cap() {
        let mut sess = sess_with_turns(20);
        // 取第 5 个回合（idx=5）在 cap 前的编号。
        let before = sess.timeline.turn_number(5);
        sess.timeline.cap_turns(15); // 丢掉最旧 5 个
        assert_eq!(sess.timeline.dropped_turns, 5);
        // 它现在位于 idx=0，编号必须不变。
        assert_eq!(
            sess.timeline.turn_number(0),
            before,
            "cap 后同一回合编号漂移 → 缓存必失效"
        );
        // 总数也保持稳定。
        assert!(sess.timeline.turn_total() >= before);
    }
}
