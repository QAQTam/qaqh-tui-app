//! 客户端 open 握手 payload（镜像 `qaqh-ringing/src/capability.rs`）。

use serde::{Deserialize, Serialize};

use super::{RINGING_SCHEMA, RINGING_VERSION};

/// `POST /ringing/v1/clients/open` 请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientOpenRequest {
    pub schema: String,
    pub version: u32,
    /// 客户端实例 id（lease 绑定该身份；同实例重新 open 会替换租约）。
    pub client_instance_id: String,
}

impl ClientOpenRequest {
    pub fn new(client_instance_id: impl Into<String>) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            client_instance_id: client_instance_id.into(),
        }
    }
}

/// open 成功响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientOpenResponse {
    pub schema: String,
    pub version: u32,
    pub accepted: bool,
    /// 服务端签发的连接级身份（后续所有请求与 SSE 必须携带）。
    pub client_session_id: String,
    /// 服务端 epoch（SSE stream_seq 基准；daemon 进程生命周期内不变）。
    pub server_epoch: String,
    /// lease TTL（毫秒）。
    pub lease_ttl_ms: u64,
    /// 建议续租间隔（毫秒）。
    pub renew_interval_ms: u64,
}

/// `POST /ringing/v1/leases/renew` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRenewResponse {
    pub ok: bool,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}
