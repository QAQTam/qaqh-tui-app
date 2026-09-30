//! v2 权威载荷的测试构造器（`#[cfg(test)]`）。
//!
//! **为什么必须集中在这里**：信封 / 快照 / reset 此前散落在各测试文件里当
//! 结构体字面量手抄。后端每次动 wire 都会让它们以两种方式失真：
//!
//! - **新增必填字段** → 结构体字面量直接编译失败（`ts_ms` 就是这样把 TUI 的
//!   `cargo test` 打红的，2026-09-30）；
//! - **新增可选字段** → 字面量静默漏字段，测试照样绿，但界面拿不到新数据。
//!
//! 这里统一走 `serde_json::from_value::<qaqh_client 权威类型>`，于是两类漂移
//! 都被钉死：必填字段缺失 → 反序列化**当场上报**；可选字段缺失 → 走上游
//! `#[serde(default)]`，正是它声明的语义。
//!
//! 基线一律由 `qaqh-client` 自己的 `Default` / 编码器生成，用例只覆写真正关心
//! 的键——后端往快照里加字段时不用回来补 fixture。

use qaqh_client::{
    Channel, ClientV2Bootstrap, ClientV2ControlState, ClientV2ConversationDelta,
    ClientV2ConversationState, ClientV2Cursor, ClientV2CursorToken, ClientV2Delivery,
    ClientV2Event, ClientV2InteractionKind, ClientV2Payload, ClientV2Reset, ClientV2ResetReason,
    ClientV2StreamKey, ClientV2ToolState, RINGING_SCHEMA, RINGING_V2_VERSION,
};

/// 未决交互在 fixture 里的最小形状。
///
/// `kind` 用**wire 字符串**（`permission` / `ask` / `plan`）：权威 serde 负责
/// 校验它，拼错就是解析失败而不是静默落成别的变体。
pub struct PendingFixture {
    pub interaction_id: String,
    pub call_id: String,
    pub turn_id: String,
    pub kind: &'static str,
    /// `Some(text)` = inline 正文（权威 `request`）。
    pub request_text: Option<String>,
}

/// 无正文的未决交互。
pub fn pending(
    interaction_id: &str,
    call_id: &str,
    turn_id: &str,
    kind: &'static str,
) -> PendingFixture {
    PendingFixture {
        interaction_id: interaction_id.to_string(),
        call_id: call_id.to_string(),
        turn_id: turn_id.to_string(),
        kind,
        request_text: None,
    }
}

/// 带 inline 正文的未决交互。
pub fn pending_with_text(
    interaction_id: &str,
    call_id: &str,
    turn_id: &str,
    kind: &'static str,
    text: &str,
) -> PendingFixture {
    PendingFixture {
        interaction_id: interaction_id.to_string(),
        call_id: call_id.to_string(),
        turn_id: turn_id.to_string(),
        kind,
        request_text: Some(text.to_string()),
    }
}

/// driver 段 fixture。
pub struct DriverFixture {
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub can_claim: bool,
}

pub fn driver(holder: &str, driver_epoch: u64, can_claim: bool) -> DriverFixture {
    DriverFixture {
        holder: Some(holder.to_string()),
        driver_epoch,
        can_claim,
    }
}

/// **类型化**的 bootstrap 构造：调用点不必碰 `serde_json`（静态门禁 G2 要求
/// 展示层与状态机不出现 `serde_json::Value`）。
pub fn bootstrap_typed(
    session_id: &str,
    server_epoch: &str,
    cursor_log_id: &str,
    cursor_fact_seq: u64,
    state_revision: u64,
    interactions: &[PendingFixture],
    driver: Option<DriverFixture>,
) -> ClientV2Bootstrap {
    let interactions = serde_json::Value::Array(
        interactions
            .iter()
            .map(|fixture| {
                let mut row = serde_json::json!({
                    "interaction_id": fixture.interaction_id,
                    "call_id": fixture.call_id,
                    "turn_id": fixture.turn_id,
                    "kind": fixture.kind,
                });
                if let Some(text) = &fixture.request_text {
                    row["request"] = serde_json::json!({
                        "kind": "inline",
                        "data": { "text": text }
                    });
                }
                row
            })
            .collect(),
    );
    let driver = driver.map(|fixture| {
        serde_json::json!({
            "holder": fixture.holder,
            "driver_epoch": fixture.driver_epoch,
            "can_claim": fixture.can_claim,
        })
    });
    bootstrap_with(
        session_id,
        server_epoch,
        cursor_log_id,
        cursor_fact_seq,
        state_revision,
        interactions,
        driver.unwrap_or(serde_json::Value::Null),
    )
}

/// 权威 audit payload（本仓测试用的「不驱动 UI」载荷）。
pub fn audit_payload() -> ClientV2Payload {
    serde_json::from_value(serde_json::json!({
        "kind": "audit_ref",
        "data": { "audit_seq": 1, "audit_hash": "abc" }
    }))
    .expect("权威 audit payload 可反序列化")
}

/// 权威 interaction kind 的 wire 值（由上游 serde 决定，TUI 不再自己映射）。
pub fn interaction_kind_wire(kind: ClientV2InteractionKind) -> String {
    serde_json::to_value(kind)
        .expect("kind 可序列化")
        .as_str()
        .expect("kind 的 wire 形态是字符串")
        .to_string()
}

/// 权威 `ConversationDelta::TurnStarted`（校验 TUI 能直接命名与匹配该变体）。
pub fn turn_started_delta(turn_id: &str) -> ClientV2ConversationDelta {
    serde_json::from_value(serde_json::json!({
        "kind": "turn_started",
        "data": {
            "revision": 1,
            "turn_id": turn_id,
            "input_id": "in-1",
            "mode": "normal"
        }
    }))
    .expect("权威 ConversationDelta 可反序列化")
}

/// **反向闸**：仍用已退场的 legacy `seed` 键写的 reset 帧必须解析失败。
///
/// 这是「镜像时代」最典型的静默漂移形状：字段名换了，手抄件照旧。
///
/// 函数名与下面的 `"seed"` 键**故意保留 legacy 拼写**——这里测的正是「旧键名
/// 不再被接受」，把它一起改名会让这条断言失去意义。
pub fn reset_from_legacy_session_id_key() -> Result<ClientV2Reset, String> {
    serde_json::from_value(serde_json::json!({
        "schema": RINGING_SCHEMA,
        "version": RINGING_V2_VERSION,
        "server_epoch": "epoch-1",
        "seed": "session-1",
        "reason": "cursor_expired",
    }))
    .map_err(|error| error.to_string())
}

/// snapshot cursor token（`log_id` + `fact_seq` 基线）。
fn snapshot_cursor(log_id: &str, fact_seq: u64) -> ClientV2CursorToken {
    ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot(log_id, fact_seq))
        .expect("snapshot cursor 可编码")
}

/// reliable cursor token。
fn reliable_cursor(log_id: &str, fact_seq: u64, projection_index: u16) -> ClientV2CursorToken {
    ClientV2CursorToken::encode_reliable(&ClientV2Cursor::new(log_id, fact_seq, projection_index))
        .expect("reliable cursor 可编码")
}

/// 最小可用 `ClientV2Bootstrap`：三频道基线取客户端自己的 `Default`。
pub fn bootstrap(
    session_id: &str,
    server_epoch: &str,
    cursor_log_id: &str,
    cursor_fact_seq: u64,
    state_revision: u64,
) -> ClientV2Bootstrap {
    bootstrap_with(
        session_id,
        server_epoch,
        cursor_log_id,
        cursor_fact_seq,
        state_revision,
        serde_json::Value::Null,
        serde_json::Value::Null,
    )
}

/// 同上，但覆写 `interactions` / `driver` 两段（传 `Value::Null` = 保留基线）。
///
/// `interactions` 是权威 `[ClientV2PendingInteraction]` 的 JSON 形态——
/// 传 `#[serde(default)]` 允许的完整形状，不要在调用点抄字段表。
pub fn bootstrap_with(
    session_id: &str,
    server_epoch: &str,
    cursor_log_id: &str,
    cursor_fact_seq: u64,
    state_revision: u64,
    interactions: serde_json::Value,
    driver: serde_json::Value,
) -> ClientV2Bootstrap {
    let token = snapshot_cursor(cursor_log_id, cursor_fact_seq);
    let mut control =
        serde_json::to_value(ClientV2ControlState::default()).expect("control 基线可序列化");
    if !interactions.is_null() {
        control["interactions"] = interactions;
    }
    if !driver.is_null() {
        control["driver"] = driver;
    }
    let conversation = serde_json::to_value(ClientV2ConversationState::default())
        .expect("conversation 基线可序列化");
    let tool = serde_json::to_value(ClientV2ToolState::default()).expect("tool 基线可序列化");

    serde_json::from_value(serde_json::json!({
        "schema": RINGING_SCHEMA,
        "version": RINGING_V2_VERSION,
        "server_epoch": server_epoch,
        "session_id": session_id,
        "snapshot_cursor": token.as_str(),
        "control": {
            "channel": "control",
            "state_revision": state_revision,
            "snapshot_version": 1,
            "state": control
        },
        "conversation": {
            "channel": "conversation",
            "state_revision": state_revision,
            "snapshot_version": 1,
            "state": conversation
        },
        "tool": {
            "channel": "tool",
            "state_revision": state_revision,
            "snapshot_version": 1,
            "state": tool
        }
    }))
    .expect("权威 ClientV2Bootstrap 可反序列化")
}

/// reliable 事件的信封形态（cursor / log_id / fact_seq / projection_index 自洽）。
pub struct ReliableEvent {
    pub session_id: String,
    pub server_epoch: String,
    pub log_id: String,
    pub fact_seq: u64,
    pub projection_index: u16,
    pub revision: u64,
    pub ts_ms: Option<u64>,
    pub payload: ClientV2Payload,
}

impl ReliableEvent {
    pub fn new(
        session_id: &str,
        server_epoch: &str,
        log_id: &str,
        fact_seq: u64,
        projection_index: u16,
        payload: ClientV2Payload,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            server_epoch: server_epoch.to_string(),
            log_id: log_id.to_string(),
            fact_seq,
            projection_index,
            revision: fact_seq,
            ts_ms: None,
            payload,
        }
    }

    pub fn build(self) -> ClientV2Event {
        event_from_json(serde_json::json!({
            "schema": RINGING_SCHEMA,
            "version": RINGING_V2_VERSION,
            "server_epoch": self.server_epoch,
            "session_id": self.session_id,
            "event_id": format!("ev-{}-{}", self.fact_seq, self.projection_index),
            "stream_key": stream_key_json(Channel::Conversation),
            "delivery": "reliable",
            "cursor": reliable_cursor(&self.log_id, self.fact_seq, self.projection_index).as_str(),
            "log_id": self.log_id,
            "fact_seq": self.fact_seq,
            "projection_index": self.projection_index,
            "revision": self.revision,
            "ts_ms": self.ts_ms,
            "payload": serde_json::to_value(self.payload).expect("payload 可序列化"),
        }))
    }
}

/// ephemeral 事件（无 cursor / log_id / fact_seq / revision，协议强制）。
pub fn ephemeral_event(
    session_id: &str,
    server_epoch: &str,
    payload: ClientV2Payload,
) -> ClientV2Event {
    event_from_json(serde_json::json!({
        "schema": RINGING_SCHEMA,
        "version": RINGING_V2_VERSION,
        "server_epoch": server_epoch,
        "session_id": session_id,
        "event_id": "ev-ephemeral",
        "stream_key": stream_key_json(Channel::Conversation),
        "delivery": "ephemeral",
        "payload": serde_json::to_value(payload).expect("payload 可序列化"),
    }))
}

/// replaceable 事件（有 revision，无 cursor）。
pub fn replaceable_event(
    session_id: &str,
    server_epoch: &str,
    revision: u64,
    payload: ClientV2Payload,
) -> ClientV2Event {
    event_from_json(serde_json::json!({
        "schema": RINGING_SCHEMA,
        "version": RINGING_V2_VERSION,
        "server_epoch": server_epoch,
        "session_id": session_id,
        "event_id": format!("ev-replaceable-{revision}"),
        "stream_key": stream_key_json(Channel::Control),
        "delivery": "replaceable",
        "revision": revision,
        "payload": serde_json::to_value(payload).expect("payload 可序列化"),
    }))
}

/// reset 帧。
pub fn reset(
    session_id: &str,
    server_epoch: &str,
    log_id: Option<&str>,
    snapshot_cursor_at: Option<(&str, u64)>,
    reason: ClientV2ResetReason,
) -> ClientV2Reset {
    let cursor = snapshot_cursor_at.map(|(log_id, fact_seq)| snapshot_cursor(log_id, fact_seq));
    serde_json::from_value(serde_json::json!({
        "schema": RINGING_SCHEMA,
        "version": RINGING_V2_VERSION,
        "server_epoch": server_epoch,
        "session_id": session_id,
        "log_id": log_id,
        "snapshot_cursor": cursor.as_ref().map(ClientV2CursorToken::as_str),
        "reason": reason,
    }))
    .expect("权威 ClientV2Reset 可反序列化")
}

/// `RingingV2StreamKey::Channel` 的 wire 形态。
///
/// 判别式（`kind` / `data`）由上游 serde 决定，这里只暴露给需要自建事件的用例。
pub fn stream_key_json(channel: Channel) -> serde_json::Value {
    serde_json::json!({ "kind": "channel", "data": channel })
}

/// 事件必须自洽（reliable 缺 cursor 会被 `validate()` 拒），所以统一走
/// 反序列化而不是结构体字面量：**上游加字段时这里不会漏**。
fn event_from_json(value: serde_json::Value) -> ClientV2Event {
    let event: ClientV2Event =
        serde_json::from_value(value).expect("权威 ClientV2Event 可反序列化");
    event.validate().expect("fixture 事件必须通过权威校验");
    event
}

/// delivery 的权威取值（用例断言用，避免在测试里重复协议常量）。
pub fn delivery_of(event: &ClientV2Event) -> ClientV2Delivery {
    event.delivery
}

/// 让 `ClientV2StreamKey` 的构造也经过权威 serde（防止用例手写判别式漂移）。
pub fn stream_key(channel: Channel) -> ClientV2StreamKey {
    serde_json::from_value(stream_key_json(channel)).expect("权威 stream_key 可反序列化")
}
