//! 服务面 HTTP 客户端（**阶段 1.5 之前**的过渡件）。
//!
//! T-01 阶段一之后，连接生命周期、三频道 SSE 流与 per-seed timeline 流全部由
//! `qaqh-client` 承担（见 `crate::runtime`）。本文件只剩**服务面**
//! `POST /ringing/v1/service/{method}` 一条路径——阶段 1.5 会把它切到
//! `Client::query`/`action` 的封闭枚举，届时本文件整体删除。
//!
//! 因此这里刻意**不再**有 open/renew/SSE/命令/timeline 快照——它们要么已经
//! 在 `qaqh-client` 里，要么已由 `crate::runtime` 经 `Client` 调用。
//!
//! 双头注入仍在此完成：`Authorization: Bearer` 由本文件持有（token 仅内存），
//! `X-QAQH-Client-Session-Id` 由连接生命周期经 [`HttpClient::set_session_id`]
//! 注入并随重新协商刷新。

use std::sync::RwLock;
use std::time::Duration;

use thiserror::Error;

use crate::protocol::WireError;
use crate::protocol::methods::SessionMetaView;
use qaqh_client::CLIENT_SESSION_HEADER;

/// 服务面请求超时。
pub const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// daemon 对「Bearer token 被拒」返回的 plain body（`auth.rs:12`）。
/// 与 renew 的 `lease expired or unknown` 同为 plain 401，只能按内容区分。
const PLAIN_UNAUTHORIZED: &str = "unauthorized";

#[derive(Debug, Error)]
pub enum ApiError {
    /// Bearer token 被拒（plain 401 `unauthorized`）。
    ///
    /// 分类仍在此保留：服务面需要区分「token 被拒」与「租约失效」来给出正确
    /// 提示。**注意**：不再有任何「致命/可重试」判定——连接生命周期已交由
    /// `qaqh-client`，它结构上不存在「停止重试」这条路径（见 buglist D-1 改判）。
    #[error("token 被拒绝（unauthorized）")]
    Unauthorized,
    /// lease 缺失/过期/seed 未 attach（JSON 401 `lease_required`）。
    #[error("lease 失效：{0}")]
    LeaseRequired(String),
    /// 426 unsupported_version。
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

pub struct HttpClient {
    http: reqwest::Client,
    /// 可热更新：daemon 重启会换端口/token，靠 `apply_discovery` 原地换值。
    base_url: RwLock<String>,
    token: RwLock<String>,
    /// 连接生命周期协商出的 `client_session_id`，由 `set_session_id` 注入。
    session_id: RwLock<String>,
}

impl HttpClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: RwLock::new(base_url.into().trim_end_matches('/').to_string()),
            token: RwLock::new(token.into()),
            session_id: RwLock::new(String::new()),
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.read().expect("base_url lock").clone()
    }

    /// 重读 daemon 发现记录后的原地换值（daemon 重启换 token/端口）。
    /// 返回是否发生变化；变化后所有请求自动携带新值。
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

    /// 由连接生命周期侧注入（本客户端不再自己 open）。重新协商/重建后必须
    /// 重新注入，否则服务面会拿着旧 cs 一路 401。
    pub fn set_session_id(&self, id: String) {
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

    /// 双头请求构造器。
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(self.token())
            .header(
                CLIENT_SESSION_HEADER,
                self.session_id.read().expect("session lock").clone(),
            )
    }

    /// 解析 daemon 的 JSON 错误体（401 可能是 plain `unauthorized`）。
    ///
    /// 401 有三种来源，其中两种的 body 是 **plain text**——只能按内容区分：
    /// - JSON `{"code":"lease_required",...}`：租约缺失/过期/seed 未 attach；
    /// - plain `unauthorized`：Bearer token 被拒（daemon `auth.rs:12`）；
    /// - plain `lease expired or unknown`：renew 时租约已死（`command.rs:137`）。
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
            let message = serde_json::from_str::<qaqh_client::RingingCommandAck>(&text)
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

    /// 服务面 RPC（方法名必须来自 protocol::methods 常量）。
    ///
    /// 阶段 1.5 之后这里会被 `Client::query`/`action` 的封闭枚举取代——那时
    /// 方法名不可能拼错，也就不再需要这个泛型口。
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
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
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
    //!
    //! **阶段一改判**：连接生命周期已交给 `qaqh-client`，它结构上不存在
    //! 「停止重试」路径，故 `is_fatal`/`is_credential` 两个谓词及其断言已删除
    //! ——分类本身仍有意义（服务面要据此提示），但不再裁决生死。

    use super::*;

    fn classify(status: u16, body: &str) -> ApiError {
        let client = HttpClient::new("http://127.0.0.1:1", "t");
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

    /// renew 的 plain 401（租约已死）→ `LeaseRequired`，**不是** `Unauthorized`。
    #[test]
    fn plain_lease_expired_is_lease_required_not_unauthorized() {
        let err = classify(401, "lease expired or unknown");
        assert!(matches!(err, ApiError::LeaseRequired(_)), "{err:?}");
    }

    /// token 被拒的 plain 401 → `Unauthorized`。
    #[test]
    fn plain_unauthorized_is_unauthorized() {
        let err = classify(401, "unauthorized");
        assert!(matches!(err, ApiError::Unauthorized), "{err:?}");
    }

    /// JSON 401 `lease_required` → `LeaseRequired`。
    #[test]
    fn json_lease_required_is_lease_required() {
        let err = classify(
            401,
            r#"{"code":"lease_required","message":"attach the session seed first"}"#,
        );
        assert!(matches!(err, ApiError::LeaseRequired(_)), "{err:?}");
    }

    /// 尾部空白/CRLF 不得影响 plain 判定（HTTP body 可能带换行）。
    #[test]
    fn plain_unauthorized_tolerates_surrounding_whitespace() {
        let err = classify(401, "  unauthorized\n");
        assert!(matches!(err, ApiError::Unauthorized), "{err:?}");
    }

    /// 426 → `UnsupportedVersion`（服务面据此提示协议代差）。
    #[test]
    fn unsupported_version_is_classified() {
        let err = classify(
            426,
            r#"{"command_id":"","status":"rejected","code":"unsupported_version","message":"unsupported Ringing schema/version"}"#,
        );
        assert!(matches!(err, ApiError::UnsupportedVersion(_)), "{err:?}");
    }

    /// 普通 4xx/5xx 走 `Http`（既非凭据类也没有特殊语义）。
    #[test]
    fn plain_http_error_is_http() {
        let err = classify(500, "boom");
        assert!(matches!(err, ApiError::Http { status: 500, .. }), "{err:?}");
    }

    /// 热更新：`apply_discovery` 换值后请求头/URL 立即跟随。
    #[test]
    fn apply_discovery_hot_swaps_credentials() {
        let client = HttpClient::new("http://127.0.0.1:1", "old-token");
        assert!(!client.apply_discovery("http://127.0.0.1:1", "old-token"));
        assert!(client.apply_discovery("http://127.0.0.1:2/", "new-token"));
        assert_eq!(client.base_url(), "http://127.0.0.1:2");
        assert_eq!(client.token(), "new-token");
    }
}
