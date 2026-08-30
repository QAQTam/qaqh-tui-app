//! 三频道命令（镜像 `qaqh-domain/src/command.rs`；serde tag/别名必须逐字对齐）。

use serde::{Deserialize, Serialize};

use super::event::ContentRef;
use super::Channel;

/// 用户消息中的图片附件（multimodal）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// MIME type（如 "image/png"）。
    pub mime_type: String,
    /// Base64 编码数据（无 data URI 前缀）。
    pub data: String,
}

/// ask_user 表单中的单个答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub question_id: String,
    pub answer: String,
}

/// 会话工作模式。`Code` 是默认值（旧值 `normal` 兼容反序列化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMode {
    Plan,
    #[serde(rename = "code", alias = "normal")]
    Code,
}

#[allow(dead_code)]
impl ConversationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationMode::Plan => "plan",
            ConversationMode::Code => "code",
        }
    }
}

/// Control 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// 创建新会话（唯一允许 envelope 无 seed 的命令）。
    SessionCreate {
        #[serde(default)]
        close_current: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        custom_tools: Vec<String>,
    },
    /// 恢复已保存会话；daemon 会把 seed attach 到当前 lease。
    SessionResume { seed: String },
    SessionClose { seed: String },
    SessionArchive { seed: String },
    SessionUnarchive { seed: String },
    SessionDelete { seed: String },
    SessionShutdown,
    AgentReloadConfig,
    SetToolMode {
        #[serde(default)]
        tool_mode: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        custom_tools: Vec<String>,
    },
    InteractionAskRespond {
        interaction_id: String,
        answers: Vec<AskAnswer>,
    },
    InteractionAskDismiss { interaction_id: String },
    PlanReviewRespond {
        interaction_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default)]
        autonomous: bool,
    },
    SkillsActivate { name: String },
    SkillsReload,
    SkillsOperation {
        operation_id: String,
        action: String,
        name: String,
    },
}

/// Conversation 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationCommand {
    ConversationSendMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
        /// 上传后的内容引用；命令中禁止出现本地路径。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<ContentRef>>,
        #[serde(default)]
        as_system: bool,
    },
    ConversationCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    ConversationUndoTurn { turn_id: String },
    ConversationCompact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// 服务端明确拒绝（422 unsupported_command）：bootstrap 已含完整持久化历史。
    ConversationLoadMore {
        before_turn_id: String,
        #[serde(default = "default_load_count")]
        count: u32,
    },
    ConversationSetMode { mode: ConversationMode },
}

fn default_load_count() -> u32 {
    20
}

/// Tool 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCommand {
    ToolInvoke {
        tool_call_id: String,
        name: String,
        action: String,
        args: serde_json::Value,
    },
    ToolPermissionRespond {
        tool_call_id: String,
        approved: bool,
        #[serde(default)]
        trust_folder: bool,
    },
}

/// 统一命令入口。`channel()` 决定 POST /ringing/v1/commands/{channel} 的频道段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum RingingCommand {
    Control(ControlCommand),
    Conversation(ConversationCommand),
    Tool(ToolCommand),
}

impl RingingCommand {
    pub fn channel(&self) -> Channel {
        match self {
            RingingCommand::Control(_) => Channel::Control,
            RingingCommand::Conversation(_) => Channel::Conversation,
            RingingCommand::Tool(_) => Channel::Tool,
        }
    }

    pub fn is_session_create(&self) -> bool {
        matches!(self, RingingCommand::Control(ControlCommand::SessionCreate { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_matches_backend() {
        let cmd = RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
            text: "hi".into(),
            images: vec![],
            attachments: None,
            as_system: false,
        });
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["channel"], "conversation");
        assert_eq!(json["type"], "conversation_send_message");
        // 空默认字段不上 wire（与后端 skip_serializing_if 一致）。
        assert!(json.get("images").is_none());
        assert!(json.get("attachments").is_none());
        assert_eq!(json["as_system"], false);

        let mode = RingingCommand::Conversation(ConversationCommand::ConversationSetMode {
            mode: ConversationMode::Plan,
        });
        let json = serde_json::to_value(&mode).unwrap();
        assert_eq!(json["mode"], "plan");
    }

    #[test]
    fn normal_alias_decodes_to_code() {
        let mode: ConversationMode = serde_json::from_str("\"normal\"").unwrap();
        assert_eq!(mode, ConversationMode::Code);
    }
}
