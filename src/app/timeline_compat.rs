//! TUI-owned transcript model retained while the renderer moves to v2.
//!
//! The backend removed its legacy timeline wire API. These types are now only
//! an internal view model for the existing transcript reducer and renderer;
//! they must never be sent to or received from `qaqh-client`.

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
    Cancelled,
    Backgrounded,
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
    #[serde(default)]
    pub level_name: String,
    pub risk: String,
    pub consequence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolDisplay {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    #[serde(default)]
    pub header: Option<TimelineToolHeader>,
    #[serde(default)]
    pub body: Option<TimelineToolBody>,
    #[serde(default)]
    pub metrics: Option<TimelineToolMetrics>,
    #[serde(default)]
    pub outcome: Option<TimelineToolOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolOutcome {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub truncated: Option<bool>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub output_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineToolHeader {
    Path {
        path: String,
        op: TimelinePathOp,
    },
    Shell {
        command: String,
    },
    Query {
        query: String,
        #[serde(default)]
        scope: Option<String>,
    },
    Other {
        label: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelinePathOp {
    Read,
    Write,
    Edit,
    List,
    Patch,
    Delete,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineToolBody {
    None,
    Text {
        text: String,
        #[serde(default)]
        truncated: bool,
    },
    Diff {
        unified: String,
        #[serde(default)]
        files: Vec<String>,
    },
    Shell {
        output: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
    },
    Streams {
        stdout: String,
        stderr: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        interleaved: bool,
    },
    Subagent {
        name: String,
        #[serde(rename = "session_id")]
        session_id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolMetrics {
    pub elapsed_ms: u64,
    #[serde(default)]
    pub output_bytes: u64,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub effective_tool_name: Option<String>,
    #[serde(default)]
    pub user_initiated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub args_json: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub progress: String,
    #[serde(default)]
    pub progress_truncated: bool,
    #[serde(default)]
    pub progress_stream: Option<String>,
    #[serde(default)]
    pub progress_bytes_total: u64,
    #[serde(default)]
    pub display: Option<TimelineToolDisplay>,
    #[serde(default)]
    pub failure: Option<TimelineFailure>,
    #[serde(default)]
    pub permission: Option<TimelineToolPermission>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub completed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineBlock {
    pub block_id: String,
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool: Option<TimelineTool>,
    #[serde(default)]
    pub reasoning_kind: Option<qaqh_client::ReasoningKind>,
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
    pub turn_index: Option<u64>,
    #[serde(default)]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    #[serde(default)]
    pub offloaded: bool,
    pub state: TimelineTurnState,
    #[serde(default)]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSnapshot {
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEvent {
    TurnOpened {
        user_text: String,
    },
    BlockOpened {
        block: TimelineBlock,
    },
    TextDelta {
        block_id: String,
        fragment_seq: u64,
        delta: String,
    },
    BlockCheckpoint {
        block_id: String,
        #[serde(default)]
        arg: Option<String>,
        #[serde(default)]
        text: String,
    },
    ToolUpdated {
        block_id: String,
        tool: TimelineTool,
    },
    ToolProgress {
        block_id: String,
        chunk: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        stream: Option<String>,
        #[serde(default)]
        bytes_total: u64,
    },
    ToolEstimated {
        block_id: String,
        lines_added: u32,
        lines_removed: u32,
    },
    UsageSampled {
        usage: qaqh_client::UsageInfo,
    },
    ProviderRetrying {
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error: String,
    },
    BlockSealed {
        block_id: String,
    },
    RoundSealed {
        is_final: bool,
    },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default)]
        failure: Option<TimelineFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default)]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub session_id: String,
    pub snapshot: TimelineSnapshot,
    pub has_more: bool,
    pub total_turns: usize,
    pub truncated_before: bool,
}
