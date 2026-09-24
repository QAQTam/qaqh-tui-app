//! 单会话状态：timeline 模型、流式相位、挂起交互面板、composer、滚动。

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::app::timeline_model::TimelineModel;
use qaqh_client::ConversationMode;
// 权威类型与 `qaqh-client` 自身类型重名者带 `Domain` 前缀；在本模块内换回本地惯用名，
// 这样下文的引用点不必逐个改（映射只此一处）。
use qaqh_client::ConversationState;
use qaqh_client::{
    AskMode, ContentRef, DomainActivityState as ActivityState, DomainAskQuestion as AskQuestion,
    DomainError, PermissionCategory, PermissionRisk, UsageInfo,
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
    /// 当前问题页。左右键切题；与 `option_cursor[focus]` 分离。
    pub focus: usize,
    /// 每个问题的选项光标。自定义输入行位于 `options.len()`。
    pub option_cursor: Vec<usize>,
    /// 当前问题页的滚动行，供长问题文本使用。
    pub scroll: u16,
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
            option_cursor: vec![0; n],
            scroll: 0,
            error: None,
        }
    }

    pub fn option_count(&self, question_idx: usize) -> usize {
        self.questions
            .get(question_idx)
            .map(|q| q.options.len() + usize::from(q.allow_custom))
            .unwrap_or(0)
    }

    pub fn option_cursor(&self, question_idx: usize) -> usize {
        self.option_cursor
            .get(question_idx)
            .copied()
            .unwrap_or(0)
            .min(self.option_count(question_idx).saturating_sub(1))
    }

    pub fn move_option_cursor(&mut self, question_idx: usize, delta: i32) {
        let count = self.option_count(question_idx);
        if count == 0 {
            return;
        }
        let current = self.option_cursor(question_idx) as i32;
        let next = (current + delta).clamp(0, count.saturating_sub(1) as i32) as usize;
        if let Some(cursor) = self.option_cursor.get_mut(question_idx) {
            *cursor = next;
        }
    }

    pub fn select_option(&mut self, question_idx: usize, option_idx: usize) {
        let Some(question) = self.questions.get(question_idx) else {
            return;
        };
        if option_idx >= question.options.len() {
            return;
        }
        if let Some(selection) = self.selections.get_mut(question_idx) {
            *selection = Some(option_idx);
        }
        if let Some(custom) = self.customs.get_mut(question_idx) {
            custom.clear();
        }
        if let Some(cursor) = self.option_cursor.get_mut(question_idx) {
            *cursor = option_idx;
        }
        self.error = None;
    }

    pub fn toggle_option(&mut self, question_idx: usize, option_idx: usize) {
        let selected = self
            .selections
            .get(question_idx)
            .is_some_and(|value| *value == Some(option_idx));
        if selected {
            if let Some(selection) = self.selections.get_mut(question_idx) {
                *selection = None;
            }
        } else {
            self.select_option(question_idx, option_idx);
        }
    }

    pub fn is_on_custom_row(&self) -> bool {
        let Some(question) = self.questions.get(self.focus) else {
            return false;
        };
        question.allow_custom && self.option_cursor(self.focus) == question.options.len()
    }

    pub fn first_unanswered(&self) -> Option<usize> {
        (0..self.questions.len()).find(|&idx| {
            let custom = self.customs.get(idx).is_some_and(|v| !v.trim().is_empty());
            !custom && self.selections.get(idx).and_then(|value| *value).is_none()
        })
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

/// 选项快捷键：1..9 对应前 9 项，a..f 对应第 10..15 项。
pub fn option_shortcut_label(index: usize) -> Option<char> {
    match index {
        0..=8 => Some((b'1' + index as u8) as char),
        9..=14 => Some((b'a' + (index - 9) as u8) as char),
        _ => None,
    }
}

/// 把用户按下的字符映射为 0-based 选项下标。
pub fn option_index_for_key(ch: char) -> Option<usize> {
    match ch {
        '1'..='9' => Some(ch as usize - '1' as usize),
        'a'..='f' => Some(9 + ch as usize - 'a' as usize),
        _ => None,
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
    /// P3 交底（后端 `docs/spec/2026-09-23-TUI-typed-todo消费路径-spec.md` §2）：
    /// 旧 `qaqh_client::TodoItem` 已改名为 `PlanReviewItem`，**纯改名、字段一字未动**
    /// （id / title / description / complexity），wire 形状无变化。
    /// 注意：它只服务 plan review 预览，与 workspace todo 面板用的 `DashboardTask`
    /// 不是同一个语义面。
    pub todo_items: Vec<qaqh_client::PlanReviewItem>,
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
    /// 有界动作摘要（目前 daemon 对 exec 下发 command/argv/args/shell/cwd）。
    pub action_summary: Option<String>,
    pub reason: String,
    pub paths: Vec<String>,
    pub category: PermissionCategory,
    pub level: u8,
    pub risk: PermissionRisk,
    pub consequence: String,
    pub trust_folder: bool,
}

/// 「已经解决掉」的权限请求 id 历史（有界 FIFO）。
///
/// 为什么需要它：daemon 的三个频道各自投递，`ToolPermissionRequested` 完全可能
/// 在 `ToolStarted` **之后**才到（补投/乱序）。旧实现只按 tool_call_id 从
/// `pending_permissions` 里 `retain` 再 `push`，于是补投会把已经应答过的请求
/// 重新塞回去——用户看到一个「幽灵面板」，而 `active_permission()` 取
/// `pending_permissions.first()`，它还会把真正该处理的 ask 挤到后面。
///
/// 有界的原因：需要记住的只是「同一个 tool_call 的事件乱序窗口」，量级是个位数；
/// [`RespondedPermissions::CAP`] 给到 256 足够覆盖任何现实的补投，同时保证集合
/// 不随会话长度无界增长。超出容量时淘汰最旧的 id（那时它的补投窗口早已关闭）。
/// 会话关闭时整个 `SessionState` 被移除，历史随之丢弃，不跨会话泄漏。
#[derive(Debug, Clone, Default)]
pub struct RespondedPermissions {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl RespondedPermissions {
    /// 容量上限（见类型注释）。
    pub const CAP: usize = 256;

    pub fn insert(&mut self, tool_call_id: &str) {
        if !self.seen.insert(tool_call_id.to_owned()) {
            return;
        }
        self.order.push_back(tool_call_id.to_owned());
        while self.order.len() > Self::CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
    }

    pub fn contains(&self, tool_call_id: &str) -> bool {
        self.seen.contains(tool_call_id)
    }
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
    /// 已解决的权限请求 id（防「幽灵面板」补投，见 [`RespondedPermissions`]）。
    pub responded_permissions: RespondedPermissions,
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
    /// 块级渲染缓存（M1 `render::refresh`，T8 起为生产唯一来源；
    /// 锁 8 动画出带的宿主）。
    pub block_cache: Option<crate::app::render::TranscriptCache>,
    /// bootstrap / re-baseline 是否已就绪。
    pub ready: bool,
    /// 被 LRU 逐出 transcript 后，重新聚焦时需要 re-baseline。
    pub needs_rebaseline: bool,
    /// 加载更早：in-flight 去重。
    pub loading_older: bool,
    /// §4.2 运行组展开态：(turn_id, round_num)。展开 = 组内卡片列表可见；
    /// 收起 = 一行组行（失败例外：组内最后一张 Failed 卡始终内联）。
    pub expanded_groups: std::collections::HashSet<(String, u32)>,
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
            responded_permissions: RespondedPermissions::default(),
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
            block_cache: None,
            ready: false,
            needs_rebaseline: false,
            loading_older: false,
            expanded_groups: std::collections::HashSet::new(),
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

    /// **实时**权限请求的入队口（`ToolPermissionRequested`）：已解决过的
    /// tool_call_id 不再入队，同一 id 重复到达时以最新一条替换（详情靠后续
    /// tool 事件补全）。
    ///
    /// bootstrap 快照恢复**不走这里**——快照是旧视图，走
    /// [`SessionState::restore_permission_from_snapshot`]（只补不换，免得用
    /// 「（恢复中）」占位符覆盖实时事件带来的详情）。两条路径共用「已解决的 id
    /// 不再入队」这一条判据（都查 [`RespondedPermissions`]）。
    ///
    /// 返回是否入队（`false` = 已解决，被拒）。生产代码不需要这个值，保留是为了
    /// 让测试能直接断言「补投被丢弃」。
    pub fn queue_permission(&mut self, panel: PermissionPanel) -> bool {
        if self.responded_permissions.contains(&panel.tool_call_id) {
            return false;
        }
        self.pending_permissions
            .retain(|p| p.tool_call_id != panel.tool_call_id);
        self.pending_permissions.push(panel);
        true
    }

    /// 权限已解决（用户应答 / 工具已开始 / 已结束）：面板下架并记入历史。
    pub fn resolve_permission(&mut self, tool_call_id: &str) {
        self.pending_permissions
            .retain(|p| p.tool_call_id != tool_call_id);
        self.responded_permissions.insert(tool_call_id);
    }

    /// bootstrap 快照恢复挂起权限：**只补不换**。
    ///
    /// 与实时事件入口（[`SessionState::queue_permission`]）的区别在「同 id 已存在」
    /// 时怎么办：快照可能是**旧**视图（`tool_state` 的 pending 字段只带 id），
    /// 用它覆盖实时事件带来的面板会把工具名/理由/风险等级换成「（恢复中）」占位符，
    /// 还会把该面板挪到队尾、改变 `active_permission()` 的优先级。所以这里只在
    /// 「内存里没有这个 id」时补一条；已解决的 id 同样不再入队。
    ///
    /// 返回是否真的补了面板。
    pub fn restore_permission_from_snapshot(&mut self, panel: PermissionPanel) -> bool {
        if self.responded_permissions.contains(&panel.tool_call_id) {
            return false;
        }
        if self
            .pending_permissions
            .iter()
            .any(|p| p.tool_call_id == panel.tool_call_id)
        {
            return false;
        }
        self.pending_permissions.push(panel);
        true
    }

    /// 从 timeline 工具卡构造 permission 面板（v2 交互正文不含 permission 详情）。
    /// 找不到卡片时退化为「（恢复中）」占位——详情等 tool 事件补全。
    pub fn permission_panel_for(&self, call_id: &str) -> PermissionPanel {
        let card = self.timeline.tool_card(call_id);
        let tool_name = card
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "（恢复中）".into());
        let perm = card.and_then(|c| c.permission.clone());
        let (reason, paths, category, level, risk, consequence) = match perm {
            Some(p) => (p.reason, p.paths, p.category, p.level, p.risk, p.consequence),
            None => (
                String::new(),
                Vec::new(),
                String::new(),
                0u8,
                String::new(),
                String::new(),
            ),
        };
        PermissionPanel {
            tool_call_id: call_id.to_string(),
            tool_name,
            action_summary: None,
            reason,
            paths,
            category: permission_category_from_tag(&category),
            level,
            risk: permission_risk_from_tag(&risk),
            consequence,
            trust_folder: false,
        }
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

/// v2 control 投影的 activity 词汇（idle / running / interrupted）→ 领域
/// `ActivityState`（本仓状态栏仍用领域词汇）。
///
/// `waiting_user` 不由 activity 表达，而由挂起交互面板（permission / ask /
/// plan）判定，见 [`SessionState::is_waiting_user`]。
pub fn activity_from_v2(activity: qaqh_client::ClientV2ActivityState) -> ActivityState {
    match activity {
        qaqh_client::ClientV2ActivityState::Idle => ActivityState::Idle,
        qaqh_client::ClientV2ActivityState::Running => ActivityState::Working,
        qaqh_client::ClientV2ActivityState::Interrupted => ActivityState::Disconnected,
    }
}

/// 从 v2 conversation 投影抽出本仓 `conversation` 缓存（只保留 model/usage）。
///
/// v2 投影没有聚合的 `usage_totals` / `context_limit`：model/usage 取**最新**
/// assistant block 的字段，其余留给实时 `UsageUpdated` 事件补全。
pub fn conversation_cache_from_v2(
    snapshot: &qaqh_client::ClientV2ConversationState,
) -> ConversationState {
    let mut cache = ConversationState::default();
    for entry in snapshot.context.iter().rev() {
        if let qaqh_client::ClientV2ConversationContextKind::AssistantBlock(block) = &entry.kind {
            cache.model = Some(block.model.clone());
            cache.usage = block.usage.clone();
            break;
        }
    }
    cache
}

/// timeline 工具卡的 `category` 字符串（snake_case）→ 面板枚举。
pub fn permission_category_from_tag(tag: &str) -> PermissionCategory {
    serde_json::from_value(serde_json::Value::String(tag.to_string()))
        .unwrap_or(PermissionCategory::Read)
}

/// timeline 工具卡的 `risk` 字符串（snake_case）→ 面板枚举。
pub fn permission_risk_from_tag(tag: &str) -> PermissionRisk {
    serde_json::from_value(serde_json::Value::String(tag.to_string()))
        .unwrap_or(PermissionRisk::Medium)
}

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
            thinking: Default::default(),
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

    // ───────────── 权限面板：已响应后不得被补投复活（幽灵面板回归） ─────────────

    fn perm(tool_call_id: &str) -> PermissionPanel {
        PermissionPanel {
            tool_call_id: tool_call_id.into(),
            tool_name: "bash".into(),
            action_summary: None,
            reason: String::new(),
            paths: Vec::new(),
            category: PermissionCategory::Read,
            level: 0,
            risk: PermissionRisk::Medium,
            consequence: String::new(),
            trust_folder: false,
        }
    }

    /// 回归：同一 tool_call_id 在已响应后不得重新入队。
    ///
    /// 证伪方式：把 `queue_permission` 里的 `responded_permissions.contains` 判断
    /// 去掉（旧行为：只 retain + push）——`assert!(!...queue_permission(...))` 立刻
    /// 变红，`active_permission()` 也会重新拿到那个幽灵面板。
    #[test]
    fn responded_permission_is_not_requeued() {
        let mut s = SessionState::new("seed".into());
        assert!(s.queue_permission(perm("c1")));
        s.resolve_permission("c1"); // 用户按 a/d 应答
        assert!(s.active_permission().is_none(), "应答后面板必须下架");

        assert!(
            !s.queue_permission(perm("c1")),
            "已响应的 tool_call_id 不得重新入队"
        );
        assert!(s.active_permission().is_none(), "幽灵面板不许出现");
        assert_ne!(s.activity_label(), "permission");

        // 别的 tool_call 不受影响（集合是按 id 判定的，不是一刀切丢弃）。
        assert!(s.queue_permission(perm("c2")));
        assert_eq!(
            s.active_permission().map(|p| p.tool_call_id.as_str()),
            Some("c2")
        );
    }

    /// 回归：真实事故序列——权限请求 → 工具已开始（面板下架）→ 权限请求补投到。
    #[test]
    fn late_permission_after_tool_started_does_not_resurrect_panel() {
        let mut s = SessionState::new("seed".into());
        assert!(s.queue_permission(perm("c1")));
        // ToolStarted / ToolFinished 走的就是 resolve_permission。
        s.resolve_permission("c1");
        assert!(
            !s.queue_permission(perm("c1")),
            "补投必须被丢弃，否则它会挤掉真正该处理的 ask"
        );
        assert!(s.active_permission().is_none());
    }

    /// 反方向：没被解决过的同 id 重放仍然只保留一个面板，并刷新为最新一条。
    #[test]
    fn same_tool_call_redelivery_still_replaces_panel() {
        let mut s = SessionState::new("seed".into());
        assert!(s.queue_permission(perm("c1")));
        let mut updated = perm("c1");
        updated.reason = "新的理由".into();
        assert!(s.queue_permission(updated));
        assert_eq!(
            s.pending_permissions.len(),
            1,
            "同一 tool_call 只留一个面板"
        );
        assert_eq!(
            s.active_permission().map(|p| p.reason.as_str()),
            Some("新的理由")
        );
    }

    /// 历史必须有界（否则长会话里 `responded_permissions` 无界增长）。
    ///
    /// 这里直接读私有字段（同模块单测），不额外暴露只为测试存在的 API。
    #[test]
    fn responded_history_is_bounded() {
        let mut hist = RespondedPermissions::default();
        let total = RespondedPermissions::CAP + 32;
        for i in 0..total {
            hist.insert(&format!("c{i}"));
        }
        assert_eq!(hist.seen.len(), RespondedPermissions::CAP, "容量必须封顶");
        assert!(
            hist.contains(&format!("c{}", total - 1)),
            "最新的 id 必须记住"
        );
        assert!(!hist.contains("c0"), "最旧的 id 被淘汰");
        let oldest_survivor = format!("c{}", total - RespondedPermissions::CAP);

        // 重复 insert 必须幂等：既不增长，也**不淘汰别的 id**。
        // （只断言 `seen.len()` 是恒真的——`seen` 是 HashSet，重复插入本就不会变大；
        // 判别力在顺序表上：把已存在的 id 当新条目 push 会挤掉最旧的幸存者。）
        hist.insert(&format!("c{}", total - 1));
        assert_eq!(hist.order.len(), RespondedPermissions::CAP, "顺序表不堆积");
        assert!(
            hist.contains(&oldest_survivor),
            "重复 insert 不得淘汰别的 id（{oldest_survivor} 被挤掉了）"
        );
    }

    /// 建议项 1（PR #20 二轮）：晚到的 bootstrap 快照「只补不换」。
    ///
    /// 证伪方式：把 `restore_permission_from_snapshot` 换回 `queue_permission`
    /// （旧行为）——「已有面板不覆盖」与「详情不得被占位符降级」两条断言同时变红。
    #[test]
    fn late_snapshot_never_downgrades_live_panels() {
        let mut s = SessionState::new("seed".into());
        let mut live = perm("c1");
        live.reason = "需要写文件".into();
        live.risk = PermissionRisk::High;
        assert!(s.queue_permission(live));
        assert!(s.queue_permission(perm("c2")));

        // 快照晚到：只带 c1，且只有「（恢复中）」占位详情。
        let mut snapshot = perm("c1");
        snapshot.tool_name = "（恢复中）".into();
        snapshot.reason = String::new();
        assert!(
            !s.restore_permission_from_snapshot(snapshot),
            "已有面板不得被快照覆盖"
        );
        assert_eq!(s.pending_permissions.len(), 2, "快照里没有的条目不得被挤掉");
        assert_eq!(
            s.active_permission().map(|p| p.tool_call_id.as_str()),
            Some("c1"),
            "优先级（队首）不变"
        );
        assert_eq!(
            s.active_permission().map(|p| p.reason.as_str()),
            Some("需要写文件"),
            "详情不得被「（恢复中）」占位符降级"
        );
        assert_eq!(
            s.active_permission().map(|p| p.risk),
            Some(PermissionRisk::High)
        );

        // 快照里有、内存里没有的：补上（恢复语义仍然成立）。
        assert!(s.restore_permission_from_snapshot(perm("c9")));
        assert!(s.pending_permissions.iter().any(|p| p.tool_call_id == "c9"));
        // 已解决的 id 不被快照复活。
        s.resolve_permission("c9");
        assert!(!s.restore_permission_from_snapshot(perm("c9")));
    }
}
