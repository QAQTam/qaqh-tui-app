//! Ringing 命令/事件信封与 ack（镜像 `qaqh-ringing/src/envelope.rs`、`reset.rs`）。

use serde::{Deserialize, Serialize};

use super::command::RingingCommand;
use super::event::RingingEvent;
use super::{Channel, Delivery, RINGING_SCHEMA, RINGING_VERSION, is_safe_integer};

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
    /// 信封内的 channel 必须与所在 SSE 连接一致（transport 层校验）。
    pub fn channel(&self) -> Channel {
        self.event.channel()
    }

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

/// 命令信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingCommandEnvelope {
    pub schema: String,
    pub version: u32,
    pub channel: Channel,
    /// 命令幂等 id：accepted 前可安全重试（服务端按 payload 指纹去重）。
    pub command_id: String,
    pub client_instance_id: String,
    pub client_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub command: RingingCommand,
}

impl RingingCommandEnvelope {
    pub fn new(
        command_id: impl Into<String>,
        client_instance_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel: command.channel(),
            command_id: command_id.into(),
            client_instance_id: client_instance_id.into(),
            client_session_id: String::new(),
            seed: None,
            expected_revision: None,
            command,
        }
    }

    pub fn with_seed(mut self, seed: impl Into<String>) -> Self {
        self.seed = Some(seed.into());
        self
    }

    pub fn with_client_session_id(mut self, client_session_id: impl Into<String>) -> Self {
        self.client_session_id = client_session_id.into();
        self
    }

    /// 与后端 `RingingCommandEnvelope::validate` 等价的客户端预检。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA || self.version != RINGING_VERSION {
            return Err("unsupported_version");
        }
        if self.command.channel() != self.channel {
            return Err("invalid_envelope");
        }
        if self.command_id.is_empty() || self.client_instance_id.is_empty() {
            return Err("invalid_envelope");
        }
        if self.client_session_id.is_empty() {
            return Err("lease_required");
        }
        if self.seed.as_deref().is_some_and(str::is_empty) {
            return Err("invalid_envelope");
        }
        if self.seed.is_none() && !self.command.is_session_create() {
            return Err("missing_seed");
        }
        if self.expected_revision.is_some_and(|v| !is_safe_integer(v)) {
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
        matches!(self, CommandState::Succeeded | CommandState::Failed | CommandState::Rejected)
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
    use crate::protocol::command::ControlCommand;
    use crate::protocol::event::ControlEvent;

    #[test]
    fn command_envelope_validation() {
        let env = RingingCommandEnvelope::new(
            "cmd-1",
            "inst-1",
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: vec![],
            }),
        );
        assert!(env.validate().is_err()); // 缺 client_session_id
        let env = env.with_client_session_id("sess-1");
        assert!(env.validate().is_ok()); // session_create 是唯一允许无 seed 的命令

        let env = RingingCommandEnvelope::new(
            "cmd-2",
            "inst-1",
            RingingCommand::Control(ControlCommand::SkillsReload),
        )
        .with_client_session_id("sess-1");
        assert_eq!(env.validate(), Err("missing_seed"));
        let env = env.with_seed("0123abcd");
        assert!(env.validate().is_ok());
    }

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
