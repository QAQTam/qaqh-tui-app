//! 频道快照的 **UI 视图层**（宽松解析）。
//!
//! wire 侧的 `RingingSessionBootstrap` / `RingingChannelSnapshot` **不再镜像**：
//! 直接用 `qaqh_client` 的权威类型（`app/mod.rs` 的 bootstrap 路径早已如此，
//! 本仓那份副本只被自己的测试养着，且因 `pub` 项落在私有模块里逃过了
//! `dead_code` lint——`cargo build` 零警告）。
//!
//! 本模块只保留 TUI 自己的视图：bootstrap 的 `state` 是各频道的领域快照
//! payload（中立 JSON），解析失败一律降级为 None，绝不让 UI 崩溃。

use serde_json::Value;

use qaqh_client::TimelineTurn;
use qaqh_client::{DomainActivityState as ActivityState, SkillsStatus, UsageInfo};

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
            .map(|a| {
                a.iter()
                    .filter_map(|t| serde_json::from_value(t.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            usage: state
                .get("usage")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            usage_totals: state
                .get("usage_totals")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            usage_requests: state
                .get("usage_requests")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            cache_reported_requests: state
                .get("cache_reported_requests")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            model: state
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned),
            context_limit: state
                .get("context_limit")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
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
    pub dashboard: Option<qaqh_client::DomainDashboardSnapshot>,
}

impl ChannelStateView {
    pub fn parse_control(state: &Value) -> Self {
        Self {
            session_state: state
                .get("session_state")
                .and_then(Value::as_str)
                .map(str::to_owned),
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
                    kind: v
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("ask")
                        .to_owned(),
                })
            }),
            skills: state
                .get("skills")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
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
                    .map(|id| PendingPermissionView {
                        tool_call_id: id.to_owned(),
                    })
            }),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{RINGING_SCHEMA, RINGING_VERSION, RingingSessionBootstrap};
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
