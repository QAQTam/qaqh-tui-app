//! 单会话状态：timeline 模型、流式相位、挂起交互面板、composer、滚动。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::timeline_model::TimelineModel;
use qaqh_client::ConversationMode;
// 权威类型与 `qaqh-client` 自身类型重名者带 `Domain` 前缀；在本模块内换回本地惯用名，
// 这样下文的引用点不必逐个改（映射只此一处）。
use qaqh_client::ConversationState;
use qaqh_client::{
    AskMode, ContentRef, DomainActivityState as ActivityState, DomainAskQuestion as AskQuestion,
    DomainError, PermissionCategory, PermissionRisk, SkillsStatus, UsageInfo,
};

// ───────────────────────── 流式相位 ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPhase {
    Thinking,
    ToolCalling,
    Answering,
}

impl StreamPhase {
    pub fn label(self) -> &'static str {
        match self {
            StreamPhase::Thinking => "thinking",
            StreamPhase::ToolCalling => "tool",
            StreamPhase::Answering => "answering",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamingState {
    pub turn_id: String,
    pub phase: StreamPhase,
    pub round_num: u32,
    pub tool_name: Option<String>,
    /// 武装时刻：幽灵清除的宽限判据（见 [`STREAM_GHOST_GRACE`]）。
    pub armed_at: Instant,
}

// ───────────────────────── 挂起交互面板 ─────────────────────────

/// ask_user 面板（每个问题可选预设项或自定义输入）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AskPanel {
    pub interaction_id: String,
    pub turn_id: String,
    pub mode: AskMode,
    pub questions: Vec<AskQuestion>,
    /// 每个问题选中的预设项下标。
    pub selections: Vec<Option<usize>>,
    /// 每个问题的自定义输入（提交时优先于预设项）。
    pub customs: Vec<String>,
    /// 正在编辑自定义输入的问题下标。
    pub editing_custom: Option<usize>,
    pub input: String,
    pub focus: usize,
    pub error: Option<String>,
}

impl AskPanel {
    pub fn new(
        interaction_id: String,
        turn_id: String,
        mode: AskMode,
        questions: Vec<AskQuestion>,
    ) -> Self {
        let n = questions.len();
        Self {
            interaction_id,
            turn_id,
            mode,
            questions,
            selections: vec![None; n],
            customs: vec![String::new(); n],
            editing_custom: None,
            input: String::new(),
            focus: 0,
            error: None,
        }
    }

    /// 提交前检查：每个问题都必须有答案（自定义输入优先）。
    pub fn collect_answers(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for (idx, q) in self.questions.iter().enumerate() {
            let custom = self.customs[idx].trim();
            if !custom.is_empty() {
                out.push((q.id.clone(), custom.to_owned()));
                continue;
            }
            if let Some(sel) = self.selections[idx]
                && let Some(opt) = q.options.get(sel)
            {
                out.push((q.id.clone(), opt.clone()));
                continue;
            }
            if q.options.is_empty() && q.allow_custom {
                // 仅自由文本的问题。
                return Err(format!("问题 {} 需要输入", idx + 1));
            }
            return Err(format!("问题 {} 尚未作答", idx + 1));
        }
        Ok(out)
    }
}

/// plan review 面板。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PlanPanel {
    pub interaction_id: String,
    pub turn_id: String,
    pub plan_content: String,
    pub review_type: String,
    pub todo_items: Vec<qaqh_client::TodoItem>,
    /// 拒绝理由输入。
    pub message: String,
    pub entering_message: bool,
    pub scroll: usize,
}

/// 工具权限面板。
#[derive(Debug, Clone)]
pub struct PermissionPanel {
    pub tool_call_id: String,
    pub tool_name: String,
    pub reason: String,
    pub paths: Vec<String>,
    pub category: PermissionCategory,
    pub level: u8,
    pub risk: PermissionRisk,
    pub consequence: String,
    pub trust_folder: bool,
}

// ───────────────────────── Composer ─────────────────────────

#[derive(Debug, Clone)]
pub struct Attachment {
    pub path: String,
    pub content: ContentRef,
}

/// 多行输入框（手写：char 粒度光标 + 历史；\n 为行分隔，Enter 发送、Alt+Enter/Ctrl+J 换行）。
#[derive(Debug, Clone, Default)]
pub struct Composer {
    pub input: Vec<char>,
    pub cursor: usize, // char 下标
    pub attachments: Vec<Attachment>,
    pub history: VecDeque<String>,
    pub history_idx: Option<usize>,
    pub draft_saved: Option<String>,
}

impl Composer {
    pub fn value(&self) -> String {
        self.input.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty() && self.attachments.is_empty()
    }

    pub fn insert(&mut self, ch: char) {
        let at = self.cursor.min(self.input.len());
        self.input.insert(at, ch);
        self.cursor = at + 1;
        self.history_idx = None;
    }

    /// 插入字符串；换行归一化（`\r\n`/`\r` → 单个 `\n`）并保留多行（粘贴）。
    pub fn insert_str(&mut self, s: &str) {
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    self.insert('\n');
                }
                _ => self.insert(ch),
            }
        }
    }

    /// 行数（含光标所在行的尾部空行）。
    pub fn rows(&self) -> usize {
        self.input.iter().filter(|c| **c == '\n').count() + 1
    }

    /// 光标的 (行号 0-based, 行内 char 偏移)。
    pub fn line_col(&self) -> (usize, usize) {
        let line = self.input[..self.cursor.min(self.input.len())]
            .iter()
            .filter(|c| **c == '\n')
            .count();
        let col = match self.input[..self.cursor.min(self.input.len())]
            .iter()
            .rposition(|c| *c == '\n')
        {
            Some(pos) => self.cursor - pos - 1,
            None => self.cursor,
        };
        (line, col)
    }

    /// 第 `line` 行（0-based）的 char 切片范围 `[start, end)`；越界行返回空行。
    pub fn line_bounds(&self, line: usize) -> (usize, usize) {
        let mut idx = 0usize;
        let mut cur = 0usize;
        let mut start = 0usize;
        while cur < line && idx < self.input.len() {
            if self.input[idx] == '\n' {
                cur += 1;
                start = idx + 1;
            }
            idx += 1;
        }
        if cur < line {
            return (self.input.len(), self.input.len());
        }
        let end = self.input[idx..]
            .iter()
            .position(|c| *c == '\n')
            .map(|p| idx + p)
            .unwrap_or(self.input.len());
        (start, end)
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let at = self.cursor - 1;
            if at < self.input.len() {
                self.input.remove(at);
            }
            self.cursor = at;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.input.len();
    }

    pub fn word_left(&mut self) {
        while self.cursor > 0 && self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    pub fn word_right(&mut self) {
        while self.cursor < self.input.len() && !self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.input.len() && self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> (String, Vec<Attachment>) {
        let text = self.value();
        let atts = std::mem::take(&mut self.attachments);
        self.clear();
        if !text.trim().is_empty() {
            self.history.push_back(text.clone());
            if self.history.len() > 100 {
                self.history.pop_front();
            }
        }
        self.history_idx = None;
        (text, atts)
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.draft_saved = Some(self.value());
                self.history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.history_idx = Some(idx);
        let value = self.history[idx].clone();
        self.set_value(&value);
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_idx else { return };
        if i + 1 >= self.history.len() {
            self.history_idx = None;
            let draft = self.draft_saved.take().unwrap_or_default();
            self.set_value(&draft);
        } else {
            self.history_idx = Some(i + 1);
            let value = self.history[i + 1].clone();
            self.set_value(&value);
        }
    }

    fn set_value(&mut self, s: &str) {
        self.input = s.chars().collect();
        self.cursor = self.input.len();
    }
}

// ───────────────────────── 滚动 ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ScrollState {
    /// 跟随底部（新内容自动下滚）。
    pub follow: bool,
    /// 非跟随模式下，距底部的行数。
    pub offset: usize,
}

// ───────────────────────── 渲染缓存 ─────────────────────────

/// 视口外仍保留精确渲染结果的段数余量。
///
/// 权衡：够顺滑滚动（快速上翻不用重渲），又不至于把整段历史留在内存。
/// Grok 用 128 是因为它的 entry 粒度是**块**（400+ 个）；本仓粒度是**回合**，
/// 单个回合可达上千行，所以取小值。
pub(crate) const KEEP_MARGIN_SEGMENTS: usize = 8;

/// 一段的渲染体：**要么**是精确的行，**要么**只是估算高度。
///
/// 这是虚拟化的关键：离屏段只留一个 `usize`，丢弃 `Arc<[RenderLine]>`。
/// 实测真实会话 110 回合共 124690 行——全部驻留渲染结果是几十 MB，
/// 而它们大多永远滚不到（Grok `evict_offscreen_render_caches` 同一动机）。
#[derive(Debug, Clone)]
pub enum SegmentBody {
    /// 精确渲染结果（仅在视口附近保留）。
    Lines(Arc<[crate::app::render_line::RenderLine]>),
    /// 仅估算高度（离屏段；向上滚到时按需重渲）。
    Height(usize),
}

impl SegmentBody {
    pub fn height(&self) -> usize {
        match self {
            SegmentBody::Lines(l) => l.len(),
            SegmentBody::Height(n) => *n,
        }
    }

    pub fn lines(&self) -> Option<&Arc<[crate::app::render_line::RenderLine]>> {
        match self {
            SegmentBody::Lines(l) => Some(l),
            SegmentBody::Height(_) => None,
        }
    }

    pub(crate) fn is_lines(&self) -> bool {
        matches!(self, SegmentBody::Lines(_))
    }
}

/// 一个缓存段：内容键 + 渲染体。
///
/// `key` **不含宽度**：宽度变化时走「高度缩放 + 丢弃 Lines」而非全量重建
/// （Claude Code `useVirtualScroll.ts` 的 `ratio` 缩放，注释记着实测：清空缓存
/// 会让 resize 时 mount 范围膨胀到 190 项、每次 3ms 的 lexer+高亮 → 600ms 卡顿）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub key: u64,
    pub body: SegmentBody,
    /// 本段对应的回合 id（用于长度变化时按 id 对齐复用）。
    ///
    /// 不能只靠下标：`cap_turns` 从头部丢回合后，第 i 个位置换成了另一个回合。
    /// 也不能只靠 `key`——key 含 `turn_idx`（头部编号会漂移），丢一个就会全变。
    pub turn_id: String,
}

/// **分段**渲染缓存（A3 + 虚拟化）。
///
/// 历史问题：单块缓存以 `timeline.version` 为键，而 `version` 每次 delta 都
/// `bump()` → 每帧全量重渲（含 markdown/syntect 与 reasoning normalize），
/// 成本随历史线性增长。这里改为**按回合分段**：每段自带内容键，只有键变了的
/// 段才重渲，其余段直接复用 `Arc`（零拷贝）。
///
/// 进一步的虚拟化：**只有视口附近的段才持有渲染结果**，离屏段退化为估算高度。
#[derive(Debug, Clone, Default)]
pub struct SegmentCache {
    /// 渲染宽度（内容宽，含滚动条扣除）。与 `transcript::draw` 必须一致。
    pub width: u16,
    /// `show_reasoning`（F3）：影响 reasoning 块输出，必须进缓存身份。
    pub show_reasoning: bool,
    /// 每回合一段，与 `timeline.turns` 同序。
    pub turns: Vec<Segment>,
    /// 头部横幅段（更早回合折叠 / 够不到提示）。
    pub banner: Option<Segment>,
    /// 空会话占位段（无回合时的提示行）。
    pub empty: Option<Segment>,
    /// 最近一次重建中实际重渲的段数（观测用：验证增量是否真的发生）。
    pub rebuilt_segments: usize,
    /// 当前仍持有渲染结果的段数（观测用：验证淘汰真的发生了）。
    pub resident_segments: usize,
}

impl SegmentCache {
    /// 总行数（视窗定位/滚动条需要）。估算高度也计入——这就是虚拟化能
    /// 在**不渲染**的前提下算出滚动几何的原因。
    pub fn total_lines(&self) -> usize {
        self.all().map(|s| s.body.height()).sum()
    }

    /// 按顺序枚举所有段。
    pub(crate) fn all(&self) -> impl Iterator<Item = &Segment> {
        self.banner
            .iter()
            .chain(self.turns.iter())
            .chain(self.empty.iter())
    }

    /// 取可见窗口 `[top, top+height)` 的行切片。
    ///
    /// 返回的每项是 `&RenderLine`，指向段内的 `Arc`，无拷贝。
    ///
    /// **不变式**：`refresh_segments` 保证窗口覆盖的段均为 [`SegmentBody::Lines`]
    /// （它按同一份高度几何渲染了窗口 + 余量）。若此处遇到估算段，说明
    /// 不变式被破坏——调试构建下会断言失败。
    pub fn window(&self, top: usize, height: usize) -> Vec<&crate::app::render_line::RenderLine> {
        let mut out: Vec<&crate::app::render_line::RenderLine> = Vec::with_capacity(height);
        let mut skip = top;
        for seg in self.all() {
            if out.len() >= height {
                break;
            }
            // 无论是否已渲染，段都**占据几何空间**，必须先扣减 skip。
            // （曾在此处漏减，导致窗口定位整体偏移。）
            let h = seg.body.height();
            let Some(lines) = seg.body.lines() else {
                if skip >= h {
                    skip -= h;
                    continue;
                }
                // 视口落在未渲染段上：`refresh_segments_at` 的收尾保证不应让
                // 这种情况发生。debug 下报错；release 下保守少渲几行，不崩。
                debug_assert!(false, "窗口内出现未渲染的估算段（虚拟化不变式被破坏）");
                skip = 0;
                continue;
            };
            if skip >= lines.len() {
                skip -= lines.len();
                continue;
            }
            let start = skip;
            skip = 0;
            let take = (height - out.len()).min(lines.len() - start);
            out.extend(lines[start..start + take].iter());
        }
        out
    }

    /// 覆盖行区间 `[top, top+height)` 的段下标范围（**仅含 turns**）。
    ///
    /// 供 `refresh_segments` 决定「哪些段要精确渲染」与「哪些可淘汰」。
    /// 空段（高度 0）不占据区间。
    pub(crate) fn segment_range_for(&self, top: usize, height: usize) -> (usize, usize) {
        let banner_h = self.banner.as_ref().map_or(0, |s| s.body.height());
        let bottom = top.saturating_add(height);
        let mut acc = banner_h;
        let mut first: Option<usize> = None;
        let mut last: usize = 0;
        for (i, seg) in self.turns.iter().enumerate() {
            let h = seg.body.height();
            if first.is_none() && acc + h > top {
                first = Some(i);
            }
            if acc < bottom {
                last = i;
            }
            acc += h;
        }
        match first {
            // 视口完全在内容之下（空会话）或 turns 为空：退化为空范围。
            None => (self.turns.len(), self.turns.len().saturating_sub(1)),
            Some(f) => (f, last.max(f)),
        }
    }
}

// ───────────────────────── 会话状态 ─────────────────────────

/// 压缩过程动画状态（由 Conversation 事件驱动；结束/重基线时清除）。
#[derive(Debug, Clone)]
pub struct CompactionAnim {
    pub started_at: std::time::Instant,
    pub turns_total: u32,
    pub turns_keeping: u32,
    /// CompactProgress 的 delta 文本（协议真实信息，随条展示）。
    pub last_delta: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seed: String,
    pub title: Option<String>,
    pub mode: ConversationMode,
    pub timeline: TimelineModel,
    /// bootstrap 的 conversation 快照视图（usage/model/context）。
    /// conversation 频道快照的**类型化**视图（权威类型，本仓不再手解）。
    ///
    /// 仅作 `model` / `context_limit` / `usage` 的缓存——快照里其余字段由
    /// `TimelineModel` 与实时事件承担。
    pub conversation: Option<ConversationState>,
    pub activity: Option<ActivityState>,
    pub usage: Option<UsageInfo>,
    pub usage_totals: Option<UsageInfo>,
    pub context_limit: Option<u32>,
    pub streaming: Option<StreamingState>,
    pub pending_ask: Option<AskPanel>,
    pub pending_plan: Option<PlanPanel>,
    pub pending_permissions: Vec<PermissionPanel>,
    pub skills: Option<SkillsStatus>,
    /// workspace 面板数据（bootstrap control state + DashboardSnapshot 推送）。
    pub dashboard: Option<qaqh_client::DomainDashboardSnapshot>,
    /// 压缩进度动画（Some = 压缩进行中）。
    pub compact_anim: Option<CompactionAnim>,
    /// 代码变更聚合（+行 / −行）。
    pub code_added: usize,
    pub code_removed: usize,
    pub last_error: Option<DomainError>,
    pub composer: Composer,
    pub scroll: ScrollState,
    /// 分段渲染缓存（A3，transcript 的主渲染路径）。
    pub segments: Option<SegmentCache>,
    /// bootstrap / re-baseline 是否已就绪。
    pub ready: bool,
    /// 被 LRU 逐出 transcript 后，重新聚焦时需要 re-baseline。
    pub needs_rebaseline: bool,
    /// 加载更早：in-flight 去重。
    pub loading_older: bool,
    /// 已展开的工具输出（tool_call_id 集合，折叠态默认收起超长输出）
    pub expanded_tools: std::collections::HashSet<String>,
    /// 本会话拉起的子代理（spawn 顺序；身份锚点 = timeline 工具卡 id）。
    pub subagents: Vec<super::subagent::SubagentEntry>,
}

impl SessionState {
    pub fn new(seed: String) -> Self {
        Self {
            seed,
            title: None,
            mode: ConversationMode::Code,
            timeline: TimelineModel::default(),
            conversation: None,
            activity: None,
            usage: None,
            usage_totals: None,
            context_limit: None,
            streaming: None,
            pending_ask: None,
            pending_plan: None,
            pending_permissions: Vec::new(),
            skills: None,
            dashboard: None,
            compact_anim: None,
            code_added: 0,
            code_removed: 0,
            last_error: None,
            composer: Composer::default(),
            scroll: ScrollState {
                follow: true,
                offset: 0,
            },
            segments: None,
            ready: false,
            needs_rebaseline: false,
            loading_older: false,
            expanded_tools: std::collections::HashSet::new(),
            subagents: Vec::new(),
        }
    }

    /// 标签标题：控制频道 `SessionMetaChanged` 推来的 title → `session <seed>`。
    ///
    /// 这里曾有第三级回退 `meta.display_title()`——**那段代码从未执行过**：
    /// `SessionState::meta` 自首版起就没被赋过值（`git log -S'meta = Some'` 全史
    /// 零命中），恒为 `None`。G2 类型化时一并删除，语义不变（它本来就走不到）。
    pub fn title(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_owned();
        }
        format!("session {}", self.seed)
    }

    pub fn display_model(&self) -> Option<String> {
        self.conversation.as_ref().and_then(|c| c.model.clone())
    }

    /// 优先级：permission > ask > plan（winui 语义）。
    pub fn active_permission(&self) -> Option<&PermissionPanel> {
        self.pending_permissions.first()
    }

    pub fn is_waiting_user(&self) -> bool {
        self.activity == Some(ActivityState::WaitingUser)
            || !self.pending_permissions.is_empty()
            || self.pending_ask.is_some()
            || self.pending_plan.is_some()
    }

    /// 状态栏标签（working / waiting / idle…）。
    pub fn activity_label(&self) -> String {
        if !self.pending_permissions.is_empty() {
            return "permission".into();
        }
        if self.pending_ask.is_some() {
            return "ask".into();
        }
        if self.pending_plan.is_some() {
            return "plan review".into();
        }
        if let Some(s) = &self.streaming {
            return format!("{} · r{}", s.phase.label(), s.round_num);
        }
        match self.activity {
            Some(ActivityState::Starting) => "starting".into(),
            Some(ActivityState::Working) => "working".into(),
            Some(ActivityState::WaitingUser) => "waiting_user".into(),
            Some(ActivityState::Disconnected) => "disconnected".into(),
            _ => "idle".into(),
        }
    }

    pub fn apply_usage(&mut self, usage: UsageInfo, context_limit: u32, model: String) {
        self.usage = Some(usage);
        self.context_limit = Some(context_limit);
        if let Some(conv) = self.conversation.as_mut() {
            conv.model = Some(model);
            conv.context_limit = Some(context_limit as u64);
        }
    }
}

/// 会话退出流式的统一收口。
pub fn streaming_done(session: &mut SessionState, turn_id: Option<&str>) {
    let matches = match (&session.streaming, turn_id) {
        (Some(s), Some(t)) => s.turn_id == t,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if matches {
        session.streaming = None;
    }
}

/// 幽灵流式状态的宽限期。
///
/// daemon 侧 timeline `TurnOpened` 先于 conversation `TurnStarted` 发布
///（`agent/engine_input.rs`），但两条 SSE 通道独立投递仍可能反转；宽限期确保
/// "timeline 暂时还没出现这个 turn" 不被误判成 "turn 已终结"。
pub const STREAM_GHOST_GRACE: Duration = Duration::from_secs(2);

/// timeline（transcript 与 turn 生命周期权威）→ streaming 状态收敛。
///
/// 收敛矩阵（本函数是 "working 卡死" 的唯一自愈点）：
/// 1. 窗口内有 running turn：无状态则武装；状态指向别的 turn 则改指（漏 TurnStarted 兜底）。
/// 2. streaming 指向的 turn 在窗口内且已非 Running → 清除。终态条目（TurnSealed）
///    与对话终态事件走两条独立通道，任一方乱序/丢失都不允许把 UI 永久钉在 working。
/// 3. streaming 指向的 turn 已滑出尾部窗口且窗口非空 → 宽限后清除幽灵。
/// 4. 窗口为空（尚未装载 / 全新会话）→ 不动：信息不足，等事件或下一次 rebaseline。
pub fn sync_streaming_from_timeline(session: &mut SessionState) {
    sync_streaming_from_timeline_at(session, Instant::now());
}

/// [`sync_streaming_from_timeline`] 的可注入时钟版本（单测用）。
pub fn sync_streaming_from_timeline_at(session: &mut SessionState, now: Instant) {
    let running = session
        .timeline
        .running_turn_id()
        .map(|turn_id| turn_id.to_owned());
    match (
        session.streaming.as_ref().map(|s| s.turn_id.clone()),
        running,
    ) {
        (None, Some(turn_id)) => {
            session.streaming = Some(StreamingState {
                turn_id,
                phase: StreamPhase::Answering,
                round_num: 0,
                tool_name: None,
                armed_at: now,
            });
        }
        (Some(current), Some(turn_id)) if current != turn_id => {
            // 新 turn 已在窗口 running 而本地还指着旧 turn：会话事件丢失后的改指。
            if let Some(state) = session.streaming.as_mut() {
                state.turn_id = turn_id;
                state.armed_at = now;
            }
        }
        (Some(current), None) => match session.timeline.turn_running(&current) {
            // 权威终态：该 turn 已在窗口内 sealed/failed/cancelled。
            Some(false) => {
                session.streaming = None;
                clear_busy_activity(session);
            }
            // 不在窗口：尾部窗口已淘汰它（窗口非空即有更新的 turn）→ 宽限后清幽灵。
            None if !session.timeline.turns.is_empty()
                && session
                    .streaming
                    .as_ref()
                    .is_some_and(|s| now.duration_since(s.armed_at) >= STREAM_GHOST_GRACE) =>
            {
                session.streaming = None;
                clear_busy_activity(session);
            }
            _ => {}
        },
        _ => {}
    }
}

/// timeline 已证伪 busy：把 control 域残留的 Working/Starting 降级为 Idle。
///
/// 只在 timeline 给出"无 running turn"证据时调用——daemon 真的卡在流式中时
/// timeline 仍是 Running，此处不会说谎（两类 "working" 因此可区分）。
fn clear_busy_activity(session: &mut SessionState) {
    if matches!(
        session.activity,
        Some(ActivityState::Working | ActivityState::Starting)
    ) {
        session.activity = Some(ActivityState::Idle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_line_col_track_newlines() {
        let mut c = Composer::default();
        assert_eq!(c.rows(), 1);
        assert_eq!(c.line_col(), (0, 0));
        for ch in "ab".chars() {
            c.insert(ch);
        }
        c.insert('\n');
        for ch in "cd".chars() {
            c.insert(ch);
        }
        assert_eq!(c.value(), "ab\ncd");
        assert_eq!(c.rows(), 2);
        assert_eq!(c.line_col(), (1, 2));
        c.left();
        c.left();
        assert_eq!(c.line_col(), (1, 0));
    }

    #[test]
    fn line_bounds_split_multiline() {
        let mut c = Composer::default();
        for ch in "ab\ncd\n\n".chars() {
            c.insert(ch);
        }
        let (s0, e0) = c.line_bounds(0);
        assert_eq!(c.input[s0..e0], ['a', 'b']);
        let (s1, e1) = c.line_bounds(1);
        assert_eq!(c.input[s1..e1], ['c', 'd']);
        let (s2, e2) = c.line_bounds(2);
        assert_eq!(c.input[s2..e2], Vec::<char>::new());
        // 越界行：空窗口。
        let (s3, e3) = c.line_bounds(9);
        assert_eq!(s3, e3);
        assert_eq!(c.line_col(), (3, 0));
    }

    #[test]
    fn insert_str_keeps_paste_newlines() {
        let mut c = Composer::default();
        c.insert_str("第一行\n第二行\r\n第三行");
        assert_eq!(c.rows(), 3);
        assert!(c.value().starts_with("第一行\n第二行\n第三行"));
    }

    // ───────────── streaming ↔ timeline 收敛（"working 卡死"回归） ─────────────

    use crate::app::timeline_model::Turn;
    use qaqh_client::TimelineTurnState;

    fn turn(id: &str, state: TimelineTurnState) -> Turn {
        Turn {
            turn_index: None,
            turn_id: id.into(),
            user_text: "hi".into(),
            state,
            failure: None,
            // 夹具约定：封口必与终态同时出现（后端 `seal_turn_with_state`
            // 也是 `sealed = true` 与 `state = <终态>` 一同写入）。
            sealed: state != TimelineTurnState::Running,
            offloaded: false,
            rounds: Vec::new(),
        }
    }

    fn session_with(turns: Vec<Turn>, streaming: Option<(&str, Instant)>) -> SessionState {
        let mut session = SessionState::new("seed".into());
        session.timeline.turns = turns;
        session.streaming = streaming.map(|(turn_id, armed_at)| StreamingState {
            turn_id: turn_id.into(),
            phase: StreamPhase::Answering,
            round_num: 0,
            tool_name: None,
            armed_at,
        });
        session
    }

    #[test]
    fn sealed_turn_clears_streaming_and_downgrades_busy_activity() {
        let now = Instant::now();
        let mut s = session_with(
            vec![turn("t1", TimelineTurnState::Completed)],
            Some(("t1", now - STREAM_GHOST_GRACE * 2)),
        );
        s.activity = Some(ActivityState::Working);
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(s.streaming.is_none(), "sealed turn must clear streaming");
        assert_eq!(
            s.activity,
            Some(ActivityState::Idle),
            "busy activity must be downgraded together with streaming"
        );
    }

    #[test]
    fn sealed_turn_converges_after_late_timeline_rearm() {
        // 旧 bug 的关键序列：TurnCompleted 已收口（streaming=None），但该 turn
        // 的终态条目尚未到达（模型仍 Running）→ 旧实现 (None, true) 会重新武装，
        // 且因 (Some, false) 分支不清理而永久停在 working。
        let now = Instant::now();
        let mut s = session_with(vec![turn("t1", TimelineTurnState::Running)], None);
        s.activity = Some(ActivityState::Working);
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(s.streaming.is_some(), "running turn still arms streaming");

        // TurnSealed 到达：权威终态必须把 UI 收敛回 idle。
        s.timeline.turns = vec![turn("t1", TimelineTurnState::Completed)];
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(
            s.streaming.is_none(),
            "sealed turn must converge back to idle"
        );
        assert_eq!(s.activity, Some(ActivityState::Idle));
    }

    #[test]
    fn ghost_turn_evicted_from_window_clears_after_grace() {
        let now = Instant::now();
        let mut s = session_with(
            vec![turn("t2", TimelineTurnState::Completed)],
            Some(("t1", now - STREAM_GHOST_GRACE - Duration::from_millis(1))),
        );
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(
            s.streaming.is_none(),
            "turn evicted from the tail window must not keep the UI busy"
        );
    }

    #[test]
    fn fresh_arm_survives_timeline_lag() {
        // TurnStarted 早于 timeline TurnOpened 到达（跨通道投递反转）：
        // 宽限期内不得被误判为幽灵。
        let now = Instant::now();
        let mut s = session_with(
            vec![turn("t9", TimelineTurnState::Completed)],
            Some(("t10", now)),
        );
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(
            s.streaming.is_some(),
            "freshly armed streaming must survive window lag"
        );
    }

    #[test]
    fn empty_window_is_not_evidence() {
        let now = Instant::now();
        let mut s = session_with(Vec::new(), Some(("t1", now - STREAM_GHOST_GRACE * 10)));
        sync_streaming_from_timeline_at(&mut s, now);
        assert!(
            s.streaming.is_some(),
            "an empty timeline proves nothing about the turn"
        );
    }

    #[test]
    fn newer_running_turn_repoints_stale_streaming() {
        let now = Instant::now();
        let mut s = session_with(
            vec![turn("t2", TimelineTurnState::Running)],
            Some(("t1", now - STREAM_GHOST_GRACE * 2)),
        );
        sync_streaming_from_timeline_at(&mut s, now);
        assert_eq!(s.streaming.as_ref().map(|s| s.turn_id.as_str()), Some("t2"));
    }
}
