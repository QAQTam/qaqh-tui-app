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

#[derive(Debug, Error)]
pub enum ApiError {
    /// Bearer token 被拒（plain 401）——不要循环重试，提示用户。
    #[error("token 被拒绝（unauthorized）")]
    Unauthorized,
    /// lease 缺失/过期/seed 未 attach（JSON 401 `lease_required`）→ 需重新 open+attach。
    #[error("lease 失效：{0}")]
    LeaseRequired(String),
    /// 426 unsupported_version → 提示需更新，停止重试。
    #[error("协议版本不被接受：{0}")]
    UnsupportedVersion(String),
    #[error("服务错误 {status} {code}: {message}")]
    Http { status: u16, code: String, message: String },
    #[error("网络错误：{0}")]
    Network(String),
    #[error("协议错误：{0}")]
    Protocol(String),
}

impl ApiError {
    pub fn is_lease_required(&self) -> bool {
        matches!(self, ApiError::LeaseRequired(_))
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ApiError::Unauthorized)
    }

    pub fn is_unsupported_version(&self) -> bool {
        matches!(self, ApiError::UnsupportedVersion(_))
    }
}

pub struct HttpClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
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
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            instance_id,
            session_id: RwLock::new(String::new()),
            open_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn session_id(&self) -> String {
        self.session_id.read().expect("session lock").clone()
    }

    fn set_session_id(&self, id: String) {
        *self.session_id.write().expect("session lock") = id;
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// 双头请求构造器（open 之外的一切请求）。
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
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
        // 401：plain "unauthorized"（token 错）或 JSON lease_required（租约问题）。
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(err) = serde_json::from_str::<WireError>(&text) {
                if err.code == "lease_required" {
                    return Err(ApiError::LeaseRequired(err.message));
                }
                return Err(ApiError::LeaseRequired(err.code));
            }
            return Err(ApiError::Unauthorized);
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
        Err(ApiError::Http { status: status.as_u16(), code, message })
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        rb: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        let resp = rb.timeout(timeout).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
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
            .bearer_auth(&self.token)
            .json(&req)
            .timeout(OPEN_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
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
        let resp = rb.timeout(OPEN_TIMEOUT).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
        self.classify(status, body, Ok).await
    }

    /// 发送命令信封。command_id 幂等：accepted 前可安全重试。
    pub async fn command(
        &self,
        envelope: &RingingCommandEnvelope,
    ) -> Result<RingingCommandAck, ApiError> {
        envelope.validate().map_err(|code| ApiError::Protocol(code.to_string()))?;
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
        self.send_json(self.request(reqwest::Method::GET, &path), COMMAND_TIMEOUT).await
    }

    /// 服务面 RPC（方法名必须来自 protocol::methods 常量）。
    pub async fn service(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let path = format!("/ringing/v1/service/{method}");
        let rb = self.request(reqwest::Method::POST, &path).json(params);
        let resp = rb.timeout(SERVICE_TIMEOUT).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
        self.classify(status, body, |text| serde_json::from_str(&text).map_err(|e| {
            ApiError::Protocol(format!("service {method} 响应解析失败: {e}"))
        }))
        .await
    }

    pub async fn session_list(&self) -> Result<Vec<SessionMetaView>, ApiError> {
        let value = self.service(crate::protocol::methods::SESSION_LIST, &serde_json::json!({})).await?;
        let arr = value
            .as_array()
            .ok_or_else(|| ApiError::Protocol("session.list 应返回数组".into()))?;
        Ok(arr.iter().filter_map(SessionMetaView::parse).collect())
    }

    /// bootstrap：三频道快照原子恢复。
    pub async fn bootstrap(&self, seed: &str) -> Result<RingingSessionBootstrap, ApiError> {
        let path = format!("/ringing/v1/sessions/{seed}/bootstrap");
        self.send_json(self.request(reqwest::Method::GET, &path), SNAPSHOT_TIMEOUT).await
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
        self.send_json(self.request(reqwest::Method::GET, &path), SNAPSHOT_TIMEOUT).await
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
        let resp = rb.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return self.classify(status, body, |text| Err(ApiError::Protocol(truncate(&text, 200)))).await;
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
            body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        };
        push_field("seed", seed.as_bytes(), &mut body);
        push_field("media_type", media_type.as_bytes(), &mut body);
        push_field("content", &bytes, &mut body);
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let rb = self
            .request(reqwest::Method::POST, "/ringing/v1/content")
            .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(body);
        self.send_json(rb, UPLOAD_TIMEOUT).await
    }

    /// 内容下载（校验 sha256 与引用一致）。
    #[allow(dead_code)]
    pub async fn download_content(&self, content: &ContentRef, seed: &str) -> Result<Vec<u8>, ApiError> {
        let path = format!("/ringing/v1/content/{}?seed={}", content.content_id, urlencode(seed));
        let rb = self.request(reqwest::Method::GET, &path);
        let resp = rb.timeout(UPLOAD_TIMEOUT).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return self.classify(status, body, |text| Err(ApiError::Protocol(truncate(&text, 200)))).await;
        }
        let bytes = resp.bytes().await.map_err(|e| ApiError::Network(e.to_string()))?;
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
    RingingCommandEnvelope::new(uuid::Uuid::new_v4().to_string(), client.instance_id.clone(), command)
        .with_client_session_id(client.session_id())
}
