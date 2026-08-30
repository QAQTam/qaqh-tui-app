//! 会话 transcript 的权威模型（镜像 `qaqh-domain/src/timeline.rs`）。
//!
//! timeline 是唯一历史真源（PLAN N6）：bootstrap/timeline HTTP 快照 +
//! timeline SSE 严格 +1 光标；gap 一律 re-baseline，禁止本地猜测。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineBlockKind {
    Reasoning,
    Text,
    Tool,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineBlockState {
    Open,
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineToolState {
    Prepared,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineFailure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolPermission {
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<TimelineToolPermission>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineBlock {
    pub block_id: String,
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineRound {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<TimelineBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTurn {
    pub turn_id: String,
    #[serde(default)]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    pub state: TimelineTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

/// 权威恢复状态：`watermark` 是 `turns` 覆盖到的最大 timeline 序号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSnapshot {
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

/// 一次 transcript 变更（严格 +1 消费）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEvent {
    TurnOpened { user_text: String },
    BlockOpened { block: TimelineBlock },
    /// `fragment_seq` 在单个文本/推理块内单调。
    TextDelta {
        block_id: String,
        fragment_seq: u64,
        delta: String,
    },
    /// 块的周期完整值（replaceable，覆盖语义，自愈丢失/乱序增量）。
    BlockCheckpoint { block_id: String, text: String },
    ToolUpdated { block_id: String, tool: TimelineTool },
    ToolProgress { block_id: String, chunk: String },
    BlockSealed { block_id: String },
    RoundSealed { is_final: bool },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// 对单个 (server epoch, seed) 严格单调。
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

/// `GET /ringing/v1/sessions/{seed}/timeline` 分页响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub snapshot: TimelineSnapshot,
    pub has_more: bool,
    pub total_turns: usize,
}
