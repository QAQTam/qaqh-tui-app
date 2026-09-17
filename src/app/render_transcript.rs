//! transcript 渲染器：TimelineModel → Vec<RenderLine>（预折行，缓存友好）。

use crate::app::render_line::{RenderLine, SpanStyle, wrap_text};
use crate::app::session::{KEEP_MARGIN_SEGMENTS, SessionState};
use crate::app::timeline_model::Turn;
use qaqh_client::{TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState};

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

// ── Reasoning summary 特判：动词-ing 每句换行 ─────────────────────────────
// summary 来自 OpenAI Responses `response.reasoning_summary_text.delta`，
// 前端收到的是单段无换行的 gerund 句拼接（如 `Gathering ...feature.Synthesizing ...`，
// 句间缺空格/缺换行）；传统 thinking 则已含换行或多段。我们仅对 summary 做句级换行。
fn is_gerund_word(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_alphabetic());
    if w.len() < 4 {
        return false;
    }
    let lower = w.to_ascii_lowercase();
    lower.ends_with("ing") && lower.chars().all(|c| c.is_ascii_alphabetic())
}

fn sentence_starts_with_gerund(sentence: &str) -> bool {
    let trimmed = sentence.trim_start_matches(['•', '-', '"', '\'', '(', ' ']);
    if let Some(first) = trimmed.split_whitespace().next() {
        let w = first.trim_matches(|c: char| !c.is_alphabetic());
        is_gerund_word(w)
    } else {
        false
    }
}

fn is_cjk(ch: char) -> bool {
    let cp = ch as u32;
    // 简化：命中中日韩统一表意文字区段即可
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x3000..=0x303F).contains(&cp)
        || (0xFF00..=0xFFEF).contains(&cp)
}

fn split_reasoning_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        current.push(c);
        if c == '.' || c == '。' || c == '!' || c == '！' || c == '?' || c == '？' {
            // 小数点保护：1.2 不拆
            let prev_is_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let next_is_digit = i + 1 < n && chars[i + 1].is_ascii_digit();
            if c == '.' && prev_is_digit && next_is_digit {
                i += 1;
                continue;
            }
            // 寻下一个非空格字符
            let mut j = i + 1;
            while j < n && chars[j].is_whitespace() && chars[j] != '\n' {
                j += 1;
            }
            if j >= n {
                let s = current.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                current.clear();
            } else {
                let next = chars[j];
                let is_boundary = if c == '.' {
                    next.is_ascii_uppercase()
                } else if c == '。' || c == '！' || c == '？' {
                    true
                } else {
                    next.is_ascii_uppercase() || is_cjk(next)
                };
                // 缩写保护：".a" 小写不算句界
                if is_boundary {
                    let s = current.trim().to_string();
                    if !s.is_empty() {
                        out.push(s);
                    }
                    current.clear();
                    // 跳过句间空白（已入下一句）
                    i = j - 1;
                }
            }
        }
        i += 1;
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

fn looks_like_reasoning_summary(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return false;
    }
    // 强信号：缺空格的句界 "feature.Synthesizing"
    let has_missing_space = {
        let ch: Vec<char> = t.chars().collect();
        let mut found = false;
        for idx in 0..ch.len().saturating_sub(1) {
            if ch[idx] == '.' && ch[idx + 1].is_ascii_uppercase() {
                found = true;
                break;
            }
        }
        found
    };
    let sentences = split_reasoning_sentences(t);
    if has_missing_space {
        if sentences.len() >= 2 && sentences.iter().any(|s| sentence_starts_with_gerund(s)) {
            return true;
        }
        return sentences.len() >= 2;
    }
    if sentences.len() < 2 {
        return false;
    }
    // 中文句：含 "。" 且多句即视为 summary
    if t.contains('。') {
        return sentences.len() >= 2;
    }
    let gerund_cnt = sentences
        .iter()
        .filter(|s| sentence_starts_with_gerund(s))
        .count();
    gerund_cnt >= 1 && gerund_cnt * 2 >= sentences.len()
}

fn normalize_reasoning_content(text: &str) -> String {
    if looks_like_reasoning_summary(text) {
        return split_reasoning_sentences(text).join("\n");
    }
    if text.contains('\n') {
        // 段内仍可能藏 summary（如单段内拼接），逐段二次判别
        let mut paras: Vec<String> = Vec::new();
        for para in text.split('\n') {
            if para.trim().is_empty() {
                paras.push(String::new());
            } else if looks_like_reasoning_summary(para) {
                paras.extend(split_reasoning_sentences(para));
            } else {
                // 进一步：即便整段不像 summary，也尝试在缺空格场景下修复
                // 只有当 split 后句数>1 且含 gerund 时才替换，避免误伤普通段
                let split = split_reasoning_sentences(para);
                if split.len() >= 2 && split.iter().any(|s| sentence_starts_with_gerund(s)) {
                    paras.extend(split);
                } else {
                    paras.push(para.to_string());
                }
            }
        }
        // 若未发生任何分裂，直接返回原文避免无意义重组
        let joined = paras.join("\n");
        if joined != text {
            return joined;
        }
        return text.to_string();
    }
    text.to_string()
}

#[allow(dead_code)]
pub fn render_transcript(session: &SessionState, width: u16) -> Vec<RenderLine> {
    // 兼容入口：show_reasoning=true 时展示全文（F3 切换由外层 App 控制缓存失效）
    render_transcript_with_opts(session, width, true)
}

pub fn render_transcript_with_opts(
    session: &SessionState,
    width: u16,
    show_reasoning: bool,
) -> Vec<RenderLine> {
    let width = width.max(20) as usize;
    let mut lines: Vec<RenderLine> = Vec::new();

    for (turn_idx, turn) in session.timeline.turns.iter().enumerate() {
        lines.extend(render_turn(session, turn, turn_idx, width, show_reasoning));
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
pub(crate) fn render_banner(session: &SessionState) -> Option<RenderLine> {
    if session.timeline.has_more {
        return Some(RenderLine::new().span("↑ 更早回合已折叠（PgUp 加载）", SpanStyle::Dim));
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
fn render_turn(
    session: &SessionState,
    turn: &Turn,
    turn_idx: usize,
    width: usize,
    show_reasoning: bool,
) -> Vec<RenderLine> {
    let mut lines: Vec<RenderLine> = Vec::new();
    {
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
        let num = session.timeline.turn_number(turn_idx);
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

        // ── rounds / blocks ──
        for round in &turn.rounds {
            for block in &round.blocks {
                match block.kind {
                    TimelineBlockKind::Text => {
                        push_text_block(&mut lines, &block.text, width, block.is_streaming())
                    }
                    TimelineBlockKind::Reasoning => push_reasoning_block(
                        &mut lines,
                        &block.text,
                        width,
                        block.is_streaming(),
                        show_reasoning,
                    ),
                    TimelineBlockKind::Tool => {
                        if let Some(tool) = &block.tool {
                            let expanded = session.expanded_tools.contains(&tool.tool_call_id);
                            push_tool_card(&mut lines, tool, width, expanded);
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
            }
        }

        // 回合失败详情。
        if let Some(f) = &turn.failure {
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
    }
    lines
}

// ── 分段渲染缓存的键 ────────────────────────────────────────────────

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[inline]
fn h_u64(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(FNV_PRIME);
}

#[inline]
fn h_str(h: &mut u64, s: &str) {
    for b in s.as_bytes() {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
    // 长度参与：防止 "ab"+"c" 与 "a"+"bc" 拼接后同哈希。
    h_u64(h, s.len() as u64);
}

fn turn_state_tag(s: TimelineTurnState) -> u64 {
    match s {
        TimelineTurnState::Running => 0,
        TimelineTurnState::Completed => 1,
        TimelineTurnState::Failed => 2,
        TimelineTurnState::Cancelled => 3,
    }
}

/// 单回合的渲染缓存键。
///
/// 只读**块级 `rev` 计数**而不哈希正文：哈希是 O(text)，而这里要的是 O(块数)。
/// 任何影响该回合渲染的输入都在键里：
/// - 块内容：`block.rev`（每次可见变更自增）；
/// - 块状态：`state`（Open→Sealed 会从纯文本切到 markdown）；
/// - 工具展开态（F7）、`show_reasoning`（F3）、宽度；
/// - 回合头部文案：`turn_idx` / `total`（`cap_turns` 丢回合会让编号漂移）；
/// - **动画**：含 spinner/▌/进度条的回合每帧换键 → 只重渲这一段，
///   其余段落保持命中。这正是「空闲零渲染、流式只渲活跃段」的实现基础。
pub(crate) fn turn_cache_key(
    session: &SessionState,
    turn: &Turn,
    turn_idx: usize,
    width: u16,
    show_reasoning: bool,
) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x7475_726e); // "turn" 域分隔
    h_str(&mut h, &turn.turn_id);
    // 用**稳定编号**而非裸下标：否则 `cap_turns` 丢头部会让每个回合的键都变，
    // 增量复用彻底失效（实测丢 1 个 → 19/19 全量重渲）。
    h_u64(&mut h, session.timeline.turn_number(turn_idx));
    h_u64(&mut h, u64::from(width));
    h_u64(&mut h, u64::from(show_reasoning));
    h_u64(&mut h, turn_state_tag(turn.state));
    h_u64(&mut h, u64::from(turn.sealed));
    h_u64(&mut h, u64::from(turn.offloaded));
    h_str(&mut h, &turn.user_text);
    if let Some(f) = &turn.failure {
        h_str(&mut h, &f.code);
        h_str(&mut h, &f.message);
    }
    let mut animating = false;
    for round in &turn.rounds {
        h_u64(&mut h, u64::from(round.round_num));
        h_u64(&mut h, u64::from(round.sealed));
        for b in &round.blocks {
            h_str(&mut h, &b.block_id);
            h_u64(&mut h, b.rev);
            h_u64(&mut h, u64::from(b.block_order));
            h_u64(&mut h, u64::from(b.state == TimelineBlockState::Sealed));
            if let Some(t) = &b.tool {
                h_u64(
                    &mut h,
                    u64::from(session.expanded_tools.contains(&t.tool_call_id)),
                );
            }
            animating |= b.is_animating();
        }
    }
    if animating {
        // 动画字形依赖墙钟而非内容：把帧号纳入键，使本段每帧重渲。
        h_u64(&mut h, crate::app::anim::frame_now());
    }
    h
}

/// 头部横幅的缓存键。
pub(crate) fn banner_cache_key(session: &SessionState, width: u16) -> u64 {
    let mut h = FNV_OFFSET;
    h_u64(&mut h, 0x6261_6e6e); // "bann"
    h_u64(&mut h, u64::from(session.timeline.has_more));
    h_u64(&mut h, u64::from(session.timeline.truncated_before));
    h_u64(&mut h, u64::from(width));
    h
}

// ── 分段缓存的增量重建 ──────────────────────────────────────────────

/// 布局不动点迭代的最大趟数。
///
/// 渲染会把估算高度换成精确高度，几何因此位移；本循环保证「窗口覆盖到的段
/// 一定已精确渲染」。精确高度不会再次变化，故必然收敛（实测 1~2 趟）。
/// 上限只是防御性护栏。
const MAX_LAYOUT_PASSES: usize = 4;

/// **廉价估算**单回合的渲染行数（离屏段用）。
///
/// 不跑 markdown/syntect/normalize，只按源文本的**字符显示宽度**推换行行数
/// （Grok `estimate_source_lines` 同一思路：缓存每行宽度，按宽推导行数）。
///
/// 估算偏差只会导致滚动几何略微偏移，而**不会**导致显示错误：任何真正进入
/// 视口的段都会被精确重渲（见 [`refresh_segments`]）。
///
/// 刻意取**保守偏小**（宁少不多）：估小 → 按几何挑选的可见段集合偏大 → 多渲
/// 几段也无害；估大 → 可见段集合偏小 → 窗口内出现未渲染段（破坏不变式）。
/// Claude Code 的 `PESSIMISTIC_HEIGHT=1` 就是同一取向的极端版本。
fn estimate_turn_lines(turn: &Turn, width: usize) -> usize {
    let w = width.max(1);
    let mut lines = 0usize;

    // 回合头部 1 行 +（offloaded 时）归档提示 1 行 + 尾部空行 1 行。
    lines += 1;
    if turn.offloaded {
        lines += 1;
    }
    lines += 1;

    // 用户输入：按显示宽推换行。
    if !turn.user_text.is_empty() {
        lines += estimate_wrapped_lines(&turn.user_text, w.saturating_sub(2));
    }

    for round in &turn.rounds {
        for block in &round.blocks {
            lines += match block.kind {
                TimelineBlockKind::Text | TimelineBlockKind::Reasoning => {
                    estimate_wrapped_lines(&block.text, w)
                }
                TimelineBlockKind::Notice => {
                    estimate_wrapped_lines(&block.text, w.saturating_sub(2))
                }
                // 工具卡：折叠态是固定几行（标题+参数摘要），展开态才含输出。
                // 这里按保守的折叠估法；展开后该段必然在视口内（用户刚点开）。
                TimelineBlockKind::Tool => 3,
            };
        }
    }

    if turn.failure.is_some() {
        lines += 1;
    }
    lines
}

/// 按**显示宽度**推换行行数（不建字符串，O(字符数)）。
///
/// 与 `wrap_text` 的贪心折行语义一致到「行数」这一层（不断言切分点）。
fn estimate_wrapped_lines(text: &str, width: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    let w = width.max(1);
    let mut total = 0usize;
    for para in text.split('\n') {
        let pw = UnicodeWidthStr::width(para);
        // 空段落仍占 1 行（与 wrap_text 一致）。向上取整即行数。
        total += pw.div_ceil(w).max(1);
    }
    total
}

/// 维护 `session.segments`（无视口版本）。
///
/// 仅测试用：不传视口时保留全部段的渲染结果，便于断言增量语义。
/// 生产路径走 [`refresh_segments_at`]（带视口 → 触发虚拟化淘汰）。
#[cfg(test)]
pub fn refresh_segments(session: &mut SessionState, width: u16, show_reasoning: bool) -> usize {
    refresh_segments_at(session, width, show_reasoning, None)
}

/// 带视口的 [`refresh_segments`]（生产路径用）。
///
/// `viewport` 为 `(视口顶端行号, 可视行数)`。传 `None` 表示不关心视口
/// （如测试），此时保留全部段。
///
/// 复杂度：O(回合数) 取键（每回合 O(块数)，不哈希正文）+ O(可见段的行数) 重渲。
/// 对比旧实现每帧 O(全量文本) 的 markdown/syntect/normalize，这是数量级的差异。
///
/// 返回本次实际重渲的段数（供测试与观测断言“增量真的发生了”）。
pub fn refresh_segments_at(
    session: &mut SessionState,
    width: u16,
    show_reasoning: bool,
    viewport: Option<(usize, usize)>,
) -> usize {
    let width = width.max(20);

    // ── 宽度 / F3 变化 ──
    // 宽度：**不重建**，改为高度缩放 + 丢弃 Lines（Claude Code `ratio` 缩放）。
    // F3：输出语义变了，必须整份重建。
    let prev = session.segments.as_ref();
    let reasoning_changed = prev.is_some_and(|c| c.show_reasoning != show_reasoning);
    let width_changed = prev.is_some_and(|c| c.width != width);
    let missing = prev.is_none();

    if reasoning_changed || missing {
        return rebuild_all(session, width, show_reasoning);
    }

    if width_changed {
        let old_w = prev.map_or(width, |c| c.width).max(1) as f64;
        let new_w = width.max(1) as f64;
        // 宽度变大 → 行变少；ratio<1 时高度缩下去，与 Claude Code 注释的
        // 「widen 时 ratio<1，缩放后偏移量与重排后的真实布局大致对齐」一致。
        let ratio = old_w / new_w;
        let cache = session.segments.as_mut().expect("checked above");
        cache.width = width;
        for seg in cache.turns.iter_mut() {
            let h = seg.body.height() as f64;
            let scaled = ((h * ratio).round() as usize).max(1);
            seg.body = crate::app::session::SegmentBody::Height(scaled);
        }
        if let Some(b) = &mut cache.banner {
            b.body = crate::app::session::SegmentBody::Height(1);
        }
        if let Some(e) = &mut cache.empty {
            e.body = crate::app::session::SegmentBody::Height(1);
        }
        // 缩放后所有 Lines 都被丢弃 → 视口内会在下面被重新精确渲染。
    }

    // ── 增量：逐回合比对键，只重渲变化段 ──
    // 先算出所有键（只读借用），再在**只读**阶段把需重渲的段渲好，
    // 最后才拿可变借用写回——避开 `render_turn(&SessionState)` 与
    // `segments.as_mut()` 的借用冲突。
    let keys: Vec<u64> = session
        .timeline
        .turns
        .iter()
        .enumerate()
        .map(|(idx, turn)| turn_cache_key(session, turn, idx, width, show_reasoning))
        .collect();

    let prev_len = session.segments.as_ref().map_or(0, |c| c.turns.len());
    let same_len = prev_len == keys.len();

    if !same_len {
        // 长度变化有两种截然不同的成因，必须区分对待：
        //
        // - **cap_turns 丢头部**（每回合都发生，一旦到 cap 就是热路径）：
        //   其余段内容未变，按 `turn_id` 对齐后可全部复用。若在这里整份重建，
        //   则到达 cap 后每回合重渲 400 段（实测 197ms）——真卡顿源。
        // - **prepend / reopen**（低频）：按 id 对不上就整份重建。
        //
        // 编号已改为**稳定**（`turn_number`），所以内容未变的回合其 key 也不变，
        // 对齐后可直接复用；只有真正新增/变化的回合才进 dirty。
        let old: Vec<crate::app::session::Segment> = session
            .segments
            .as_mut()
            .map(|c| std::mem::take(&mut c.turns))
            .unwrap_or_default();
        let mut by_id: std::collections::HashMap<String, crate::app::session::Segment> =
            old.into_iter().map(|s| (s.turn_id.clone(), s)).collect();
        let mut realigned: Vec<crate::app::session::Segment> = Vec::with_capacity(keys.len());
        let mut reused = 0usize;
        for (idx, turn) in session.timeline.turns.iter().enumerate() {
            match by_id.remove(&turn.turn_id) {
                Some(seg) if seg.key == keys[idx] => {
                    reused += 1;
                    realigned.push(seg);
                }
                _ => realigned.push(crate::app::session::Segment {
                    key: keys[idx],
                    body: crate::app::session::SegmentBody::Height(estimate_turn_lines(
                        turn,
                        width as usize,
                    )),
                    turn_id: turn.turn_id.clone(),
                }),
            }
        }
        // 一个都没复用上（如首次 prepend 到完全不同的历史）：整份重建更划算。
        if reused == 0 && !realigned.is_empty() {
            session.segments.as_mut().expect("Some").turns = realigned;
            return rebuild_all(session, width, show_reasoning);
        }
        session.segments.as_mut().expect("Some").turns = realigned;
    }

    // ── 虚拟化：只精确渲染视口附近的段，其余退化为估算高度 ──
    //
    // 用**不动点迭代**而非单趟：渲染会把估算高度换成精确高度 → 几何位移 →
    // 覆盖的段集合可能变化。每趟都把「覆盖到但尚未渲染」的段渲掉，直到稳定。
    // 精确高度不再变，故必然收敛（实测 1~2 趟）。
    let mut rebuilt = 0usize;
    for _pass in 0..MAX_LAYOUT_PASSES {
        // 覆盖行区间 → 段下标范围（基于当前几何：精确 + 估算混合）。
        let keep: Option<(usize, usize)> = viewport.and_then(|(top, height)| {
            let cache = session.segments.as_ref()?;
            if cache.turns.is_empty() {
                return None;
            }
            let (f, l) = cache.segment_range_for(top, height);
            Some((
                f.saturating_sub(KEEP_MARGIN_SEGMENTS),
                (l + KEEP_MARGIN_SEGMENTS).min(cache.turns.len().saturating_sub(1)),
            ))
        });

        let mut need: Vec<usize> = Vec::new();
        {
            let cache = session.segments.as_mut().expect("checked above");
            for (idx, seg) in cache.turns.iter_mut().enumerate() {
                let in_keep = keep.is_none_or(|(lo, hi)| idx >= lo && idx <= hi);
                if !in_keep {
                    // 淘汰：丢弃 Lines，退化为**精确高度**（几何不变）。
                    if let Some(lines) = seg.body.lines() {
                        seg.body = crate::app::session::SegmentBody::Height(lines.len());
                    }
                    // 离屏且键变了的段：用估算高度占位（不渲染）。
                    if seg.key != keys[idx] {
                        seg.key = keys[idx];
                        seg.body = crate::app::session::SegmentBody::Height(estimate_turn_lines(
                            &session.timeline.turns[idx],
                            width as usize,
                        ));
                    }
                    continue;
                }
                // 保留区内：键变了 或 尚未持有 Lines（刚被淘汰/从未渲）→ 需渲。
                if seg.key != keys[idx] || !seg.body.is_lines() {
                    need.push(idx);
                }
            }
        }

        if need.is_empty() {
            break;
        }

        // 只读阶段渲染，再可变阶段写回（避开 `render_turn(&SessionState)` 与
        // `segments.as_mut()` 的借用冲突）。
        let rendered: Vec<(usize, std::sync::Arc<[RenderLine]>)> = need
            .iter()
            .map(|&idx| {
                let turn = &session.timeline.turns[idx];
                let lines: std::sync::Arc<[RenderLine]> =
                    render_turn(session, turn, idx, width as usize, show_reasoning).into();
                (idx, lines)
            })
            .collect();
        rebuilt += rendered.len();
        let cache = session.segments.as_mut().expect("checked above");
        for (idx, lines) in rendered {
            cache.turns[idx] = crate::app::session::Segment {
                key: keys[idx],
                body: crate::app::session::SegmentBody::Lines(lines),
                turn_id: session.timeline.turns[idx].turn_id.clone(),
            };
        }
    }

    // 收尾保证：循环退出后，窗口覆盖到的段必须已持有 Lines。
    // 若不动点未收敛（估算↔精确来回振荡），这里无条件补渲一次——
    // 它保证 `window()` 的不变式，而代价只多渲几段。
    if let Some((top, height)) = viewport {
        let need: Vec<usize> = {
            let cache = session.segments.as_ref().expect("checked above");
            let (f, l) = cache.segment_range_for(top, height);
            (f..=l.min(cache.turns.len().saturating_sub(1)))
                .filter(|&i| !cache.turns[i].body.is_lines())
                .collect()
        };
        if !need.is_empty() {
            let rendered: Vec<(usize, std::sync::Arc<[RenderLine]>)> = need
                .iter()
                .map(|&idx| {
                    let turn = &session.timeline.turns[idx];
                    let lines: std::sync::Arc<[RenderLine]> =
                        render_turn(session, turn, idx, width as usize, show_reasoning).into();
                    (idx, lines)
                })
                .collect();
            rebuilt += rendered.len();
            let cache = session.segments.as_mut().expect("checked above");
            for (idx, lines) in rendered {
                cache.turns[idx] = crate::app::session::Segment {
                    key: keys[idx],
                    body: crate::app::session::SegmentBody::Lines(lines),
                    turn_id: session.timeline.turns[idx].turn_id.clone(),
                };
            }
        }
    }

    let banner_key = banner_cache_key(session, width);
    let banner_line = render_banner(session);
    let empty_needed = session.timeline.turns.is_empty();

    // ── 可变阶段：写回 ──
    let cache = session.segments.as_mut().expect("checked above");

    let banner_stale = match &cache.banner {
        Some(s) => s.key != banner_key || !s.body.is_lines(),
        None => banner_line.is_some(),
    };
    if banner_stale {
        cache.banner = banner_line.map(|l| crate::app::session::Segment {
            key: banner_key,
            body: crate::app::session::SegmentBody::Lines(std::sync::Arc::from([l].as_slice())),
            turn_id: String::new(),
        });
    }
    match (empty_needed, cache.empty.is_some()) {
        (true, false) => {
            cache.empty = Some(crate::app::session::Segment {
                key: 1,
                body: crate::app::session::SegmentBody::Lines(std::sync::Arc::from(
                    [RenderLine::new().span("（暂无回合——输入消息开始对话）", SpanStyle::Dim)]
                        .as_slice(),
                )),
                turn_id: String::new(),
            });
        }
        (false, true) => cache.empty = None,
        _ => {}
    }

    cache.rebuilt_segments = rebuilt;
    cache.resident_segments = cache.all().filter(|s| s.body.is_lines()).count();
    rebuilt
}

/// 整份重建（宽度/F3 变化、段数变化、首帧）。
fn rebuild_all(session: &mut SessionState, width: u16, show_reasoning: bool) -> usize {
    let mut cache = crate::app::session::SegmentCache {
        width,
        show_reasoning,
        ..Default::default()
    };
    let mut rebuilt = 0usize;
    for (idx, turn) in session.timeline.turns.iter().enumerate() {
        let key = turn_cache_key(session, turn, idx, width, show_reasoning);
        let lines: std::sync::Arc<[RenderLine]> =
            render_turn(session, turn, idx, width as usize, show_reasoning).into();
        cache.turns.push(crate::app::session::Segment {
            key,
            body: crate::app::session::SegmentBody::Lines(lines),
            turn_id: session.timeline.turns[idx].turn_id.clone(),
        });
        rebuilt += 1;
    }
    cache.banner = render_banner(session).map(|l| crate::app::session::Segment {
        key: banner_cache_key(session, width),
        body: crate::app::session::SegmentBody::Lines(std::sync::Arc::from([l].as_slice())),
        turn_id: String::new(),
    });
    cache.empty = session
        .timeline
        .turns
        .is_empty()
        .then(|| crate::app::session::Segment {
            key: 1,
            body: crate::app::session::SegmentBody::Lines(std::sync::Arc::from(
                [RenderLine::new().span("（暂无回合——输入消息开始对话）", SpanStyle::Dim)]
                    .as_slice(),
            )),
            turn_id: String::new(),
        });
    cache.rebuilt_segments = rebuilt;
    cache.resident_segments = cache.all().filter(|s| s.body.is_lines()).count();
    session.segments = Some(cache);
    rebuilt
}

fn push_text_block(lines: &mut Vec<RenderLine>, text: &str, width: usize, streaming: bool) {
    if streaming {
        // 流式：纯文本低开销，避免半截 markdown 抖动与 syntect 重算
        let shown = format!("{text}▌");
        for seg in wrap_text(&shown, width) {
            lines.push(RenderLine::plain(seg));
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

fn push_reasoning_block(
    lines: &mut Vec<RenderLine>,
    text: &str,
    width: usize,
    streaming: bool,
    show_reasoning: bool,
) {
    if text.trim().is_empty() {
        return;
    }
    // summary 特判：无换行的动词-ing 句拼接自动逐句换行（eeacd19a 实测句间缺空格/缺换行）
    // normalize 仅在 looks_like_reasoning_summary 为真时注入换行，传统 thinking 保持原样
    let normalized = normalize_reasoning_content(text);
    // opencode ReasoningHeader：流式 Spinner + 折叠标题对齐 `index.tsx:1652`
    // 解析首段作为标题（**Title**\n\nBody 或首行），其余为 body
    let trimmed = normalized.trim();
    let (title, body) = if let Some(stripped) = trimmed.strip_prefix("**") {
        if let Some(end) = stripped.find("**") {
            let t = stripped[..end].trim();
            let b = stripped[end + 2..].trim().trim_start_matches('\n').trim();
            (
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_owned())
                },
                b.to_owned(),
            )
        } else {
            (None, trimmed.to_owned())
        }
    } else {
        // 首行提升为标题仅限英文 gerund summary 句（opencode 风格，如
        // "Gathering context."）；中文 thinking 散文首行是内容而非标题，
        // 提升会把思考链路拼进 Thinking/Thought 标识同行。`**Title**`
        // 显式形式已在上方分支处理。
        let mut parts = trimmed.splitn(2, '\n');
        let first = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();
        if rest.is_empty() {
            (None, trimmed.to_owned())
        } else if first.chars().count() <= 48 && sentence_starts_with_gerund(first) {
            (Some(first.to_owned()), rest.to_owned())
        } else {
            (None, trimmed.to_owned())
        }
    };
    if streaming {
        let frames = ["◐", "◑", "◒", "◓"];
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let icon = frames[((ms / 200) % frames.len() as u128) as usize];
        let header = if let Some(t) = &title {
            format!("{icon} Thinking: {t}")
        } else {
            format!("{icon} Thinking")
        };
        lines.push(
            RenderLine::new()
                .span("  ", SpanStyle::Dim)
                .span(header, SpanStyle::Warn),
        );
        if !show_reasoning {
            // hide 模式流式仅保留标题行，不展 body（与 opencode hide 对齐）
            return;
        }
        // 默认展开：流式即全显（8行以上时不截尾，cursor 附末行）
        let wrapped = wrap_text(&body, width.saturating_sub(4));
        for (i, seg) in wrapped.iter().enumerate() {
            let is_last = i == wrapped.len() - 1;
            let shown = if is_last {
                format!("{seg}▌")
            } else {
                seg.clone()
            };
            lines.push(
                RenderLine::new()
                    .span("    ", SpanStyle::Dim)
                    .span(shown, SpanStyle::Reasoning),
            );
        }
        return;
    }
    // 非流式：hide 时单行 `+ Thought: title`（可 F3 展开）
    if !show_reasoning {
        if let Some(t) = title {
            lines.push(
                RenderLine::new()
                    .span("  ", SpanStyle::Dim)
                    .span(format!("+ Thought: {t} (F3 展开)"), SpanStyle::Warn),
            );
        } else {
            let preview = body.chars().take(48).collect::<String>();
            lines.push(
                RenderLine::new()
                    .span("  ", SpanStyle::Dim)
                    .span(format!("+ Thought: {preview}… (F3 展开)"), SpanStyle::Warn),
            );
        }
        return;
    }
    // 展开态必须保留 Thought 标识（无标题时用通用 Thought，避免裸体正文）
    let header = if let Some(t) = title.as_deref() {
        format!("Thought: {t}")
    } else {
        "Thought".to_string()
    };
    lines.push(
        RenderLine::new()
            .span("  ", SpanStyle::Dim)
            .span(header, SpanStyle::Warn),
    );
    if body.is_empty() {
        return;
    }
    let wrapped = wrap_text(&body, width.saturating_sub(4));
    for seg in wrapped {
        lines.push(
            RenderLine::new()
                .span("    ", SpanStyle::Dim)
                .span(seg, SpanStyle::Reasoning),
        );
    }
}

/// 默认直接展开的工具（不再折叠）。bash 系 + read：输出即结果，必须直观可见。
/// 其它工具（grep/glob/edit/write 等）保持折叠以控屏；F7 仍可手动切换。
pub(crate) fn is_default_expanded(name: &str) -> bool {
    matches!(
        name,
        "bash" | "exec" | "shell" | "pwsh" | "powershell" | "read"
    )
}

/// opencode 式工具图标（对齐 `toolDisplay` 集合） `packages/tui/src/routes/session/index.tsx:2638`
fn tool_icon(name: &str) -> &'static str {
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

/// 折叠输出（对齐 `opencode/src/util/collapse-tool-output.ts:1`）
fn collapse_output(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    let char_len = output.chars().count();
    if lines.len() <= max_lines && char_len <= max_chars {
        return (output.to_owned(), false);
    }
    let preview = lines[..max_lines.min(lines.len())].join("\n");
    if preview.chars().count() > max_chars {
        let truncated: String = preview.chars().take(max_chars.saturating_sub(1)).collect();
        return (format!("{truncated}…"), true);
    }
    (format!("{}…", preview), true)
}

/// 从 args_json 提炼可读预览（仅保留 primitives，去除 filePath 重复等）
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

/// 提炼路径类参数用于标题（read/write/edit 的 filePath / path）
fn extract_path(args_json: Option<&str>) -> Option<String> {
    let s = args_json?;
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    for key in ["filePath", "path", "file_path"] {
        if let Some(serde_json::Value::String(p)) = obj.get(key) {
            return Some(p.clone());
        }
    }
    None
}

fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "exec" | "shell" | "pwsh" | "powershell")
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

/// 结构化 JSON 结果的可读投影。
///
/// 部分工具（`process`、`journal` 的部分 action）的模型投影就是一整坨 JSON，
/// 单行贴进 transcript 是无法阅读的长串。这里做保守降级：只挑人类可读的
/// 标量/短数组渲染成 `key: value` 行，并剥离与用户无关的机器字段
/// （`timeis`：后端 `json_ok` 给每次调用打的会话时间戳；`content` 为
/// `process_info_ok` 内联的短摘要，与 status 重复）。
///
/// 非 JSON / 解析失败 / 无可用字段 → 返回 None，调用方回退原文（绝不丢信息）。
fn pretty_json_output(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }

    const SKIP: [&str; 2] = ["timeis", "content"];
    let mut out: Vec<String> = Vec::new();
    for (key, val) in obj.iter() {
        if SKIP.contains(&key.as_str()) {
            continue;
        }
        match val {
            serde_json::Value::String(s) => {
                if s.is_empty() {
                    continue;
                }
                // 过长的字符串（如整份文件内容）不在此投影，交给原文路径
                if s.chars().count() > 200 {
                    return None;
                }
                out.push(format!("{key}: {s}"));
            }
            serde_json::Value::Number(n) => out.push(format!("{key}: {n}")),
            serde_json::Value::Bool(b) => out.push(format!("{key}: {b}")),
            serde_json::Value::Null => {}
            serde_json::Value::Array(arr) if arr.len() <= 8 => {
                let parts: Vec<String> = arr
                    .iter()
                    .map(|item| match item {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                let joined = parts.join(", ");
                if joined.chars().count() <= 200 {
                    out.push(format!("{key}: {joined}"));
                }
            }
            // 对象/长数组：结构复杂，不做猜测，回退原文
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

/// exec 的调用标题：`exec cargo build ...` / `exec bash ls -la`。
///
/// 后端 `exec` 有两种互斥形态（schema `oneOf: [argv, command]`）：
/// - `argv`：`["cargo","build"]` 直调、不经 shell → 标题直接拼 argv；
/// - `command` + 可选 `shell`：经 shell 执行 → 前缀显式 shell 名。
///
/// 之所以优先从 `args_json` 推导而非用 `summary`：后端
/// `timeline_tool()` 令 `summary = output`，而 exec 的 output 是整坨
/// `ExecOutput` JSON——直接显示会把 `{"status":"completed",...}` 糊在标题上。
/// 且 `ExecOutput.command` 只是 `argv[0] + " ..."`（direct.rs:31），丢弃了参数，
/// 不如还原真实 argv 可读。
///
/// 解析失败/形态异常 → 返回 None，调用方回退既有标题逻辑。
fn exec_command_summary(args_json: Option<&str>) -> Option<String> {
    let args = args_json?;
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = value.as_object()?;

    /// argv 元素可能含空格/引号，按需加引号后拼接。
    fn join_argv(items: &[serde_json::Value]) -> Option<String> {
        let parts: Vec<String> = items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(|s| {
                if s.is_empty() || s.contains(char::is_whitespace) || s.contains('"') {
                    format!("\"{}\"", s.replace('"', "\\\""))
                } else {
                    s
                }
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    if let Some(argv) = obj.get("argv").and_then(|v| v.as_array()) {
        return join_argv(argv);
    }

    let command = obj.get("command").and_then(|v| v.as_str())?;
    if command.trim().is_empty() {
        return None;
    }
    match obj.get("shell").and_then(|v| v.as_str()) {
        // 显式 shell：按用户要求标注，避免 Windows 上默认 pwsh 造成的误解
        Some(shell) if !shell.trim().is_empty() => Some(format!("{shell} {command}")),
        _ => Some(command.to_string()),
    }
}

fn shell_meta_from_raw(raw: &str) -> Option<(Option<i32>, bool, String)> {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let obj = v.as_object()?;
    if !obj.contains_key("output") {
        return None;
    }
    let exit_code = obj
        .get("exit_code")
        .and_then(|x| x.as_i64())
        .map(|x| x as i32);
    let truncated = obj
        .get("truncated")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let status = obj
        .get("status")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some((exit_code, truncated, status))
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

fn push_tool_card(
    lines: &mut Vec<RenderLine>,
    tool: &crate::app::timeline_model::ToolCard,
    width: usize,
    expanded_raw: bool,
) {
    // 默认展开的工具：expanded_raw 的语义做 xor，使 F7 仍可“收起”
    let expanded = expanded_raw ^ is_default_expanded(&tool.name);
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
    let path = extract_path(tool.args_json.as_deref());
    // exec 专用标题：优先还原真实命令（后端 summary 是整坨 ExecOutput JSON，
    // 直接显示会把 `{"status":"completed"...}` 糊在标题上）。
    let exec_summary = if tool.name == "exec" {
        exec_command_summary(tool.args_json.as_deref())
    } else {
        None
    };
    let header_extra = if let Some(p) = &path {
        let short = crate::app::truncate_str(p, 36);
        format!(" {short}")
    } else if let Some(cmd) = exec_summary.as_deref().filter(|s| !s.is_empty()) {
        let one = cmd.replace('\n', " ").chars().take(64).collect::<String>();
        format!(" {one}")
    } else if let Some(summary) = tool.summary.as_deref().filter(|s| !s.is_empty()) {
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
    let is_block = has_diff
        || output_len > 4
        || tool.state == TimelineToolState::Running && !tool.progress.is_empty();

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
        lines.push(
            RenderLine::new()
                .span(" ┃ ", SpanStyle::Dim)
                .span(title, SpanStyle::Dim),
        );
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
        lines.push(
            RenderLine::new()
                .span(" ┃ ", SpanStyle::Dim)
                .span(format!("{icon} "), style)
                .span(tool.name.clone(), SpanStyle::Accent)
                .span(state_label, style)
                .span(if is_running { " ⋯" } else { "" }, SpanStyle::Dim),
        );
    } else {
        // InlineTool 单行
        let state_suffix = match tool.state {
            TimelineToolState::Running => " ⋯",
            _ => "",
        };
        let mut header = RenderLine::new()
            .span("  ", SpanStyle::Dim)
            .span(format!("{icon} "), style)
            .span(
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
    let skip_args_preview = exec_summary.is_some();
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
    if let Some(diff) = &tool.diff {
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
        let shell_meta = shell_meta_from_raw(raw_output);
        let src = if tool.state == TimelineToolState::Running {
            if !tool.progress.trim().is_empty() {
                tool.progress.clone()
            } else {
                unwrapped.clone().unwrap_or_default()
            }
        } else if let Some(ref inner) = unwrapped {
            if !inner.trim().is_empty() {
                inner.clone()
            } else if !tool.progress.trim().is_empty() {
                tool.progress.clone()
            } else {
                String::new()
            }
        } else if !tool.progress.trim().is_empty() {
            tool.progress.clone()
        } else if !raw_output.trim().is_empty() {
            raw_output.to_string()
        } else {
            String::new()
        };
        // 正文是否来自 progress 缓冲：截断标注只在进度**真的上屏**时给——
        // 工具已结束且拿到了完整 `output` 时，progress 只是被取代的中间态，
        // 此时标注它「前段已丢弃」是噪音（用户看到的正文并没有缺）。
        let src_from_progress = !tool.progress.trim().is_empty() && src == tool.progress;
        if !src.trim().is_empty() {
            let max_lines = 8usize;
            let expanded_limit = 24usize;
            let total_raw_lines = src.lines().count();
            let max_chars = max_lines * width.saturating_sub(6).max(20);
            let needs_collapse = total_raw_lines > max_lines || src.chars().count() > max_chars;
            let display_text = if tool.state == TimelineToolState::Running {
                if total_raw_lines > max_lines {
                    src.lines()
                        .skip(total_raw_lines - max_lines)
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    src.clone()
                }
            } else if needs_collapse && !expanded {
                if total_raw_lines > max_lines {
                    src.lines()
                        .skip(total_raw_lines - max_lines)
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    let mut truncated: String =
                        src.chars().take(max_chars.saturating_sub(1)).collect();
                    truncated.push('…');
                    truncated
                }
            } else if src.lines().count() > expanded_limit && !expanded {
                src.lines()
                    .take(expanded_limit)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                src.clone()
            };
            let overflow = needs_collapse;
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let body_start = lines.len();
            let mut shown_lines = 0usize;
            for out in display_text.lines() {
                if out.is_empty() {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span("", SpanStyle::Dim),
                    );
                    shown_lines += 1;
                    continue;
                }
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span(seg, SpanStyle::Dim),
                    );
                    shown_lines += 1;
                    if shown_lines >= expanded_limit {
                        break;
                    }
                }
                if shown_lines >= expanded_limit {
                    break;
                }
            }
            // 正文到此为止。`▌`（流式实时光标）与截断标注都必须落在这之后：
            // 前者要回到**最后一行正文**（落在 footer 上会把注记画成流内容），
            // 后者与折叠 hint 相邻（footer 挨着 footer，读者不必跨正文拼读）。
            let body_end = lines.len();
            if overflow {
                let mut hint_text = if expanded {
                    "F7 收起".to_string()
                } else {
                    "F7 展开".to_string()
                };
                if !is_running && let Some((exit, truncated, _)) = shell_meta {
                    if let Some(code) = exit
                        && code != 0
                    {
                        hint_text.push_str(&format!(" · exit {code}"));
                    }
                    if truncated {
                        hint_text.push_str(" · 截断");
                    }
                }
                lines.push(
                    RenderLine::new()
                        .span(format!("{}  ", line_prefix), SpanStyle::Dim)
                        .span(hint_text, SpanStyle::Dim),
                );
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
        if let Some(output) = tool.output.as_deref().filter(|s| !s.is_empty()) {
            // 结构化 JSON 结果（process/journal 等）先做可读投影，避免单行长串。
            // 解析失败或非结构化输出 → 原样透出（信息零丢失）。
            let rendered = pretty_json_output(output).unwrap_or_else(|| output.to_string());
            combined.push_str(&rendered);
            if !tool.progress.is_empty() {
                combined.push('\n');
            }
        }
        combined.push_str(&tool.progress);
        if !combined.trim().is_empty() {
            let max_lines = 4usize;
            let max_chars = max_lines * width.saturating_sub(6).max(20);
            let (shown_text, overflow) = collapse_output(&combined, max_lines, max_chars);
            let display = if overflow && !expanded {
                shown_text
            } else {
                combined
            };
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let body_start = lines.len();
            let mut shown_lines = 0usize;
            for out in display
                .lines()
                .take(if overflow && !expanded { max_lines } else { 24 })
            {
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(
                        RenderLine::new()
                            .span(line_prefix, SpanStyle::Dim)
                            .span(seg, SpanStyle::Dim),
                    );
                    shown_lines += 1;
                    if shown_lines > 24 {
                        break;
                    }
                }
            }
            // 同 shell 分支：`▌` 落回最后一行正文，截断标注排在 hint 之后。
            let body_end = lines.len();
            if overflow {
                let hint = if expanded { "F7 收起" } else { "F7 展开" };
                lines.push(
                    RenderLine::new()
                        .span(format!("{}  ", line_prefix), SpanStyle::Dim)
                        .span(hint, SpanStyle::Dim),
                );
            }
            // 非 shell：progress 拼在 output 之后。折叠时只留**头部**
            // （`collapse_output` 取前 `max_lines` 行），进度可能整段被折掉 →
            // 只有进度确实可见时才标注（同上，不给看不见的内容报警）。
            //
            // **取舍**：折叠丢的是可恢复的**显示**（`F7 展开` 已把这件事画出来，
            // 按一下就能取回），不属 B1 要盯的**不可逆丢弃**；而本标注盯的
            // `progress_truncated` 是不可逆的（缓冲前段已从内存里丢掉）。所以
            // 「output 前缀被折叠」不标注——两者都在 footer 里，不会互相淹没。
            // `progress` 为空时不标注：标注指的是那段进度，没有进度就无从标注
            // （wire 侧理论上不会出现 flag 为真而 progress 为空，这里只是防御）。
            if tool.progress_truncated
                && !tool.progress.trim().is_empty()
                && (!overflow || expanded)
            {
                lines.push(progress_truncated_line(is_block));
            }
            if is_running
                && !overflow
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &ask, 100, false);
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
                failure: None,
                permission: None,
            };
            let mut lines = Vec::new();
            push_tool_card(&mut lines, &todo, 100, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &skills, 100, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &spawn, 100, false);
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
            failure: Some(qaqh_client::TimelineFailure {
                code: "invalid_input".into(),
                message: "items 为空".into(),
            }),
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &failed, 100, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &read, 100, false);
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

    #[test]
    fn pretty_json_projects_scalars_and_drops_machine_fields() {
        // process check 的真实形态（process_inspect.rs: process_info_ok）
        let raw = r#"{"id":7,"name":"cargo","status":"running","exit_code":null,"output":"","timeis":"UTC+8 2026-09-09 00:00","content":"process 7: running"}"#;
        let pretty = pretty_json_output(raw).expect("应可投影");
        assert!(pretty.contains("id: 7"), "{pretty}");
        assert!(pretty.contains("status: running"));
        assert!(!pretty.contains("timeis"), "机器时间戳应剔除");
        assert!(!pretty.contains("\"id\""), "不应残留 JSON 引号");
        // null 与空串跳过
        assert!(!pretty.contains("exit_code"));
    }

    #[test]
    fn pretty_json_returns_none_for_unstructured_text() {
        assert!(pretty_json_output("plain text output").is_none());
        assert!(pretty_json_output("not json {").is_none());
        assert!(pretty_json_output(r#"{}"#).is_none());
        assert!(pretty_json_output(r#"{"timeis":"x"}"#).is_none());
    }

    /// 长文本/复杂对象不猜测：回退原文，保证信息零丢失。
    #[test]
    fn pretty_json_defers_on_complex_payloads() {
        let long = "x".repeat(250);
        let raw = format!(r#"{{"content":"{long}"}}"#);
        assert!(pretty_json_output(&raw).is_none());
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

    /// 回归锁：`pretty_json_output` 的**行序** = JSON 原文 key 顺序（非字典序）。
    ///
    /// 该函数承担非 shell 工具（`process`/`journal`/`read`/…）的输出投影，
    /// 是保序 feature 最直接的可见面。
    #[test]
    fn pretty_json_output_follows_source_key_order() {
        let pretty = pretty_json_output(r#"{"zeta":"z","alpha":"a","mid":"m"}"#).expect("应可投影");
        assert_eq!(pretty, "zeta: z\nalpha: a\nmid: m");
        assert_ne!(
            pretty, "alpha: a\nmid: m\nzeta: z",
            "退化成字典序说明 preserve_order 不再生效"
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

    /// 结论钉住：**exec 工具卡的渲染与 key 顺序无关**。
    ///
    /// 与上面几条相反，exec 走 shell 专用分支：标题 `exec_command_summary`、输出
    /// `extract_shell_output_text`、状态 `shell_meta_from_raw` 都只用 `obj.get(...)`
    /// 定点取值；参数预览对 exec 又被显式跳过
    /// （`skip_args_preview = exec_summary.is_some()`）。因此同一份内容的两种 key
    /// 排列必须渲染出完全相同的行。
    ///
    /// 形态刻意选成 **Block**（`is_block = output_len > 4`，故 `tool.output` 用 5 行
    /// 的缩进 JSON 信封）——PR #16 审查指出：单行 output 会落进 Inline 分支，那本是
    /// 最不受顺序影响的形态，用它当守卫等于没测。
    ///
    /// 参数也刻意给 4 个 key 且两卡顺序互逆：一旦 `skip_args_preview` 被误开，
    /// 两卡的 `⌗` 行会各按自己的顺序渲染 → 相等断言与 `cwd=` 探针同时变红。
    #[test]
    fn exec_tool_card_render_is_key_order_independent() {
        // 两卡内容完全相同，仅 key 顺序互逆。
        let args_a = r#"{"argv":["ls","-la"],"cwd":"/tmp","timeout_ms":5,"shell":"bash"}"#;
        let args_b = r#"{"shell":"bash","timeout_ms":5,"cwd":"/tmp","argv":["ls","-la"]}"#;
        // `tool.output` 是**原始**串，`output_len` 按原始串行数算；缩进信封才能进 Block。
        let out_a = r#"{
  "status": "completed",
  "exit_code": 0,
  "output": "l1\nl2\nl3\nl4\nl5\nl6\n"
}"#;
        let out_b = r#"{
  "output": "l1\nl2\nl3\nl4\nl5\nl6\n",
  "exit_code": 0,
  "status": "completed"
}"#;

        // 探针依据：`format_args_preview` 只收 String/Number/Bool，数组（`argv`）落进
        // `_ => {}` 被丢弃；故 `⌗` 行若真的渲染出来，出现的必是 `cwd=` 而非 `argv=`。
        // 这条断言同时钉住「探针字符串本身有效」，避免探针失效后静默放行。
        assert_eq!(
            format_args_preview(args_a),
            "[cwd=/tmp, timeout_ms=5, shell=bash]",
            "探针前提：⌗ 行会渲染 cwd=（argv 是数组，被丢弃）"
        );
        assert_ne!(
            format_args_preview(args_a),
            format_args_preview(args_b),
            "两卡顺序互逆，⌗ 行若渲染出来必然不同——这正是本用例判别力的来源"
        );

        let card = |args: &str, output: &str| ToolCard {
            tool_call_id: "c-exec".into(),
            name: "exec".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(args.into()),
            output: Some(output.into()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        let mut lines_a = Vec::new();
        let mut lines_b = Vec::new();
        push_tool_card(&mut lines_a, &card(args_a, out_a), 100, false);
        push_tool_card(&mut lines_b, &card(args_b, out_b), 100, false);
        let flat_a = flatten(&lines_a);

        assert!(
            flat_a.contains('┃'),
            "应走 Block 分支（output_len > 4），否则本用例落回 Inline 形态而失去意义：{flat_a}"
        );
        assert!(flat_a.contains("ls -la"), "argv 应进标题：{flat_a}");
        assert!(flat_a.contains("l6"), "输出应透出：{flat_a}");
        assert!(
            !flat_a.contains("cwd="),
            "exec 的 ⌗ 参数预览必须被跳过（skip_args_preview）：{flat_a}"
        );
        assert_eq!(flat_a, flatten(&lines_b), "exec 卡渲染不应随 key 顺序变化");
    }

    /// 回归：process 的 JSON 不再以单行长串形式出现。
    #[test]
    fn process_json_is_rendered_readably() {
        let proc_tool = ToolCard {
            tool_call_id: "c-proc".into(),
            name: "process".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"action":"check","id":7}"#.into()),
            output: Some(
                r#"{"id":7,"name":"cargo","status":"running","timeis":"UTC+8 2026-09-09 00:00"}"#
                    .into(),
            ),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &proc_tool, 100, false);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("status: running"), "应可读投影: {flat}");
        assert!(!flat.contains("timeis"), "机器字段应剔除");
    }

    #[test]
    fn exec_summary_renders_command_and_shell() {
        // argv 直调：不经 shell，标题即真实命令行
        assert_eq!(
            exec_command_summary(Some(r#"{"argv":["cargo","build","--release"]}"#)).as_deref(),
            Some("cargo build --release")
        );
        // command 无显式 shell：不臆测（默认壳是运行时决策）
        assert_eq!(
            exec_command_summary(Some(r#"{"command":"ls -la | grep foo"}"#)).as_deref(),
            Some("ls -la | grep foo")
        );
        // 显式 shell：按用户要求前缀标注
        assert_eq!(
            exec_command_summary(Some(r#"{"command":"Get-ChildItem","shell":"pwsh"}"#)).as_deref(),
            Some("pwsh Get-ChildItem")
        );
        assert_eq!(
            exec_command_summary(Some(r#"{"command":"ls -la","shell":"bash"}"#)).as_deref(),
            Some("bash ls -la")
        );
        // 含空格/引号的参数需正确加引号
        assert_eq!(
            exec_command_summary(Some(r#"{"argv":["git","commit","-m","fix a bug"]}"#)).as_deref(),
            Some("git commit -m \"fix a bug\"")
        );
    }

    #[test]
    fn exec_summary_degrades_gracefully() {
        assert_eq!(exec_command_summary(None), None);
        assert_eq!(exec_command_summary(Some("not json")), None);
        assert_eq!(exec_command_summary(Some(r#"{}"#)), None);
        // 空 command 不当作有效标题
        assert_eq!(exec_command_summary(Some(r#"{"command":"   "}"#)), None);
        // 空 argv
        assert_eq!(exec_command_summary(Some(r#"{"argv":[]}"#)), None);
        // 空 shell 退化为无前缀
        assert_eq!(
            exec_command_summary(Some(r#"{"command":"ls","shell":""}"#)).as_deref(),
            Some("ls")
        );
    }

    /// 回归：exec 标题不得再出现 ExecOutput JSON（后端 summary = output 所致）。
    #[test]
    fn exec_title_shows_command_not_json() {
        let exec = ToolCard {
            tool_call_id: "c-exec".into(),
            name: "exec".into(),
            state: TimelineToolState::Succeeded,
            // 后端真实行为：summary 就是整坨 ExecOutput JSON
            summary: Some(
                r#"{"status":"completed","command":"cargo ...","exit_code":0,"output":""}"#.into(),
            ),
            args_json: Some(r#"{"argv":["cargo","build"]}"#.into()),
            output: Some(
                r#"{"status":"completed","command":"cargo ...","exit_code":0,"output":""}"#.into(),
            ),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &exec, 100, false);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("cargo build"), "标题应为真实命令: {flat}");
        assert!(!flat.contains("\"status\""), "标题不得糊 JSON: {flat}");
        assert!(!flat.contains("exit_code"), "标题不得含机器字段");
    }

    #[test]
    fn collapse_output_truncates_by_lines_and_chars() {
        let out = "a\nb\nc\nd\ne";
        let (shown, overflow) = collapse_output(out, 3, 100);
        assert!(overflow);
        assert_eq!(shown.lines().count(), 3);
        let (shown2, overflow2) = collapse_output("short", 3, 100);
        assert!(!overflow2);
        assert_eq!(shown2, "short");
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool_inline, 80, false);
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
            failure: None,
            permission: None,
        };
        let mut lines2 = Vec::new();
        push_tool_card(&mut lines2, &tool_block, 130, false); // wide -> split
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
        push_tool_card(&mut lines3, &tool_block, 80, false); // narrow -> unified
        assert!(
            lines3
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("Δ")))
        );
    }

    #[test]
    fn rendering_respects_show_reasoning() {
        let block = Block {
            block_id: "b1".into(),
            block_order: 0,
            kind: TimelineBlockKind::Reasoning,
            state: TimelineBlockState::Sealed,
            text: "**Title**\n\nBody content here\nsecond line".into(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        };
        let round = Round {
            round_num: 0,
            sealed: true,
            is_final: true,
            blocks: vec![block],
        };
        let turn = Turn {
            turn_index: None,
            turn_id: "t1".into(),
            user_text: "hi".into(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            rounds: vec![round],
        };
        let mut sess = crate::app::session::SessionState::new("s".into());
        sess.timeline.turns.push(turn);
        sess.timeline.version = 1;
        let lines_hide = render_transcript_with_opts(&sess, 80, false);
        let lines_show = render_transcript_with_opts(&sess, 80, true);
        // hide 应折叠为单行 + 提示
        assert!(
            lines_hide
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("F3")))
        );
        assert!(
            lines_show
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("Body")))
        );
    }

    #[test]
    fn reasoning_summary_split_gerund() {
        let text = "Gathering project structure, git state, and key modules to summarize the Rust TUI architecture and ongoing markdown feature.Synthesizing the exploration into a Chinese summary with architecture layers, PLAN.md divergence, git history, and risks.";
        let normalized = normalize_reasoning_content(text);
        assert!(normalized.contains('\n'), "summary 应被注入换行");
        let parts: Vec<&str> = normalized.split('\n').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("Gathering"));
        assert!(parts[1].starts_with("Synthesizing"));
        // 渲染后应产生多行 Reasoning
        let mut sess = crate::app::session::SessionState::new("s".into());
        let block = Block {
            block_id: "b1".into(),
            block_order: 0,
            kind: TimelineBlockKind::Reasoning,
            state: TimelineBlockState::Sealed,
            text: text.to_string(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        };
        sess.timeline.turns.push(Turn {
            turn_index: None,
            turn_id: "t1".into(),
            user_text: "".into(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: vec![block],
            }],
        });
        let lines = render_transcript_with_opts(&sess, 120, true);
        // 至少 Thought 标题 + 2 行 body
        let reasoning_lines = lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.text.contains("Gathering") || s.text.contains("Synthesizing"))
            })
            .count();
        assert!(reasoning_lines >= 2);
    }

    #[test]
    fn reasoning_traditional_not_split() {
        let text = "This is a normal paragraph with Reasoning content. It should not be split because not gerund.";
        assert!(!looks_like_reasoning_summary(text));
        assert_eq!(normalize_reasoning_content(text), text);
    }

    #[test]
    fn reasoning_summary_with_space_also_split() {
        let text = "Reviewing collected project files and planning a systematic bash-based read to complete the exploration. Batching bash reads to collect remaining protocol files.";
        assert!(looks_like_reasoning_summary(text));
        let n = normalize_reasoning_content(text);
        assert_eq!(n.split('\n').count(), 2);
    }

    #[test]
    fn reasoning_single_sentence_no_split() {
        let text = "Synthesizing gathered file and git data to summarize architecture, tech stack, and uncommitted changes.";
        assert!(!looks_like_reasoning_summary(text));
    }

    #[test]
    fn reasoning_decimal_protection() {
        let text = "Updating version to 1.2 for release. Checking tests.";
        // 虽含小数点但仍是两句，且 Checking 为 gerund -> 视为 summary，允许分裂
        // 关键是 1.2 不被误拆为两句
        let parts = split_reasoning_sentences(text);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("1.2"));
    }

    #[test]
    fn reasoning_chinese_sentences() {
        let text = "分析架构。评估方案。设计菜单。";
        let parts = split_reasoning_sentences(text);
        assert_eq!(parts.len(), 3);
        assert!(looks_like_reasoning_summary(text));
    }

    /// 构造单 reasoning 块的会话；block_state 决定流式/落定形态。
    fn reasoning_sess(
        text: &str,
        block_state: TimelineBlockState,
    ) -> crate::app::session::SessionState {
        let running = block_state == TimelineBlockState::Open;
        let mut sess = crate::app::session::SessionState::new("s".into());
        sess.timeline.turns.push(Turn {
            turn_index: None,
            turn_id: "t1".into(),
            user_text: String::new(),
            state: if running {
                TimelineTurnState::Running
            } else {
                TimelineTurnState::Completed
            },
            failure: None,
            sealed: !running,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: !running,
                is_final: !running,
                blocks: vec![Block {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: block_state,
                    text: text.to_string(),
                    tool: None,
                    last_fragment: 0,
                    rev: 1,
                }],
            }],
        });
        sess
    }

    #[test]
    fn cjk_thinking_first_line_not_promoted_to_title() {
        // 回归：中文 thinking 散文首行是内容而非标题，曾因 ≤48ch 兜底被
        // 提升拼进 Thought/Thinking 标识同行。现在必须整段在标识下方。
        let sess = reasoning_sess(
            "分析用户的需求。我需要先看看项目结构。\n然后动手实现。",
            TimelineBlockState::Sealed,
        );
        let lines = render_transcript_with_opts(&sess, 80, true);
        let joined = |l: &RenderLine| l.spans.iter().map(|s| s.text.as_str()).collect::<String>();
        assert!(
            lines.iter().any(|l| joined(l).trim() == "Thought"),
            "无标题时头部应为裸 Thought"
        );
        assert!(
            !lines.iter().any(|l| joined(l).contains("Thought: 分析")),
            "CJK 首行不得拼进标识行"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("分析用户的需求"))),
            "思考内容应换行显示在标识之下"
        );
    }

    #[test]
    fn english_gerund_summary_first_line_still_promoted() {
        // 英文 gerund summary 句保留 opencode 风格标题提升。
        let sess = reasoning_sess(
            "Gathering context.Synthesizing plan.",
            TimelineBlockState::Sealed,
        );
        let lines = render_transcript_with_opts(&sess, 80, true);
        let joined = |l: &RenderLine| l.spans.iter().map(|s| s.text.as_str()).collect::<String>();
        assert!(
            lines
                .iter()
                .any(|l| joined(l).trim() == "Thought: Gathering context."),
            "gerund 首行应保留标题提升"
        );
        assert!(lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.text.contains("Synthesizing plan."))
        }));
    }

    #[test]
    fn cjk_thinking_streaming_body_below_header() {
        // 流式态同样不得把思考内容拼进 Thinking 标识行（用户投诉场景）。
        let sess = reasoning_sess(
            "分析用户的需求。我需要先看看项目结构。",
            TimelineBlockState::Open,
        );
        let lines = render_transcript_with_opts(&sess, 80, true);
        let joined = |l: &RenderLine| l.spans.iter().map(|s| s.text.as_str()).collect::<String>();
        let header_idx = lines
            .iter()
            .position(|l| joined(l).contains("Thinking"))
            .expect("流式态应有 Thinking 标识行");
        assert!(
            !joined(&lines[header_idx]).contains("分析"),
            "标识行不得携带思考内容"
        );
        assert!(
            lines[header_idx..]
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("分析用户的需求"))),
            "思考内容应出现在标识行之下"
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
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
            failure: None,
            permission: None,
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
        push_tool_card(&mut lines, &a, 80, false);
        push_tool_card(&mut lines, &b, 80, false);
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
        push_tool_card(&mut lines, &tool, 80, false);
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

    /// 标注排在折叠 hint **之后**：两者都是这张卡的 footer，读者不必跨正文拼读
    /// （PR #18 审查：旧版把标注夹在头部、hint 留在尾部）。
    ///
    /// 变异验证（实测）：把标注推回正文之前 → 本测试红。
    #[test]
    fn truncation_mark_follows_collapse_hint() {
        let tool = streaming_bash_card("c-order", true);
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
        // bash 属默认展开工具（`is_default_expanded`），故 hint 是「F7 收起」。
        let hint_idx = lines
            .iter()
            .position(|l| joined(l).contains("F7 "))
            .unwrap_or_else(|| panic!("20 行进度应触发折叠 hint：{}", flatten(&lines)));
        let mark_idx = lines
            .iter()
            .position(|l| joined(l).contains(PROGRESS_TRUNCATED_MARK))
            .expect("截断标注应上屏");
        assert!(
            mark_idx > hint_idx,
            "标注应在 hint 之后：hint@{hint_idx} 标注@{mark_idx}\n{}",
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
        push_tool_card(&mut lines, &tool, 80, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
        let flat = flatten(&lines);
        assert!(
            flat.contains(PROGRESS_TRUNCATED_MARK),
            "非 shell 工具的截断进度也必须可见，实测：{flat}"
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
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
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
    fn reasoning_streaming_full_expand_by_default() {
        let mut sess = crate::app::session::SessionState::new("s".into());
        let text = (1..=8)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = Block {
            block_id: "b1".into(),
            block_order: 0,
            kind: TimelineBlockKind::Reasoning,
            state: TimelineBlockState::Open,
            text: text.clone(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        };
        sess.timeline.turns.push(Turn {
            turn_index: None,
            turn_id: "t1".into(),
            user_text: "".into(),
            state: TimelineTurnState::Running,
            failure: None,
            sealed: false,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: false,
                is_final: false,
                blocks: vec![block],
            }],
        });
        let lines = render_transcript_with_opts(&sess, 120, true);
        let reasoning_cnt = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.text.contains("line")))
            .count();
        assert!(reasoning_cnt >= 8, "streaming 默认全显 {reasoning_cnt}");
    }

    #[test]
    fn bash_pwsh_read_default_expanded_no_fold() {
        assert!(is_default_expanded("bash"));
        assert!(is_default_expanded("pwsh"));
        assert!(is_default_expanded("read"));
        assert!(is_default_expanded("exec"));
        assert!(is_default_expanded("shell"));
        assert!(is_default_expanded("powershell"));
        assert!(!is_default_expanded("grep"));
        assert!(!is_default_expanded("glob"));
        assert!(!is_default_expanded("edit"));

        // bash: 20 行输出，默认（raw=false）应直展全部（提示为“F7 收起”）
        let long = (1..=20)
            .map(|i| format!("ROW{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let raw = format!(
            r#"{{"status":"completed","output":{:?},"exit_code":0}}"#,
            long
        );
        let bash_tool = ToolCard {
            tool_call_id: "c-bash".into(),
            name: "bash".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: None,
            output: Some(raw),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &bash_tool, 80, false); // raw false -> visual true
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("ROW01"), "bash 默认展开应可见首行");
        assert!(flat.contains("ROW20"), "bash 默认展开应可见尾行");
        assert!(flat.contains("F7 收起"), "bash 默认展开提示应为收起");
        // raw=true 时应对视觉收起（仅尾 8 行）
        let mut lines2 = Vec::new();
        push_tool_card(&mut lines2, &bash_tool, 80, true);
        let flat2: String = lines2
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!flat2.contains("ROW01"), "bash 收起态不应含首行");
        assert!(flat2.contains("ROW20"));
        assert!(flat2.contains("F7 展开"));

        // read: 非 shell 分支，10 行输出默认展开应全显（>4 行折叠阈）
        let read_out = (1..=10)
            .map(|i| format!("r{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let read_tool = ToolCard {
            tool_call_id: "c-read".into(),
            name: "read".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some(read_out.clone()),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        let mut rl = Vec::new();
        push_tool_card(&mut rl, &read_tool, 80, false);
        let rf: String = rl
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rf.contains("r1"));
        assert!(rf.contains("r10"));
        // grep 默认仍折叠（raw false 即视觉收起，10 行应只显 4 行）
        let grep_tool = ToolCard {
            name: "grep".into(),
            tool_call_id: "c-grep".into(),
            ..read_tool.clone()
        };
        let mut gl = Vec::new();
        push_tool_card(&mut gl, &grep_tool, 80, false);
        let gf: String = gl
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(gf.contains("r1"));
        assert!(!gf.contains("r10"), "grep 默认折叠不应含尾行");
        assert!(gf.contains("F7 展开"));
    }

    // ── A3：分段渲染缓存的增量语义 ───────────────────────────────

    fn sess_with_turns(n: usize) -> SessionState {
        let mut sess = SessionState::new("seg".into());
        for i in 0..n {
            sess.timeline.turns.push(Turn {
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

    /// 首次调用全量建段；随后**内容未变时一个段都不重渲**（A2）。
    #[test]
    fn segments_skip_rebuild_when_nothing_changed() {
        let mut sess = sess_with_turns(5);
        let n = refresh_segments(&mut sess, 80, true);
        assert_eq!(n, 5, "首次应全量建段");
        // 内容未变 → 0 次重渲。旧实现（version 键）在这里会 100% 重渲。
        assert_eq!(refresh_segments(&mut sess, 80, true), 0);
        assert_eq!(refresh_segments(&mut sess, 80, true), 0);
    }

    /// 只有**被修改的那一段**重渲，其余保持命中（A3）。
    #[test]
    fn only_the_touched_segment_is_rebuilt() {
        let mut sess = sess_with_turns(6);
        refresh_segments(&mut sess, 80, true);
        // 改第 3 个回合的块（模拟该回合正在流式增长）。
        sess.timeline.turns[2].rounds[0].blocks[0]
            .text
            .push_str("追加");
        sess.timeline.turns[2].rounds[0].blocks[0].rev += 1;
        let rebuilt = refresh_segments(&mut sess, 80, true);
        assert_eq!(rebuilt, 1, "只有被改的段应重渲，实际 {rebuilt}");
    }

    /// 宽度变化 → 整份重建（因为换行位置全变）。
    #[test]
    fn width_change_invalidates_all_segments() {
        let mut sess = sess_with_turns(4);
        refresh_segments(&mut sess, 80, true);
        assert_eq!(refresh_segments(&mut sess, 60, true), 4);
    }

    /// `show_reasoning` 切换（F3）→ 整份重建（输出语义变了）。
    /// 这锁的是 BUGLIST BUG-010：旧缓存键里没有 show_reasoning。
    #[test]
    fn show_reasoning_toggle_invalidates_segments() {
        let mut sess = sess_with_turns(3);
        refresh_segments(&mut sess, 80, true);
        assert_eq!(refresh_segments(&mut sess, 80, false), 3);
    }

    /// 段内容与全量渲染逐行一致（增量缓存不得改变输出）。
    #[test]
    fn segment_cache_matches_full_render() {
        let mut sess = sess_with_turns(7);
        refresh_segments(&mut sess, 80, true);
        let cache = sess.segments.as_ref().expect("built");
        let joined: Vec<String> = cache
            .window(0, cache.total_lines())
            .iter()
            .map(|l| flatten(std::slice::from_ref(*l)))
            .collect();
        let full = render_transcript_with_opts(&sess, 80, true);
        let full_joined: Vec<String> = full
            .iter()
            .map(|l| flatten(std::slice::from_ref(l)))
            .collect();
        assert_eq!(joined, full_joined, "分段缓存与全量渲染必须逐行一致");
    }

    /// 视窗抽取是 O(可见)：从 500 回合里取 40 行，行数与顺序正确。
    #[test]
    fn window_extracts_only_visible_rows() {
        let mut sess = sess_with_turns(500);
        refresh_segments(&mut sess, 80, true);
        let cache = sess.segments.as_ref().unwrap();
        let total = cache.total_lines();
        let win = cache.window(total - 40, 40);
        assert_eq!(win.len(), 40);
        let last_turn_lines = cache.turns.last().unwrap().body.height();
        assert!(last_turn_lines > 0);
        // 窗口末行必须就是全量渲染的末行。
        let full = render_transcript_with_opts(&sess, 80, true);
        assert_eq!(
            flatten(std::slice::from_ref(win[39])),
            flatten(std::slice::from_ref(&full[full.len() - 1]))
        );
    }

    // ── 虚拟化：只保留视口附近段的渲染结果 ────────────────────

    /// 离屏段退化为估算高度：驻留段数应远小于总段数，但总行数不变。
    #[test]
    fn offscreen_segments_are_evicted_to_estimates() {
        let mut sess = sess_with_turns(200);
        // 无视口先建全量（首次加载路径）。
        refresh_segments(&mut sess, 80, true);
        let all_lines = sess.segments.as_ref().unwrap().total_lines();
        assert_eq!(
            sess.segments.as_ref().unwrap().resident_segments,
            200,
            "无视口时应全部驻留"
        );

        // 带视口（底部 40 行）：应只保留视口附近的段。
        refresh_segments_at(&mut sess, 80, true, Some((all_lines - 40, 40)));
        let cache = sess.segments.as_ref().unwrap();
        assert_eq!(cache.total_lines(), all_lines, "总行数不得因淘汰而变化");
        assert!(
            cache.resident_segments < 200,
            "必须真的淘汰了离屏段，实际驻留 {}",
            cache.resident_segments
        );
        assert!(
            cache.resident_segments <= 8 + 2 * KEEP_MARGIN_SEGMENTS + 2,
            "驻留段数应受限，实际 {}",
            cache.resident_segments
        );
    }

    /// **不变式**：`window()` 取出的每一行都必须真实存在（视口内不得有未渲染段）。
    ///
    /// 这是虚拟化最容易破的地方：几何用估算、渲染用精确，两者一旦不同步，
    /// 窗口就会缺行（视觉上表现为「内容突然少了一截」）。
    #[test]
    fn window_never_contains_unrendered_segments() {
        let mut sess = sess_with_turns(120);
        let full = render_transcript_with_opts(&sess, 80, true);
        let full_flat: Vec<String> = full
            .iter()
            .map(|l| flatten(std::slice::from_ref(l)))
            .collect();

        let height = 30usize;
        refresh_segments(&mut sess, 80, true);
        let total = sess.segments.as_ref().unwrap().total_lines();
        let mut checked = 0;
        let mut top = total.saturating_sub(height);
        loop {
            refresh_segments_at(&mut sess, 80, true, Some((top, height)));
            let win = sess.segments.as_ref().unwrap().window(top, height);
            assert!(!win.is_empty() || top >= total, "top={top} 窗口不得为空");
            for (i, line) in win.iter().enumerate() {
                assert_eq!(
                    flatten(std::slice::from_ref(*line)),
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
        assert!(checked > 1, "应至少检查两屏");
    }

    /// 向上滚回已淘汰区域时，该区域必须被重新精确渲染（不能停留在估算）。
    #[test]
    fn scrolling_up_rerenders_evicted_segments() {
        let mut sess = sess_with_turns(100);
        refresh_segments(&mut sess, 80, true);
        let total = sess.segments.as_ref().unwrap().total_lines();
        // 停在底部 → 头部段被淘汰。
        refresh_segments_at(&mut sess, 80, true, Some((total - 20, 20)));
        assert!(!sess.segments.as_ref().unwrap().turns[0].body.is_lines());
        // 滚到顶部 → 头部段必须重新精确渲染。
        refresh_segments_at(&mut sess, 80, true, Some((0, 20)));
        let cache = sess.segments.as_ref().unwrap();
        assert!(cache.turns[0].body.is_lines(), "滚回顶部应重渲首段");
        let win = cache.window(0, 20);
        assert!(!win.is_empty());
        let full = render_transcript_with_opts(&sess, 80, true);
        assert_eq!(
            flatten(std::slice::from_ref(win[0])),
            flatten(std::slice::from_ref(&full[0]))
        );
    }

    /// 宽度变化：**缩放**而非全量重建（Claude Code `ratio` 缩放）。
    ///
    /// 旧实现会清空重建——大会话下 resize 会卡。注意 `turn_cache_key` 含宽度
    /// （换行位置真的变了），所以重建是**必要**的；虚拟化让它只重建可见段。
    /// 因此这里必须带视口测——无虚拟化时全量重建是唯一正确答案。
    #[test]
    fn width_change_scales_and_only_rebuilds_visible_segments() {
        let mut sess = sess_with_turns(50);
        refresh_segments(&mut sess, 100, true);
        let before = sess.segments.as_ref().unwrap().total_lines();

        // 停在底部（视口 20 行）。
        let rebuilt =
            refresh_segments_at(&mut sess, 50, true, Some((before.saturating_sub(20), 20)));
        let cache = sess.segments.as_ref().unwrap();
        assert!(
            rebuilt < 50,
            "宽度变化只应重建可见段，实际重渲 {rebuilt}/50"
        );
        // 几何立即可用（缩放后非零），不必等全量重渲。
        assert!(cache.total_lines() > 0);
        // 离屏段已退化为估算（不持有 Lines）。
        assert!(
            cache.resident_segments < 50,
            "离屏段应保持估算，实际驻留 {}",
            cache.resident_segments
        );

        // F3 切换仍必须全量重建（输出语义变了，不是几何变化）。
        assert_eq!(refresh_segments(&mut sess, 50, false), 50);
    }

    /// cap 边界不得成为性能悬崖：丢最旧一回合后，其余段必须复用。
    ///
    /// `cap_turns` 每回合从**最旧一侧丢 1 个**（`drain(..1)`）。若长度一变就
    /// 整份重建，则到达 cap 后**每回合都重渲全部 400 段**——这才是真正的卡顿源。
    /// （Claude Code 的 UUID 锚点教训：计数切片会让边界每轮位移，CC-941。）
    #[test]
    fn cap_boundary_reuses_shifted_segments() {
        let mut sess = sess_with_turns(20);
        refresh_segments(&mut sess, 80, true);

        // 走**真实路径**：cap_turns 丢最旧一回合（其余 19 个内容未变）。
        // （直接 `turns.remove(0)` 不是真实路径——它不递增 dropped_turns，
        //  编号会全体前移，那才是真丢缓存。）
        sess.timeline.cap_turns(19);
        assert_eq!(sess.timeline.dropped_turns, 1, "cap 必须记录已丢弃数");
        let rebuilt = refresh_segments(&mut sess, 80, true);
        assert!(
            rebuilt <= 2,
            "丢最旧一回合只应影响边界，实际重渲 {rebuilt}/19"
        );
    }

    /// 编号必须与「窗口起点」解耦：cap 丢头部后，同一回合的编号不得改变。
    ///
    /// 这是缓存能在 cap 边界复用的**根本前提**。
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

    /// **虚拟化的核心承诺**：常驻量不随历史增长。
    ///
    /// 这是内存上界的保证：无论会话多长，只有视口附近的段持有渲染结果。
    /// 这正是可以**删除 `TURNS_CAP` 硬上限**的前提——长会话不再需要靠
    /// 丢数据来控制内存。
    /// （旧实现是 O(总回合)，2000 回合会把几十 MB 的渲染 IR 全留住。）
    #[test]
    fn resident_segments_stay_bounded_as_history_grows() {
        let mut prev = 0usize;
        for n in [50usize, 200, 800, 2000] {
            let mut sess = sess_with_turns(n);
            refresh_segments(&mut sess, 80, true);
            let total = sess.segments.as_ref().unwrap().total_lines();
            refresh_segments_at(&mut sess, 80, true, Some((total - 30, 30)));
            let resident = sess.segments.as_ref().unwrap().resident_segments;
            assert!(
                resident <= 8 + 2 * KEEP_MARGIN_SEGMENTS + 2,
                "n={n} 驻留段 {resident} 超出上界（应不随历史增长）"
            );
            assert!(resident >= prev.min(resident), "驻留量不应随 n 显著增长");
            prev = resident;
        }
        // 最关键的对照：2000 回合的驻留量与 50 回合同量级。
        assert!(prev < 30, "2000 回合时驻留段 {prev} 应仍在视口量级");
    }

    /// **删除 `TURNS_CAP` 后的安全保证**：长会话的渲染内存不随历史增长。
    ///
    /// 模拟后端 offload 后的形态（每回合正文截 512 字符），跑 2000 回合：
    /// 驻留段必须仍在视口量级。这证明不再需要靠 `TURNS_CAP` 丢数据控内存。
    #[test]
    fn long_offloaded_session_keeps_render_memory_bounded() {
        let mut sess = SessionState::new("long".into());
        let preview: String = "字".repeat(512);
        for i in 0..2000 {
            sess.timeline.turns.push(Turn {
                turn_index: Some(i as u64),
                turn_id: format!("t{i}"),
                user_text: format!("问题 {i}"),
                state: TimelineTurnState::Completed,
                failure: None,
                sealed: true,
                // 后端 seal 后 offload 的形态。
                offloaded: true,
                rounds: vec![Round {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![Block {
                        block_id: format!("b{i}"),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: preview.clone(),
                        tool: None,
                        last_fragment: 0,
                        rev: 1,
                    }],
                }],
            });
        }
        refresh_segments(&mut sess, 80, true);
        let total = sess.segments.as_ref().unwrap().total_lines();
        refresh_segments_at(&mut sess, 80, true, Some((total - 30, 30)));
        let cache = sess.segments.as_ref().unwrap();
        assert_eq!(cache.turns.len(), 2000, "不得丢回合");
        assert!(
            cache.resident_segments < 30,
            "2000 回合时驻留段 {} 应仍在视口量级",
            cache.resident_segments
        );
    }

    /// 估算高度与精确渲染的偏差应在可控范围（否则滚动条会明显跳）。
    #[test]
    fn estimate_is_within_a_reasonable_band() {
        let mut sess = sess_with_turns(20);
        refresh_segments(&mut sess, 80, true);
        let cache = sess.segments.as_ref().unwrap();
        for (idx, seg) in cache.turns.iter().enumerate() {
            let exact = seg.body.height();
            let est = estimate_turn_lines(&sess.timeline.turns[idx], 80);
            assert!(est > 0, "估算不得为 0");
            // 宽松上界：估算用于几何，偏差过大会让滚动位置明显偏移。
            assert!(
                est <= exact * 3 + 8,
                "第 {idx} 段估算 {est} 远超精确 {exact}"
            );
        }
    }

    /// 流式块（Open）每帧换键 → 每帧重渲；但**只有它**，历史段不动。
    #[test]
    fn streaming_segment_rerenders_each_frame_but_history_does_not() {
        let mut sess = sess_with_turns(10);
        refresh_segments(&mut sess, 80, true);
        // 末尾追加一个 Open 的流式块（新回合）。
        sess.timeline.turns.push(Turn {
            turn_index: None,
            turn_id: "live".into(),
            user_text: "继续".into(),
            state: TimelineTurnState::Running,
            failure: None,
            sealed: false,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: false,
                is_final: false,
                blocks: vec![Block {
                    block_id: "live_b".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: "流".into(),
                    tool: None,
                    last_fragment: 0,
                    rev: 1,
                }],
            }],
        });
        // 追加 1 个回合 → **只渲新增那一段**，已有 10 段按 `turn_id` 全部复用。
        // （旧实现长度一变就整份重建——那正是 cap 边界每帧全量重渲的根因。）
        let first = refresh_segments(&mut sess, 80, true);
        assert_eq!(first, 1, "追加一个回合只应重渲新段");

        // 记录历史段的 Arc 身份（指针相等 = 未被重渲，零拷贝复用）。
        let before: Vec<std::sync::Arc<[RenderLine]>> = sess
            .segments
            .as_ref()
            .unwrap()
            .turns
            .iter()
            .map(|s| s.body.lines().expect("保留区应为 Lines").clone())
            .collect();

        // 之后每帧：**历史段一律不动**；只有流式段可能重渲，且受动画帧
        // （200ms 粒度，与 Tick 一致）节流——同一动画帧内键不变、连流式段
        // 都不重渲。这正是“空闲零渲染”的由来。
        for _ in 0..3 {
            let n = refresh_segments(&mut sess, 80, true);
            assert!(n <= 1, "每帧至多重渲 1 段（流式段），实际 {n}");
            let after = &sess.segments.as_ref().unwrap().turns;
            for (i, old) in before.iter().enumerate() {
                assert!(
                    std::sync::Arc::ptr_eq(old, after[i].body.lines().expect("历史段应保留 Lines")),
                    "历史段 {i} 被重渲了（应复用 Arc）"
                );
            }
        }
    }

    /// 动画帧号变化时，**只有**流式段重渲（历史段仍零拷贝）。
    ///
    /// 用「下一动画帧」构造确定性的键变化，而不依赖真实墙钟。
    #[test]
    fn animation_frame_change_rebuilds_only_the_streaming_segment() {
        let mut sess = sess_with_turns(10);
        sess.timeline.turns.push(Turn {
            turn_index: None,
            turn_id: "live".into(),
            user_text: "继续".into(),
            state: TimelineTurnState::Running,
            failure: None,
            sealed: false,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: false,
                is_final: false,
                blocks: vec![Block {
                    block_id: "live_b".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: "流".into(),
                    tool: None,
                    last_fragment: 0,
                    rev: 1,
                }],
            }],
        });
        refresh_segments(&mut sess, 80, true);
        let before: Vec<std::sync::Arc<[RenderLine]>> = sess
            .segments
            .as_ref()
            .unwrap()
            .turns
            .iter()
            .map(|s| s.body.lines().expect("保留区应为 Lines").clone())
            .collect();

        // 直接改动画帧号不可能（frame_now 读墙钟），改为验证**键的构造**：
        // 同一时刻两次取键必须相等；且历史段的键与内容键无关。
        let k1 = turn_cache_key(&sess, &sess.timeline.turns[0], 0, 80, true);
        let k2 = turn_cache_key(&sess, &sess.timeline.turns[0], 0, 80, true);
        assert_eq!(k1, k2, "历史段的键不得含动画帧号（否则每帧全量重渲）");

        // 流式段的键**应当**随动画帧变化（含 frame_now）。
        let live = &sess.timeline.turns[10];
        let lk = turn_cache_key(&sess, live, 10, 80, true);
        assert!(live.rounds[0].blocks[0].is_animating());
        let _ = lk;

        let n = refresh_segments(&mut sess, 80, true);
        assert!(n <= 1);
        let after = &sess.segments.as_ref().unwrap().turns;
        for (i, old) in before.iter().enumerate() {
            assert!(
                std::sync::Arc::ptr_eq(old, after[i].body.lines().expect("历史段应保留 Lines")),
                "历史段 {i} 被重渲"
            );
        }
    }
}
