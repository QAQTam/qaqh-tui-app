//! 传输层：daemon 发现、HTTP 双头客户端、手写 SSE 解码。
//!
//! 协议纪律（PLAN.md §2）：Bearer + `X-QAQH-Client-Session-Id` 双头；
//! token 仅内存持有；SSE 手写流解析（逐字节判活）；禁止 WebSocket/轮询。

pub mod discovery;
pub mod http;
pub mod sse;
