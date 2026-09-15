//! Ringing 命令/事件信封与 ack（镜像 `qaqh-ringing/src/envelope.rs`、`reset.rs`）。

use serde::{Deserialize, Serialize};

use super::event::RingingEvent;
use super::{Channel, Delivery, is_safe_integer};

/// 事件信封。注意 M4 瘦身后 wire 上**没有** schema/version/channel/epoch 字段：
/// 版本由端点 URL 承担，epoch/channel 由 SSE 帧 id 承担。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingEventEnvelope {
    pub delivery: Delivery,
    pub seed: String,
    /// 每 (server_epoch, channel) 全局递增。
    pub stream_seq: u64,
    /// 每 (seed, channel) 递增。
    pub channel_seq: u64,
    /// 每 session/channel 因果序。
    pub session_seq: u64,
    /// 事件唯一 id；同 id 至少一次投递但只允许应用一次。
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ts: Option<u64>,
    pub event: RingingEvent,
}

impl RingingEventEnvelope {
    /// 信封内的 channel 必须与所在 SSE 连接一致。
    ///
    /// 阶段一后**生产路径不再消费**它（帧的频道校验已由 `qaqh-client` 负责），
    /// 仅由本文件的镜像保真测试使用——保留是为了让「镜像与权威类型同形」这条
    /// 前提在本仓仍可被断言。阶段二整体删除 `protocol/` 时一并消失。
    #[allow(dead_code)]
    pub fn channel(&self) -> Channel {
        self.event.channel()
    }

    /// 见 [`Self::channel`]：阶段一后仅测试消费。
    #[allow(dead_code)]
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.seed.is_empty()
            || self.event_id.is_empty()
            || !is_safe_integer(self.stream_seq)
            || !is_safe_integer(self.channel_seq)
            || !is_safe_integer(self.session_seq)
            || self.state_revision.is_some_and(|v| !is_safe_integer(v))
        {
            return Err("invalid_envelope");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Accepted,
    Rejected,
}

/// 命令确认。accepted 仅代表进入 actor；业务终态经 `causation_id == command_id`
/// 的可靠事件（或 receipt 端点）返回。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingCommandAck {
    pub command_id: String,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Rejected,
}

impl CommandState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CommandState::Succeeded | CommandState::Failed | CommandState::Rejected
        )
    }
}

/// `GET /ringing/v1/commands/{command_id}` 的 receipt。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingCommandStatus {
    pub command_id: String,
    pub state: CommandState,
    pub payload_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// `event: ringing.reset_required` 的 data payload（镜像 `qaqh-ringing/src/reset.rs`）。
/// cursor 超出可靠 journal 保留窗口时下发；客户端必须重新拉取对应频道快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingResetRequired {
    pub channel: Channel,
    pub seed: String,
    pub earliest_available_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::event::ControlEvent;

    #[test]
    fn event_envelope_channel_matches_connection() {
        let json = r#"{
            "delivery": "reliable",
            "seed": "0123abcd",
            "stream_seq": 7,
            "channel_seq": 3,
            "session_seq": 3,
            "event_id": "e7",
            "event": {
                "channel": "control",
                "type": "session_state_changed",
                "seed": "0123abcd",
                "state": "created"
            }
        }"#;
        let env: RingingEventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.channel(), Channel::Control);
        assert!(env.validate().is_ok());
        assert!(matches!(
            env.event,
            RingingEvent::Control(ControlEvent::SessionStateChanged { .. })
        ));
    }
}
