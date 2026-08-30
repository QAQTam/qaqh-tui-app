//! 协议类型镜像（PLAN.md §4 类型镜像纪律）。
//!
//! 手工镜像 `QAQ-Harness/crates/qaqh-ringing` 与 `qaqh-domain` 的 wire 形状。
//! **改动须对照后端 PR**；镜像以 `F:\QAQ-Harness` 2026-08 协议瘦身后的最终形态
//! 为基准（协议版本 1）。禁止在本仓散落协议字面量，全部 import 自本模块。
//!
//! 对应关系：
//! - `qaqh-ringing/src/protocol.rs` → [`mod.rs`] 常量
//! - `qaqh-ringing/src/capability.rs` → [`capability`]
//! - `qaqh-ringing/src/envelope.rs` → [`envelope`]
//! - `qaqh-domain/src/command.rs` → [`command`]
//! - `qaqh-domain/src/event.rs` → [`event`]
//! - `qaqh-domain/src/timeline.rs` → [`timeline`]
//! - `qaqh-ringing/src/{snapshot,reset}.rs` → [`snapshot`]
//! - `qaqh-runtime/src/ringing/service_methods.rs` → [`methods`]

pub mod capability;
pub mod command;
pub mod envelope;
pub mod event;
pub mod methods;
pub mod snapshot;
pub mod timeline;

use serde::{Deserialize, Serialize};

/// Ringing 协议 schema 标识（`qaqh-ringing/src/protocol.rs:4`）。
pub const RINGING_SCHEMA: &str = "qaqh.Ringing";
/// Ringing 协议版本（同上 :7）。代差不匹配时 daemon 返回 426 `unsupported_version`。
pub const RINGING_VERSION: u32 = 1;
/// 连接级身份 header（同上 :13）。每个请求与 SSE 连接都必须携带。
pub const SESSION_ID_HEADER: &str = "X-QAQH-Client-Session-Id";

/// SSE 帧 id 中的频道段与断点续传 header 名。
pub const LAST_EVENT_ID_HEADER: &str = "Last-Event-ID";

/// JS 安全整数上限；协议中全部 seq/revision 不得超过（同上 :23）。
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn is_safe_integer(v: u64) -> bool {
    v <= MAX_SAFE_INTEGER
}

/// Ringing 三频道（`qaqh-domain/src/channel.rs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Control,
    Conversation,
    Tool,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Control, Channel::Conversation, Channel::Tool];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Conversation => "conversation",
            Channel::Tool => "tool",
        }
    }

    /// URL path 段（与 as_str 相同）。
    pub fn path_segment(self) -> &'static str {
        self.as_str()
    }

    /// SSE 帧 id 频道段：`<epoch>:<channel>:<seq>`。
    #[allow(dead_code)]
    pub fn from_path_segment(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

/// 事件可靠性等级（`qaqh-domain/src/delivery.rs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Reliable,
    Replaceable,
    Ephemeral,
}

/// daemon 统一 JSON 错误体（HTTP 4xx/5xx 的 body）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_round_trip() {
        for c in Channel::ALL {
            assert_eq!(c.as_str(), serde_json::to_string(&c).unwrap().trim_matches('"'));
            assert_eq!(Channel::from_path_segment(c.as_str()), Some(c));
        }
        assert_eq!(Channel::from_path_segment("bogus"), None);
    }
}
