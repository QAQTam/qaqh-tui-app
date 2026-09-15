//! 协议类型镜像（PLAN.md §4 类型镜像纪律）。
//!
//! 手工镜像 `QAQ-Harness/crates/qaqh-ringing` 与 `qaqh-domain` 的 wire 形状。
//! **改动须对照后端 PR**；镜像以 `F:\QAQ-Harness` 2026-08 协议瘦身后的最终形态
//! 为基准（协议版本 1）。禁止在本仓散落协议字面量，全部 import 自本模块。
//!
//! 对应关系：
//! - `qaqh-ringing/src/protocol.rs` → [`mod.rs`] 常量
//! - `qaqh-ringing/src/envelope.rs` → [`envelope`]
//! - `qaqh-domain/src/command.rs` → [`command`]
//! - `qaqh-domain/src/event.rs` → [`event`]
//! - `qaqh-ringing/src/{snapshot,reset}.rs` → [`snapshot`]
//! - `qaqh-runtime/src/ringing/service_methods.rs` → [`methods`]

pub mod config;
pub mod methods;
pub mod snapshot;

use serde::{Deserialize, Serialize};

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

    /// SSE 帧 id 频道段：`<epoch>:<channel>:<seq>`。
    #[allow(dead_code)]
    pub fn from_path_segment(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
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
            assert_eq!(
                c.as_str(),
                serde_json::to_string(&c).unwrap().trim_matches('"')
            );
            assert_eq!(Channel::from_path_segment(c.as_str()), Some(c));
        }
        assert_eq!(Channel::from_path_segment("bogus"), None);
    }
}
