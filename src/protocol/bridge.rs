//! 权威类型 ↔ 本仓镜像类型的**临时**转换缝（T-01 阶段一专用）。
//!
//! 阶段一之后，连接生命周期与全部流由 `qaqh-client` 承担，回调交回的是
//! `qaqh-domain`/`qaqh-ringing` 的**权威**类型；而 app 层仍在消费 `protocol/`
//! 的手工镜像（2419 行）。两者 serde 表示逐字相同（同一 wire 形状，见
//! buglist §4「协议镜像字段正确性」），故用 serde 往返搭一座桥，让阶段一
//! **不必**同时改动 app 层的数百个引用点。
//!
//! **阶段二删除本模块**：届时 `protocol/` 整体消失，app 层直接引用权威类型。
//! 在那之前，这里是唯一允许出现两侧类型名的地方——不要在别处再写转换。
//!
//! 转换**绝不允许静默降级**：任何失败都必须以 `Err` 返回并由调用方上报
//! （丢弃一个事件而不说，正是本仓反复吃亏的那类缺陷）。

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 本仓镜像类型 ← 权威类型。
pub fn to_mirror<W: Serialize, M: DeserializeOwned>(wire: &W) -> Result<M, serde_json::Error> {
    serde_json::from_value(serde_json::to_value(wire)?)
}

/// 权威类型 ← 本仓镜像类型。
pub fn to_wire<M: Serialize, W: DeserializeOwned>(mirror: &M) -> Result<W, serde_json::Error> {
    serde_json::from_value(serde_json::to_value(mirror)?)
}

// ───────────────────────── 具名别名（调用点可读性） ─────────────────────────

pub fn envelope_from_wire(
    wire: &qaqh_client::RingingEventEnvelope,
) -> Result<crate::protocol::envelope::RingingEventEnvelope, serde_json::Error> {
    to_mirror(wire)
}

pub fn command_to_wire(
    mirror: &crate::protocol::command::RingingCommand,
) -> Result<qaqh_client::RingingCommand, serde_json::Error> {
    to_wire(mirror)
}

pub fn ack_from_wire(
    wire: &qaqh_client::RingingCommandAck,
) -> Result<crate::protocol::envelope::RingingCommandAck, serde_json::Error> {
    to_mirror(wire)
}

pub fn command_status_from_wire(
    wire: &qaqh_client::RingingCommandStatus,
) -> Result<crate::protocol::envelope::RingingCommandStatus, serde_json::Error> {
    to_mirror(wire)
}

pub fn content_ref_from_wire(
    wire: &qaqh_client::ContentRef,
) -> Result<crate::protocol::event::ContentRef, serde_json::Error> {
    to_mirror(wire)
}

/// 频道枚举（两侧同名同形，仍走一次显式映射，避免将来加变体时静默错配）。
pub fn channel_from_wire(wire: qaqh_client::Channel) -> crate::protocol::Channel {
    match wire {
        qaqh_client::Channel::Control => crate::protocol::Channel::Control,
        qaqh_client::Channel::Conversation => crate::protocol::Channel::Conversation,
        qaqh_client::Channel::Tool => crate::protocol::Channel::Tool,
    }
}

pub fn reset_from_wire(
    wire: &qaqh_client::ResetRequired,
) -> Result<crate::protocol::envelope::RingingResetRequired, serde_json::Error> {
    to_mirror(wire)
}

/// bootstrap 走泛型：`qaqh_ringing::RingingSessionBootstrap` 没有被
/// `qaqh-client` 再导出，为它单独加一条 `qaqh-ringing` 依赖不值得——按
/// `Serialize` 泛型转换即可，不必在本仓写出那个类型名。
pub fn bootstrap_from_wire<W: Serialize>(
    wire: &W,
) -> Result<crate::protocol::snapshot::RingingSessionBootstrap, serde_json::Error> {
    to_mirror(wire)
}

#[cfg(test)]
mod tests {
    //! 桥的**形状保真**回归。
    //!
    //! 这些断言锁的是「镜像与权威类型 serde 表示一致」这一前提：一旦后端改
    //! 了字段名/形状而本仓镜像没跟，阶段一期间会表现为**事件被丢弃**（而不是
    //! 编译错误）。在这里红，比在运行时静默丢帧好。

    use super::*;
    use serde::Deserialize;

    fn canonical_envelope() -> qaqh_client::RingingEventEnvelope {
        use qaqh_client::ConversationEvent;
        qaqh_client::RingingEventEnvelope::new(
            "seed-1",
            7,
            3,
            3,
            "event-1",
            qaqh_client::RingingEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "你好".into(),
            }),
        )
    }

    /// 带中文的事件信封必须逐字段过桥（含 `event` 的 `channel` tag）。
    #[test]
    fn envelope_round_trips_field_for_field() {
        let wire = canonical_envelope();
        let mirror = envelope_from_wire(&wire).expect("bridge must succeed");

        assert_eq!(mirror.seed, "seed-1");
        assert_eq!(mirror.stream_seq, 7);
        assert_eq!(mirror.channel_seq, 3);
        assert_eq!(mirror.event_id, "event-1");
        assert_eq!(mirror.channel(), crate::protocol::Channel::Conversation);
        match mirror.event {
            crate::protocol::event::RingingEvent::Conversation(
                crate::protocol::event::ConversationEvent::TurnStarted { turn_id, user_text },
            ) => {
                assert_eq!(turn_id, "t1");
                assert_eq!(user_text, "你好", "多字节文本必须原样过桥");
            }
            other => panic!("事件变体过桥后走样：{other:?}"),
        }
    }

    /// 命令走的是**反向**（镜像 → 权威），同样逐字段核对。
    #[test]
    fn command_round_trips_and_keeps_channel_tag() {
        let mirror = crate::protocol::command::RingingCommand::Control(
            crate::protocol::command::ControlCommand::SessionResume {
                seed: "seed-9".into(),
            },
        );
        let wire = command_to_wire(&mirror).expect("bridge must succeed");
        assert_eq!(wire.channel(), qaqh_client::Channel::Control);
        match wire {
            qaqh_client::RingingCommand::Control(qaqh_client::ControlCommand::SessionResume {
                seed,
            }) => assert_eq!(seed, "seed-9"),
            other => panic!("命令变体过桥后走样：{other:?}"),
        }
    }

    /// 上传返回的 `ContentRef` 必须逐字段过桥（附件随后随命令回传）。
    #[test]
    fn content_ref_round_trips_from_wire() {
        let wire = qaqh_client::ContentRef {
            content_id: "c-1".into(),
            media_type: "image/png".into(),
            sha256: "ab".repeat(32),
            truncated: false,
        };
        let mirror = content_ref_from_wire(&wire).expect("from wire");
        assert_eq!(mirror.content_id, "c-1");
        assert_eq!(mirror.media_type, "image/png");
        assert_eq!(mirror.sha256, "ab".repeat(32));
        assert!(!mirror.truncated);
    }

    /// **失败不得静默**：后端新增了本仓镜像还不认识的变体时，桥必须返回
    /// `Err` 交给调用方上报——绝不退化成默认值或空事件（那会让「后端发了什么」
    /// 变成不可见，正是本仓反复吃亏的一类缺陷）。
    ///
    /// 用本地类型对构造该场景：真实的「权威有、镜像无」只会在后端领先本仓时
    /// 出现，单次构建里造不出来。
    #[test]
    fn unknown_wire_variant_is_an_error_not_a_default() {
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        enum NewerWire {
            Known,
            AddedLater,
        }
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum StaleMirror {
            Known,
        }

        assert!(to_mirror::<_, StaleMirror>(&NewerWire::Known).is_ok());
        let err = to_mirror::<_, StaleMirror>(&NewerWire::AddedLater)
            .expect_err("镜像缺少的变体必须报错，而不是落成默认值");
        assert!(
            err.to_string().contains("added_later"),
            "错误必须点名是哪个变体，否则无法定位漂移：{err}"
        );
    }
}
