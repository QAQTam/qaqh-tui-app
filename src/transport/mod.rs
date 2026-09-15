//! 传输层：daemon 发现、HTTP 双头客户端、手写 SSE 解码。
//!
//! 协议纪律（PLAN.md §2）：Bearer + `X-QAQH-Client-Session-Id` 双头；
//! token 仅内存持有；SSE 手写流解析（逐字节判活）；禁止 WebSocket/轮询。
//!
//! **T-01 阶段一后本目录只剩服务面**：daemon 发现、SSE 帧解码、三条频道流与
//! per-seed timeline 流全部由 `qaqh-client` 承担（见 `crate::runtime`）。
//! 原先的 `sse.rs`（手写解码器）与 `discovery.rs`（发现/拉起 daemon）已删除。

pub mod http;
