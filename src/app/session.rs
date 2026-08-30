//! 单会话状态：timeline 模型、流式相位、挂起交互面板、composer、滚动。

use std::collections::VecDeque;

use crate::protocol::command::ConversationMode;
use crate::protocol::event::{
    ActivityState, AskMode, AskQuestion, ContentRef, DomainError, PermissionCategory,
    PermissionRisk, SkillsStatus, UsageInfo,
};
use crate::protocol::methods::SessionMetaView;
use crate::app::timeline_model::TimelineModel;
use crate::protocol::snapshot::ConversationStateView;

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
            if let Some(sel) = self.selections[idx] {
                if let Some(opt) = q.options.get(sel) {
                    out.push((q.id.clone(), opt.clone()));
                    continue;
                }
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
    pub todo_items: Vec<crate::protocol::event::TodoItem>,
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

/// 单行输入框（手写：char 粒度光标 + 历史）。
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

    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            if ch == '\n' || ch == '\r' {
                self.insert(' ');
            } else {
                self.insert(ch);
            }
        }
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

/// 已渲染 transcript 行缓存（model.version + 宽度键控）。
#[derive(Debug, Clone, Default)]
pub struct RenderedTranscript {
    pub version: u64,
    pub width: u16,
    pub lines: Vec<crate::app::render_line::RenderLine>,
}

// ───────────────────────── 会话状态 ─────────────────────────

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seed: String,
    pub meta: Option<SessionMetaView>,
    pub title: Option<String>,
    pub mode: ConversationMode,
    pub timeline: TimelineModel,
    /// bootstrap 的 conversation 快照视图（usage/model/context）。
    pub conversation: Option<ConversationStateView>,
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
    pub dashboard: Option<crate::protocol::event::DashboardSnapshot>,
    pub compact_status: Option<String>,
    /// 代码变更聚合（+行 / −行）。
    pub code_added: usize,
    pub code_removed: usize,
    pub last_error: Option<DomainError>,
    pub composer: Composer,
    pub scroll: ScrollState,
    pub rendered: Option<RenderedTranscript>,
    /// bootstrap / re-baseline 是否已就绪。
    pub ready: bool,
    /// 被 LRU 逐出 transcript 后，重新聚焦时需要 re-baseline。
    pub needs_rebaseline: bool,
    /// 加载更早：in-flight 去重。
    pub loading_older: bool,
}

impl SessionState {
    pub fn new(seed: String) -> Self {
        Self {
            seed,
            meta: None,
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
            compact_status: None,
            code_added: 0,
            code_removed: 0,
            last_error: None,
            composer: Composer::default(),
            scroll: ScrollState { follow: true, offset: 0 },
            rendered: None,
            ready: false,
            needs_rebaseline: false,
            loading_older: false,
        }
    }

    pub fn title(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_owned();
        }
        if let Some(meta) = &self.meta {
            return meta.display_title();
        }
        format!("session {}", self.seed)
    }

    pub fn display_model(&self) -> Option<String> {
        self.conversation
            .as_ref()
            .and_then(|c| c.model.clone())
            .or_else(|| self.meta.as_ref().and_then(|m| m.model.clone()))
    }

    /// 优先级：permission > ask > plan（winui 语义）。
    pub fn active_permission(&self) -> Option<&PermissionPanel> {
        self.pending_permissions.first()
    }

    pub fn is_waiting_user(&self) -> bool {
        self.activity == Some(ActivityState::WaitingUser)
            || self.pending_permissions.len() > 0
            || self.pending_ask.is_some()
            || self.pending_plan.is_some()
    }

    /// 状态栏标签（working / waiting / idle…）。
    pub fn activity_label(&self) -> String {
        if self.pending_permissions.len() > 0 {
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
            conv.context_limit = Some(context_limit);
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

/// turn state → 是否仍在流式（timeline 视角兜底）。
pub fn sync_streaming_from_timeline(session: &mut SessionState) {
    let timeline_streaming = session.timeline.is_streaming();
    match (&session.streaming, timeline_streaming) {
        (None, true) => {
            if let Some(turn) = session.timeline.turns.last() {
                session.streaming = Some(StreamingState {
                    turn_id: turn.turn_id.clone(),
                    phase: StreamPhase::Answering,
                    round_num: 0,
                    tool_name: None,
                });
            }
        }
        (Some(_), false) => {
            // timeline 已 sealed；等 TurnCompleted/Cancelled 事件收口，
            // 这里不提前清除（事件是权威终态）。
        }
        _ => {}
    }
}
