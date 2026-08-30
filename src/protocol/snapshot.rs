//! 频道快照与 bootstrap（镜像 `qaqh-ringing/src/snapshot.rs`）。
//!
//! bootstrap 的 `state` 是各频道的领域快照 payload（中立 JSON）。本模块
//! 提供宽松解析的 UI 视图：解析失败一律降级为 None，绝不让 UI 崩溃。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::event::{ActivityState, SkillsStatus, UsageInfo};
use super::timeline::TimelineTurn;
use super::Channel;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingChannelSnapshot {
    pub schema: String,
    pub version: u32,
    pub channel: Channel,
    pub seed: String,
    /// 快照覆盖到的 stream_seq 基线（其后的可靠事件需从 cursor 回放）。
    pub baseline_stream_seq: u64,
    pub state_revision: u64,
    pub snapshot_version: u32,
    pub state: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingSessionBootstrap {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub control: RingingChannelSnapshot,
    pub conversation: RingingChannelSnapshot,
    pub tool: RingingChannelSnapshot,
}

impl RingingSessionBootstrap {
    #[allow(dead_code)]
    pub fn channel_snapshot(&self, channel: Channel) -> Option<&RingingChannelSnapshot> {
        match channel {
            Channel::Control => Some(&self.control),
            Channel::Conversation => Some(&self.conversation),
            Channel::Tool => Some(&self.tool),
        }
    }
}

/// UI 需要的 conversation 频道快照视图（宽松解析）。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ConversationStateView {
    pub usage: Option<UsageInfo>,
    pub usage_totals: Option<UsageInfo>,
    pub usage_requests: Option<u32>,
    pub cache_reported_requests: Option<u32>,
    pub model: Option<String>,
    pub context_limit: Option<u32>,
    /// 持久化 transcript 投影（仅用于没有 timeline 可用时的降级展示）。
    pub turns: Vec<TimelineTurn>,
}

impl ConversationStateView {
    pub fn parse(state: &Value) -> Self {
        let turns = state
            .get("turns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| serde_json::from_value(t.clone()).ok()).collect())
            .unwrap_or_default();
        Self {
            usage: state.get("usage").and_then(|v| serde_json::from_value(v.clone()).ok()),
            usage_totals: state
                .get("usage_totals")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            usage_requests: state.get("usage_requests").and_then(Value::as_u64).map(|v| v as u32),
            cache_reported_requests: state
                .get("cache_reported_requests")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            model: state.get("model").and_then(Value::as_str).map(str::to_owned),
            context_limit: state.get("context_limit").and_then(Value::as_u64).map(|v| v as u32),
            turns,
        }
    }
}

/// control 频道快照中的挂起交互（bootstrap 恢复视图）。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PendingInteractionView {
    pub id: String,
    /// "ask" | "plan"
    pub kind: String,
}

/// tool 频道快照中的挂起权限。
#[derive(Debug, Clone)]
pub struct PendingPermissionView {
    pub tool_call_id: String,
}

/// UI 需要的 control/tool 频道快照视图（宽松解析）。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ChannelStateView {
    pub session_state: Option<String>,
    pub activity: Option<ActivityState>,
    pub agent_lifecycle: Option<String>,
    pub config_rev: Option<u64>,
    pub pending_interaction: Option<PendingInteractionView>,
    pub skills: Option<SkillsStatus>,
    pub pending_permission: Option<PendingPermissionView>,
    /// bootstrap control state 内置的仪表盘快照（todo/最近改动）。
    pub dashboard: Option<crate::protocol::event::DashboardSnapshot>,
}

impl ChannelStateView {
    pub fn parse_control(state: &Value) -> Self {
        Self {
            session_state: state.get("session_state").and_then(Value::as_str).map(str::to_owned),
            activity: state.get("activity").and_then(|v| {
                // activity 可能是 {state: "..."} 或直接字符串。
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.get("state").and_then(Value::as_str).map(str::to_owned))
                    .and_then(|s| serde_json::from_value(Value::String(s)).ok())
            }),
            agent_lifecycle: state
                .get("agent_lifecycle")
                .and_then(Value::as_str)
                .map(str::to_owned),
            config_rev: state.get("config_rev").and_then(Value::as_u64),
            pending_interaction: state.get("pending_interaction").and_then(|v| {
                if v.is_null() {
                    return None;
                }
                Some(PendingInteractionView {
                    id: v.get("id").and_then(Value::as_str)?.to_owned(),
                    kind: v.get("kind").and_then(Value::as_str).unwrap_or("ask").to_owned(),
                })
            }),
            skills: state.get("skills").and_then(|v| serde_json::from_value(v.clone()).ok()),
            dashboard: state
                .get("dashboard_snapshot")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            pending_permission: None,
        }
    }

    pub fn parse_tool(state: &Value) -> Self {
        Self {
            pending_permission: state.get("pending_permission").and_then(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .map(|id| PendingPermissionView { tool_call_id: id.to_owned() })
            }),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};
    use serde_json::json;

    #[test]
    fn bootstrap_round_trip() {
        let json = json!({
            "schema": RINGING_SCHEMA,
            "version": RINGING_VERSION,
            "server_epoch": "ep1",
            "seed": "0123abcd",
            "control": {
                "schema": RINGING_SCHEMA, "version": RINGING_VERSION,
                "channel": "control", "seed": "0123abcd",
                "baseline_stream_seq": 5, "state_revision": 2, "snapshot_version": 1,
                "state": { "session_state": "resumed", "pending_interaction": null,
                "dashboard_snapshot": { "seed": "0123abcd", "documents": [], "recent_edits": ["a.rs"],
                    "tasks": [{"id": "1", "subject": "做A", "description": "", "status": "in_progress"}],
                    "current_todo_id": "1" } }
            },
            "conversation": {
                "schema": RINGING_SCHEMA, "version": RINGING_VERSION,
                "channel": "conversation", "seed": "0123abcd",
                "baseline_stream_seq": 9, "state_revision": 4, "snapshot_version": 1,
                "state": { "turns": [], "total_turns": 0, "model": "m1", "context_limit": 128000 }
            },
            "tool": {
                "schema": RINGING_SCHEMA, "version": RINGING_VERSION,
                "channel": "tool", "seed": "0123abcd",
                "baseline_stream_seq": 3, "state_revision": 1, "snapshot_version": 1,
                "state": { "running": [], "pending_permission": null }
            }
        });
        let b: RingingSessionBootstrap = serde_json::from_value(json).unwrap();
        assert_eq!(b.server_epoch, "ep1");
        let conv = ConversationStateView::parse(&b.conversation.state);
        assert_eq!(conv.model.as_deref(), Some("m1"));
        assert_eq!(conv.context_limit, Some(128000));
        let ctl = ChannelStateView::parse_control(&b.control.state);
        assert_eq!(ctl.session_state.as_deref(), Some("resumed"));
        assert!(ctl.pending_interaction.is_none());
        let dash = ctl.dashboard.expect("dashboard_snapshot");
        assert_eq!(dash.tasks.len(), 1);
        assert_eq!(dash.tasks[0].subject, "做A");
        assert_eq!(dash.tasks[0].status, "in_progress");
        assert_eq!(dash.current_todo_id.as_deref(), Some("1"));
        let tool = ChannelStateView::parse_tool(&b.tool.state);
        assert!(tool.pending_permission.is_none());
    }
}
