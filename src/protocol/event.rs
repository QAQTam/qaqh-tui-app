//! 三频道事件（镜像 `qaqh-domain/src/event.rs`；36 个变体全量镜像）。
//! 共享类型 `UsageInfo`/`ContentRef`/`ToolResult` 镜像自 `qaqh-types`。

use serde::{Deserialize, Serialize};

use super::Channel;
use super::Delivery;

// ───────────────────────── 共享支持类型 ─────────────────────────

/// Token 用量（镜像 `qaqh-types/src/api_types.rs`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_usage_reported: Option<bool>,
}

/// 内容引用（镜像 `qaqh-types/src/tool_result.rs`）。命令中只传引用不传路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Partial,
    Backgrounded,
    Cancelled,
}

impl ToolStatus {
    #[allow(dead_code)]
    pub fn is_success(self) -> bool {
        matches!(self, ToolStatus::Ok | ToolStatus::Backgrounded)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContinuation {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolModelPayload {
    pub text: String,
    pub truncated: bool,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    pub data: String,
}

/// 工具执行权威结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub summary: String,
    pub data: serde_json::Value,
    pub model: ToolModelPayload,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
    /// 展示平面 unified diff（绝不进入模型投影）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDeltaKind {
    Thinking,
    ToolCalling,
    Answering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolState {
    InProgress,
    Searching,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactStatus {
    Completed,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
    Read,
    Write,
    Exec,
    Net,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Resumed,
    Closed,
    Archived,
    Unarchived,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Starting,
    Idle,
    Working,
    WaitingUser,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Booting,
    Ready,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardDocument {
    pub tag: String,
    pub path: String,
    pub turns_since_read: u32,
    pub is_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub seed: String,
    pub documents: Vec<DashboardDocument>,
    pub recent_edits: Vec<String>,
    pub tasks: Vec<DashboardTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_todo_id: Option<String>,
}

/// 失败终态的错误域。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainError {
    pub error_id: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorScope {
    Control,
    Conversation,
    Tool,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    Single,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskResolution {
    Answered,
    Dismissed,
}

/// ask_user 中的单个问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_custom: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// "project" | "user"
    pub scope: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeInfo {
    pub name: String,
    pub description: String,
    /// "catalog" | "requested" | "active" | "unavailable"
    pub state: String,
    pub source: String,
    pub token_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsStatus {
    pub available: Vec<SkillInfo>,
    pub active: Vec<String>,
    #[serde(default)]
    pub catalog_revision: String,
    #[serde(default)]
    pub context_epoch: u64,
    #[serde(default)]
    pub operation_revision: u64,
    #[serde(default)]
    pub token_budget: usize,
    #[serde(default)]
    pub token_usage: usize,
    #[serde(default)]
    pub runtime: Vec<SkillRuntimeInfo>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ───────────────────────── Conversation 频道 ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEvent {
    /// 新回合开始的权威事件。
    TurnStarted { turn_id: String, user_text: String },
    TurnCompleted {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageInfo>,
    },
    TurnFailed { turn_id: String, error: DomainError },
    /// 流式增量（reliable：追加语义）。
    RoundDelta {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        delta: String,
    },
    /// 流式块周期完整值（replaceable，覆盖语义，自愈丢增量）。
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        text: String,
        char_count: u32,
    },
    /// 一轮 API 调用完成的权威终态。
    RoundCompleted {
        turn_id: String,
        round_num: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<ContentRef>,
        is_final: bool,
    },
    ProviderRetrying {
        turn_id: String,
        round_num: u32,
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error_message: String,
    },
    ProviderToolStatus {
        turn_id: String,
        round_num: u32,
        call_id: String,
        tool_kind: String,
        state: ProviderToolState,
    },
    UsageUpdated {
        turn_id: String,
        round_num: u32,
        usage: UsageInfo,
        context_limit: u32,
        model: String,
    },
    CompactStarted {
        compact_id: String,
        turns_total: u32,
        turns_keeping: u32,
    },
    CompactProgress { compact_id: String, delta: String },
    CompactFinished {
        compact_id: String,
        status: CompactStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_chars: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_compacted: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_removed: Option<u32>,
    },
    ConversationCancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

// ───────────────────────── Tool 频道 ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolEvent {
    /// replaceable 预览，可被 ToolStarted 覆盖。
    ToolCallPrepared {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        name: String,
        args_so_far: String,
    },
    ToolStarted {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        name: String,
    },
    ToolFinished {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        result: ToolResult,
    },
    /// 权限请求：agent 挂起回合等待用户批准/拒绝（回复 ToolPermissionRespond）。
    ToolPermissionRequested {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        tool_name: String,
        reason: String,
        paths: Vec<String>,
        category: PermissionCategory,
        level: u8,
        risk: PermissionRisk,
        consequence: String,
    },
    ToolNotice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        level: NoticeLevel,
        message: String,
    },
    AuditRecorded {
        tool_name: String,
        result_summary: String,
        success: bool,
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_ref: Option<ContentRef>,
    },
    CodeChanged {
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        turn_id: String,
        #[serde(default)]
        round_num: u32,
        lines_added: usize,
        lines_removed: usize,
        files_created: usize,
        files_deleted: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
}

impl ToolEvent {
    /// daemon 侧权限恢复路径没有独立 resolved 事件；消费端以
    /// ToolStarted/ToolFinished 兜底清除挂起权限面板。
    #[allow(dead_code)]
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            ToolEvent::ToolCallPrepared { tool_call_id, .. }
            | ToolEvent::ToolStarted { tool_call_id, .. }
            | ToolEvent::ToolFinished { tool_call_id, .. }
            | ToolEvent::ToolPermissionRequested { tool_call_id, .. } => Some(tool_call_id),
            ToolEvent::ToolNotice { tool_call_id, .. } => tool_call_id.as_deref(),
            ToolEvent::CodeChanged { tool_call_id, .. } if !tool_call_id.is_empty() => {
                Some(tool_call_id)
            }
            ToolEvent::AuditRecorded { .. } | ToolEvent::CodeChanged { .. } => None,
        }
    }
}

// ───────────────────────── Control 频道 ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    SessionStateChanged { seed: String, state: SessionState },
    /// seed 惯例为空串（全局广播）；消费后重拉 config.load。
    ConfigChanged { rev: u64 },
    SessionActivityChanged {
        seed: String,
        state: ActivityState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        seq: u64,
        updated_at: u64,
    },
    SessionMetaChanged {
        seed: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    AgentLifecycleChanged { state: AgentLifecycleState },
    DashboardUpdated {
        hp_connected: bool,
        session_seed: String,
        tool_calls_total: u32,
        tool_failures: u32,
        current_phase: String,
        streaming: bool,
    },
    DashboardSnapshot { snapshot: DashboardSnapshot },
    /// ask_user 请求（ask/plan 归 Control，permission 归 Tool）。
    InteractionRequested {
        interaction_id: String,
        turn_id: String,
        mode: AskMode,
        questions: Vec<AskQuestion>,
    },
    InteractionResolved {
        interaction_id: String,
        resolution: AskResolution,
    },
    PlanReviewRequested {
        interaction_id: String,
        turn_id: String,
        plan_content: String,
        #[serde(default)]
        review_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        todo_items: Option<Vec<TodoItem>>,
    },
    PlanReviewResolved {
        interaction_id: String,
        approved: bool,
    },
    SkillsUpdated {
        available: Vec<SkillInfo>,
        active: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catalog_revision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_revision: Option<u64>,
        #[serde(default)]
        context_epoch: usize,
        #[serde(default)]
        token_budget: usize,
        #[serde(default)]
        token_usage: usize,
        #[serde(default)]
        runtime: Vec<SkillRuntimeInfo>,
        #[serde(default)]
        diagnostics: Vec<String>,
    },
    SystemNotice {
        notice_id: String,
        level: NoticeLevel,
        message: String,
    },
    SubagentStatus {
        seed: String,
        name: String,
        state: String,
    },
    OperationCompleted {
        occurrence_id: String,
        scope: ErrorScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
    OperationFailed {
        occurrence_id: String,
        scope: ErrorScope,
        error: DomainError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
}

// ───────────────────────── 统一事件入口 ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum RingingEvent {
    Control(ControlEvent),
    Conversation(ConversationEvent),
    Tool(ToolEvent),
}

#[allow(dead_code)]
impl RingingEvent {
    pub fn channel(&self) -> Channel {
        match self {
            RingingEvent::Control(_) => Channel::Control,
            RingingEvent::Conversation(_) => Channel::Conversation,
            RingingEvent::Tool(_) => Channel::Tool,
        }
    }

    pub fn delivery(&self) -> Delivery {
        match self {
            RingingEvent::Control(e) => e.delivery(),
            RingingEvent::Conversation(e) => e.delivery(),
            RingingEvent::Tool(e) => e.delivery(),
        }
    }
}

#[allow(dead_code)]
impl ConversationEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ConversationEvent::ProviderToolStatus { .. }
            | ConversationEvent::UsageUpdated { .. }
            | ConversationEvent::CompactProgress { .. }
            | ConversationEvent::BlockCheckpoint { .. } => Delivery::Replaceable,
            _ => Delivery::Reliable,
        }
    }
}

#[allow(dead_code)]
impl ToolEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ToolEvent::ToolCallPrepared { .. } => Delivery::Replaceable,
            _ => Delivery::Reliable,
        }
    }
}

#[allow(dead_code)]
impl ControlEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ControlEvent::DashboardUpdated { .. } | ControlEvent::DashboardSnapshot { .. } => {
                Delivery::Replaceable
            }
            _ => Delivery::Reliable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants_round_trip_with_backend_tags() {
        // 抽样验证 tag/casing 与后端一致；全量形状由 daemon 集成自检覆盖。
        let cases: Vec<(RingingEvent, &str, &str)> = vec![
            (
                RingingEvent::Conversation(ConversationEvent::RoundDelta {
                    turn_id: "t".into(),
                    round_num: 0,
                    kind: RoundDeltaKind::Answering,
                    delta: "x".into(),
                }),
                "conversation",
                "round_delta",
            ),
            (
                RingingEvent::Tool(ToolEvent::ToolPermissionRequested {
                    tool_call_id: "c".into(),
                    turn_id: "t".into(),
                    round_num: 0,
                    tool_name: "exec".into(),
                    reason: String::new(),
                    paths: vec![],
                    category: PermissionCategory::Write,
                    level: 2,
                    risk: PermissionRisk::High,
                    consequence: String::new(),
                }),
                "tool",
                "tool_permission_requested",
            ),
            (
                RingingEvent::Control(ControlEvent::InteractionRequested {
                    interaction_id: "i".into(),
                    turn_id: "t".into(),
                    mode: AskMode::Single,
                    questions: vec![],
                }),
                "control",
                "interaction_requested",
            ),
        ];
        for (event, channel, ty) in cases {
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["channel"], channel);
            assert_eq!(json["type"], ty);
            let back: RingingEvent = serde_json::from_value(json).unwrap();
            assert_eq!(back.channel(), event.channel());
        }
    }
}
