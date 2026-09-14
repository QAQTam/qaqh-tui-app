//! HTTP 客户端：双头注入、错误分类、各端点封装（对照 `qaqh-client/src/client.rs`）。

use std::sync::RwLock;
use std::time::Duration;

use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::protocol::capability::{ClientOpenRequest, ClientOpenResponse, LeaseRenewResponse};
use crate::protocol::command::RingingCommand;
use crate::protocol::envelope::{
    RingingCommandAck, RingingCommandEnvelope, RingingCommandStatus, RingingResetRequired,
};
use crate::protocol::event::ContentRef;
use crate::protocol::methods::SessionMetaView;
use crate::protocol::snapshot::RingingSessionBootstrap;
use crate::protocol::timeline::TimelinePage;
use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION, SESSION_ID_HEADER, WireError};

pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// SSE 空闲判活阈值：server 每 15s 发注释行；45s 无**字节**即判死。
pub const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// daemon 对「Bearer token 被拒」返回的 plain body（`auth.rs:12`）。
/// 与 renew 的 `lease expired or unknown` 同为 plain 401，只能按内容区分。
const PLAIN_UNAUTHORIZED: &str = "unauthorized";

#[derive(Debug, Error)]
pub enum ApiError {
    /// Bearer token 被拒（plain 401 `unauthorized`）。
    /// **不致命**：daemon 重启会换 token，调用方应重读 discovery 自愈。
    #[error("token 被拒绝（unauthorized）")]
    Unauthorized,
    /// lease 缺失/过期/seed 未 attach（JSON 401 `lease_required`）→ 需重新 open+attach。
    #[error("lease 失效：{0}")]
    LeaseRequired(String),
    /// 426 unsupported_version → 提示需更新，停止重试。
    #[error("协议版本不被接受：{0}")]
    UnsupportedVersion(String),
    #[error("服务错误 {status} {code}: {message}")]
    Http {
        status: u16,
        code: String,
        message: String,
    },
    #[error("网络错误：{0}")]
    Network(String),
    #[error("协议错误：{0}")]
    Protocol(String),
}

impl ApiError {
    /// 是否**不可恢复**（继续重试无意义，应停止连接生命周期）。
    ///
    /// 仅协议代差。**token 被拒不算致命**：daemon 重启会换 token，调用方应
    /// 重读 `daemon.json` 原地换值后重新协商（见 `apply_discovery`）。
    pub fn is_fatal(&self) -> bool {
        matches!(self, ApiError::UnsupportedVersion(_))
    }

    /// 是否「凭据/租约」类失败——值得先重读 discovery 再退避。
    pub fn is_credential(&self) -> bool {
        matches!(self, ApiError::Unauthorized | ApiError::LeaseRequired(_))
    }
}

pub struct HttpClient {
    http: reqwest::Client,
    /// 可热更新：daemon 重启会换端口/token，靠 `apply_discovery` 原地换值
    /// （BUG-2026-09-14-01：旧凭据只会永久 401，无法自愈）。
    base_url: RwLock<String>,
    token: RwLock<String>,
    /// 客户端实例 id：open 前 require 生成，lease 绑定该身份。
    pub instance_id: String,
    /// open 成功后的连接级身份；open 更新后所有流自动携带新值。
    session_id: RwLock<String>,
    /// 串行化 open（避免并发重复协商）。
    open_lock: tokio::sync::Mutex<()>,
}

impl HttpClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>, instance_id: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: RwLock::new(base_url.into().trim_end_matches('/').to_string()),
            token: RwLock::new(token.into()),
            instance_id,
            session_id: RwLock::new(String::new()),
            open_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> String {
        self.base_url.read().expect("base_url lock").clone()
    }

    /// 重读 daemon 发现记录后的原地换值（daemon 重启换 token/端口）。
    /// 返回是否发生变化；变化后所有请求与 SSE 自动携带新值。
    pub fn apply_discovery(&self, base_url: &str, token: &str) -> bool {
        let base = base_url.trim_end_matches('/').to_string();
        let mut changed = false;
        {
            let mut current = self.base_url.write().expect("base_url lock");
            if *current != base {
                *current = base;
                changed = true;
            }
        }
        {
            let mut current = self.token.write().expect("token lock");
            if *current != token {
                *current = token.to_string();
                changed = true;
            }
        }
        changed
    }

    pub fn session_id(&self) -> String {
        self.session_id.read().expect("session lock").clone()
    }

    fn set_session_id(&self, id: String) {
        *self.session_id.write().expect("session lock") = id;
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.read().expect("base_url lock"), path)
    }

    /// 当前 Bearer token（拷贝出锁，避免 `RwLockReadGuard` 跨 `await`
    /// 使 future 变成 `!Send`）。
    fn token(&self) -> String {
        self.token.read().expect("token lock").clone()
    }

    /// 双头请求构造器（open 之外的一切请求）。
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(self.token())
            .header(SESSION_ID_HEADER, self.session_id())
    }

    /// 解析 daemon 的 JSON 错误体（401 可能是 plain `unauthorized`）。
    async fn classify<T>(
        &self,
        status: reqwest::StatusCode,
        body: String,
        parse_ok: impl FnOnce(String) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        if status.is_success() {
            return parse_ok(body);
        }
        let text = body;
        // 401 有三种来源，其中两种的 body 都是 **plain text**——只能按内容区分：
        // - JSON `{"code":"lease_required",...}`：租约缺失/过期/seed 未 attach；
        // - plain `unauthorized`：Bearer token 被拒（daemon `auth.rs:12`）；
        // - plain `lease expired or unknown`：renew 时租约已死（`command.rs:137`）。
        // BUG-2026-09-14-01：第三种此前落入 `Unauthorized` 分支，被调用方当作
        // 「token 错、不可恢复」而**终止连接生命周期**——实际它正是「重新 open
        // 换新租约」就能恢复的情形，误判后客户端永久 401 无法回连。
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(err) = serde_json::from_str::<WireError>(&text) {
                return Err(ApiError::LeaseRequired(err.message));
            }
            if text.trim() == PLAIN_UNAUTHORIZED {
                return Err(ApiError::Unauthorized);
            }
            return Err(ApiError::LeaseRequired(truncate(text.trim(), 200)));
        }
        if status == reqwest::StatusCode::UPGRADE_REQUIRED {
            // 426：body 是 RingingCommandAck{code:"unsupported_version"}。
            let message = serde_json::from_str::<RingingCommandAck>(&text)
                .map(|ack| ack.message.unwrap_or_else(|| "unsupported version".into()))
                .unwrap_or_else(|_| text.clone());
            return Err(ApiError::UnsupportedVersion(message));
        }
        let (code, message) = match serde_json::from_str::<WireError>(&text) {
            Ok(err) => (err.code, err.message),
            Err(_) => (format!("http_{}", status.as_u16()), truncate(&text, 200)),
        };
        Err(ApiError::Http {
            status: status.as_u16(),
            code,
            message,
        })
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        rb: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        let resp = rb
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        self.classify(status, body, |text| {
            serde_json::from_str(&text)
                .map_err(|e| ApiError::Protocol(format!("响应解析失败: {e}")))
        })
        .await
    }

    /// open 握手（仅 Bearer；成功后记录 client_session_id）。
    pub async fn open(&self) -> Result<ClientOpenResponse, ApiError> {
        let _guard = self.open_lock.lock().await;
        let req = ClientOpenRequest::new(self.instance_id.clone());
        let resp = self
            .http
            .post(self.url("/ringing/v1/clients/open"))
            .bearer_auth(self.token())
            .json(&req)
            .timeout(OPEN_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let open: ClientOpenResponse = self
            .classify(status, body, |text| {
                serde_json::from_str(&text)
                    .map_err(|e| ApiError::Protocol(format!("open 响应解析失败: {e}")))
            })
            .await?;
        if !open.accepted
            || open.schema != RINGING_SCHEMA
            || open.version != RINGING_VERSION
            || open.client_session_id.is_empty()
        {
            return Err(ApiError::Protocol("open 响应不完整".into()));
        }
        self.set_session_id(open.client_session_id.clone());
        Ok(open)
    }

    /// lease 续期（双头，空 body）。
    pub async fn renew(&self) -> Result<LeaseRenewResponse, ApiError> {
        self.send_json(
            self.request(reqwest::Method::POST, "/ringing/v1/leases/renew"),
            OPEN_TIMEOUT,
        )
        .await
    }

    pub async fn health(&self) -> Result<String, ApiError> {
        let rb = self.http.get(self.url("/health"));
        let resp = rb
            .timeout(OPEN_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        self.classify(status, body, Ok).await
    }

    /// 发送命令信封。command_id 幂等：accepted 前可安全重试。
    pub async fn command(
        &self,
        envelope: &RingingCommandEnvelope,
    ) -> Result<RingingCommandAck, ApiError> {
        envelope
            .validate()
            .map_err(|code| ApiError::Protocol(code.to_string()))?;
        let path = format!("/ringing/v1/commands/{}", envelope.channel.as_str());
        self.send_json(
            self.request(reqwest::Method::POST, &path).json(envelope),
            COMMAND_TIMEOUT,
        )
        .await
    }

    /// 命令 receipt（ack 丢失或需要终态确认时使用）。
    pub async fn command_status(&self, command_id: &str) -> Result<RingingCommandStatus, ApiError> {
        let path = format!("/ringing/v1/commands/{command_id}");
        self.send_json(self.request(reqwest::Method::GET, &path), COMMAND_TIMEOUT)
            .await
    }

    /// 服务面 RPC（方法名必须来自 protocol::methods 常量）。
    pub async fn service(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let path = format!("/ringing/v1/service/{method}");
        let rb = self.request(reqwest::Method::POST, &path).json(params);
        let resp = rb
            .timeout(SERVICE_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        self.classify(status, body, |text| {
            serde_json::from_str(&text)
                .map_err(|e| ApiError::Protocol(format!("service {method} 响应解析失败: {e}")))
        })
        .await
    }

    pub async fn session_list(&self) -> Result<Vec<SessionMetaView>, ApiError> {
        let value = self
            .service(
                crate::protocol::methods::SESSION_LIST,
                &serde_json::json!({}),
            )
            .await?;
        let arr = value
            .as_array()
            .ok_or_else(|| ApiError::Protocol("session.list 应返回数组".into()))?;
        Ok(arr.iter().filter_map(SessionMetaView::parse).collect())
    }

    /// bootstrap：三频道快照原子恢复。
    pub async fn bootstrap(&self, seed: &str) -> Result<RingingSessionBootstrap, ApiError> {
        let path = format!("/ringing/v1/sessions/{seed}/bootstrap");
        self.send_json(self.request(reqwest::Method::GET, &path), SNAPSHOT_TIMEOUT)
            .await
    }

    /// timeline 快照分页：无 before_turn = 尾窗（默认 30，最大 200）。
    pub async fn timeline_page(
        &self,
        seed: &str,
        before_turn: Option<&str>,
        limit: u32,
    ) -> Result<TimelinePage, ApiError> {
        let mut path = format!("/ringing/v1/sessions/{seed}/timeline?limit={limit}");
        if let Some(turn) = before_turn {
            path.push_str("&before_turn=");
            path.push_str(&urlencode(turn));
        }
        self.send_json(self.request(reqwest::Method::GET, &path), SNAPSHOT_TIMEOUT)
            .await
    }

    /// SSE 连接（无整体超时；调用方负责逐字节判活）。
    pub async fn sse_connect(
        &self,
        path: &str,
        last_event_id: Option<String>,
    ) -> Result<reqwest::Response, ApiError> {
        let rb = self
            .request(reqwest::Method::GET, path)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let rb = match last_event_id {
            Some(id) => rb.header(crate::protocol::LAST_EVENT_ID_HEADER, id),
            None => rb,
        };
        let resp = rb
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return self
                .classify(status, body, |text| {
                    Err(ApiError::Protocol(truncate(&text, 200)))
                })
                .await;
        }
        Ok(resp)
    }

    /// timeline SSE 恢复指令的解析辅助（`event: ringing.reset_required` 也可能
    /// 出现在 timeline 流上，data 形状相同）。
    pub fn parse_reset(data: &str) -> Option<RingingResetRequired> {
        serde_json::from_str(data.trim()).ok()
    }

    /// 附件上传：手写 multipart（daemon 受限解析器只收 seed/media_type/content
    /// 三个字段；与 `qaqh-client/src/client.rs:619-661` 对齐）。响应即 ContentRef。
    pub async fn upload_content(
        &self,
        seed: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<ContentRef, ApiError> {
        let boundary = format!("qaqh-{}", uuid::Uuid::new_v4());
        let mut body: Vec<u8> = Vec::with_capacity(bytes.len() + 256);
        let push_field = |name: &str, value: &[u8], body: &mut Vec<u8>| {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        };
        push_field("seed", seed.as_bytes(), &mut body);
        push_field("media_type", media_type.as_bytes(), &mut body);
        push_field("content", &bytes, &mut body);
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let rb = self
            .request(reqwest::Method::POST, "/ringing/v1/content")
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        self.send_json(rb, UPLOAD_TIMEOUT).await
    }

    /// 内容下载（校验 sha256 与引用一致）。
    #[allow(dead_code)]
    pub async fn download_content(
        &self,
        content: &ContentRef,
        seed: &str,
    ) -> Result<Vec<u8>, ApiError> {
        let path = format!(
            "/ringing/v1/content/{}?seed={}",
            content.content_id,
            urlencode(seed)
        );
        let rb = self.request(reqwest::Method::GET, &path);
        let resp = rb
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return self
                .classify(status, body, |text| {
                    Err(ApiError::Protocol(truncate(&text, 200)))
                })
                .await;
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let digest = sha256_hex(&bytes);
        if digest != content.sha256 {
            return Err(ApiError::Protocol(format!(
                "内容 sha256 不匹配（期望 {}，实际 {digest}）",
                content.sha256
            )));
        }
        Ok(bytes.to_vec())
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 最小 URL 编码（query 值场景足够）。
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// 新建客户端实例 id（uuid v4）。
pub fn new_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 组装命令信封（uuid v4 command_id；seed/期望修订由调用方注入）。
pub fn build_envelope(client: &HttpClient, command: RingingCommand) -> RingingCommandEnvelope {
    RingingCommandEnvelope::new(
        uuid::Uuid::new_v4().to_string(),
        client.instance_id.clone(),
        command,
    )
    .with_client_session_id(client.session_id())
}

#[cfg(test)]
mod tests {
    //! 401 三态分类回归（BUG-2026-09-14-01）。
    //!
    //! daemon 对三种失败都回 401，其中两种 body 是 **plain text**：
    //!
    //! - `unauthorized`（token 错，`auth.rs:12`）
    //! - `lease expired or unknown`（renew 时租约已死，`command.rs:137`）
    //!
    //! 旧实现把两种都归为 `Unauthorized`，而调用方把 `Unauthorized` 当致命
    //! 错误终止连接生命周期——于是「租约过期」被误当成「token 错」，客户端
    //! 永久 401 无法回连。分类必须按 body 内容区分。

    use super::*;

    fn classify(status: u16, body: &str) -> ApiError {
        let client = HttpClient::new("http://127.0.0.1:1", "t", "ci".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(client.classify(
            reqwest::StatusCode::from_u16(status).expect("status"),
            body.to_string(),
            Ok::<_, ApiError>,
        ))
        .expect_err("non-2xx must error")
    }

    /// renew 的 plain 401（租约已死）→ `LeaseRequired`（可自愈），**不是** `Unauthorized`。
    #[test]
    fn plain_lease_expired_is_lease_required_not_unauthorized() {
        let err = classify(401, "lease expired or unknown");
        assert!(
            err.is_credential(),
            "租约过期属凭据类，应触发重新协商：{err:?}"
        );
        assert!(
            !err.is_fatal(),
            "租约过期绝不可判为致命（旧 bug 的根因）：{err:?}"
        );
        assert!(matches!(err, ApiError::LeaseRequired(_)), "{err:?}");
    }

    /// token 被拒的 plain 401 → `Unauthorized`，且**同样不致命**（daemon 重启换 token）。
    #[test]
    fn plain_unauthorized_is_not_fatal() {
        let err = classify(401, "unauthorized");
        assert!(matches!(err, ApiError::Unauthorized), "{err:?}");
        assert!(
            !err.is_fatal(),
            "token 被拒可经重读 discovery 自愈，不得终止连接生命周期"
        );
        assert!(err.is_credential());
    }

    /// JSON 401 `lease_required` → `LeaseRequired`。
    #[test]
    fn json_lease_required_is_lease_required() {
        let err = classify(
            401,
            r#"{"code":"lease_required","message":"attach the session seed first"}"#,
        );
        assert!(matches!(err, ApiError::LeaseRequired(_)), "{err:?}");
        assert!(!err.is_fatal());
    }

    /// 尾部空白/CRLF 不得影响 plain 判定（HTTP body 可能带换行）。
    #[test]
    fn plain_unauthorized_tolerates_surrounding_whitespace() {
        let err = classify(401, "  unauthorized\n");
        assert!(matches!(err, ApiError::Unauthorized), "{err:?}");
    }

    /// 426 → 唯一不可自愈情形（`is_fatal`）。
    #[test]
    fn unsupported_version_is_the_only_fatal_case() {
        let err = classify(
            426,
            r#"{"command_id":"","status":"rejected","code":"unsupported_version","message":"unsupported Ringing schema/version"}"#,
        );
        assert!(matches!(err, ApiError::UnsupportedVersion(_)), "{err:?}");
        assert!(err.is_fatal(), "协议代差是唯一停止重试的情形");
        assert!(!err.is_credential());
    }

    /// 普通 4xx/5xx 既非致命也非凭据类（走普通退避重试）。
    #[test]
    fn plain_http_error_is_neither_fatal_nor_credential() {
        let err = classify(500, "boom");
        assert!(matches!(err, ApiError::Http { status: 500, .. }), "{err:?}");
        assert!(!err.is_fatal());
        assert!(!err.is_credential());
    }

    /// 热更新：`apply_discovery` 换值后请求头/URL 立即跟随。
    #[test]
    fn apply_discovery_hot_swaps_credentials() {
        let client = HttpClient::new("http://127.0.0.1:1", "old-token", "ci".into());
        assert!(!client.apply_discovery("http://127.0.0.1:1", "old-token"));
        assert!(client.apply_discovery("http://127.0.0.1:2/", "new-token"));
        assert_eq!(client.base_url(), "http://127.0.0.1:2");
        assert_eq!(client.token(), "new-token");
    }
}
