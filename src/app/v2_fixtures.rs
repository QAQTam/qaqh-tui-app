//! v3 权威载荷的测试构造器（`#[cfg(test)]`）。
//!
//! 所有 fixture 都**从权威契约类型出发**（`qaqh-ringing-contract`），经 serde
//! 往返锁定 wire 形状：契约加必填字段，这些构造器在测试编译期当场红。绝不
//! 手抄 JSON（历史上手抄件按 legacy 键写快照，正是静默漂移的源头）。
//!
//! backend-v3 的词表（`SessionState` / `SessionEventFrame`）没有 delivery /
//! cursor / revision 信封——单流单游标，水位校验在 `ringing_v2` 的测试里做。

use qaqh_ringing::SESSION_CONTRACT_REVISION;
use qaqh_ringing_contract::fact::{ContentValue, InteractionId, InteractionKind, TurnId};
use qaqh_ringing_contract::state::{DriverState, InteractionView};
use qaqh_ringing_contract::{SessionBootstrap, SessionEvent, SessionEventFrame, SessionState};

const SCHEMA: &str = "qaqh.ringing.session/v2";

/// 空 SessionState 的最小 bootstrap（水位 `seq`）。
pub fn bootstrap(session_id: &str, server_epoch: &str, seq: u64) -> SessionBootstrap {
    bootstrap_with_state(
        session_id,
        server_epoch,
        SessionState {
            seq,
            ..SessionState::default()
        },
    )
}

/// 带 pending 交互的 bootstrap（快照恢复路径回归用）。
pub fn bootstrap_with_interactions(
    session_id: &str,
    server_epoch: &str,
    seq: u64,
    interactions: Vec<InteractionView>,
) -> SessionBootstrap {
    bootstrap_with_state(
        session_id,
        server_epoch,
        SessionState {
            seq,
            interactions,
            ..SessionState::default()
        },
    )
}

/// 带 driver 席位的 bootstrap。
pub fn bootstrap_with_driver(
    session_id: &str,
    server_epoch: &str,
    seq: u64,
    driver: Option<DriverState>,
) -> SessionBootstrap {
    bootstrap_with_state(
        session_id,
        server_epoch,
        SessionState {
            seq,
            driver,
            ..SessionState::default()
        },
    )
}

/// 全字段自由构造（serde 往返锁定 wire 形状）。
pub fn bootstrap_with_state(
    session_id: &str,
    server_epoch: &str,
    state: SessionState,
) -> SessionBootstrap {
    let bootstrap = SessionBootstrap {
        schema: SCHEMA.to_owned(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        session_contract_revision: SESSION_CONTRACT_REVISION.to_owned(),
        server_epoch: server_epoch.to_owned(),
        session_id: session_id.to_owned(),
        state,
    };
    // wire 往返：契约字段增删在此当场红。
    let value = serde_json::to_value(&bootstrap).expect("bootstrap serializes");
    serde_json::from_value(value).expect("bootstrap round-trips")
}

/// 一条事件帧（任意 seq；水位校验在 reducer 测试里做）。
pub fn frame(seq: u64, event: SessionEvent) -> SessionEventFrame {
    SessionEventFrame { seq, event }
}

/// ask 交互视图（正文 = ask 工具 canonical args JSON，v3 形状）。
pub fn ask_interaction(
    interaction_id: &str,
    call_id: &str,
    turn_id: &str,
    questions: serde_json::Value,
) -> InteractionView {
    let body = serde_json::json!({
        "input": format!("ASK v1\n{}", ask_v1_lines(&questions)),
    });
    InteractionView {
        interaction_id: InteractionId::new(interaction_id.to_owned()),
        call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new(
            call_id.to_owned(),
        )),
        turn_id: Some(TurnId::new(turn_id.to_owned())),
        kind: InteractionKind::Ask,
        request: ContentValue::Inline {
            text: body.to_string(),
        },
        choices: vec![],
        expires_at_ms: None,
        resolution: None,
    }
}

/// permission 交互视图（正文 = `PermissionRequestDetails` JSON）。
pub fn permission_interaction(interaction_id: &str, call_id: &str, turn_id: &str) -> InteractionView {
    let body = serde_json::json!({
        "tool_name": "exec",
        "action_summary": "command: cargo test",
        "reason": "执行测试",
        "paths": [],
        "category": "exec",
        "level": 1,
        "risk": "medium",
        "consequence": "运行本地测试",
    });
    InteractionView {
        interaction_id: InteractionId::new(interaction_id.to_owned()),
        call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new(
            call_id.to_owned(),
        )),
        turn_id: Some(TurnId::new(turn_id.to_owned())),
        kind: InteractionKind::Permission,
        request: ContentValue::Inline {
            text: body.to_string(),
        },
        choices: vec![],
        expires_at_ms: None,
        resolution: None,
    }
}

/// 把 `{questions: [{question, options, allow_custom}]}` 渲染成 ASK v1 行。
fn ask_v1_lines(questions: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(items) = questions.get("questions").and_then(|q| q.as_array()) else {
        return out;
    };
    for (index, q) in items.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "Question: {}\n",
            q.get("question").and_then(|v| v.as_str()).unwrap_or("")
        ));
        if let Some(options) = q.get("options").and_then(|v| v.as_array())
            && !options.is_empty()
        {
            out.push_str("Options:\n");
            for option in options {
                if let Some(text) = option.as_str() {
                    out.push_str(&format!("- {text}\n"));
                }
            }
        }
        let allow = q
            .get("allow_custom")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        out.push_str(if allow {
            "Allow custom: yes\n"
        } else {
            "Allow custom: no\n"
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture 走权威 serde 往返：契约加必填字段这里当场红。
    #[test]
    fn bootstrap_round_trips_the_wire_shape() {
        let bootstrap = bootstrap("s1", "epoch-1", 7);
        assert_eq!(bootstrap.state.seq, 7);
        assert_eq!(bootstrap.session_id, "s1");
        assert_eq!(bootstrap.session_contract_revision, SESSION_CONTRACT_REVISION);
    }

    #[test]
    fn ask_interaction_body_is_the_canonical_args_shape() {
        let interaction = ask_interaction(
            "int-1",
            "call-1",
            "turn-1",
            serde_json::json!({"questions": [
                {"question": "选哪个？", "options": ["甲", "乙"], "allow_custom": false}
            ]}),
        );
        let ContentValue::Inline { text } = &interaction.request else {
            panic!("inline body");
        };
        let parsed: serde_json::Value = serde_json::from_str(text).expect("json body");
        let prose = parsed["input"].as_str().expect("ASK v1 prose");
        assert!(prose.starts_with("ASK v1"), "{prose}");
        assert!(prose.contains("Question: 选哪个？"));
        assert!(prose.contains("- 甲"));
        assert!(prose.contains("Allow custom: no"));
    }
}
