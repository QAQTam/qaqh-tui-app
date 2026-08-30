//! 运行时编排：open/lease 续期循环、三频道 SSE 流、per-seed timeline 流。
//!
//! 行为契约（对照 `qaqh-client`，修正 winui 侧已知弱点）：
//! - 续租间隔 = `renew_interval_ms / 2`（下限 1s）；连续 2 次失败 → 重新 open；
//! - epoch 变化（daemon 重启）→ 所有频道 cursor 归零 + timeline re-baseline；
//! - 同 epoch 内重 open（租约过期）→ 流保持 cursor，等 attach 恢复后自然续上；
//! - 频道 SSE：校验 `envelope.stream_seq == 帧 id seq`，失配 → 重连；
//! - timeline SSE：严格 +1 光标，gap/reset/epoch 变化 → 快照 re-baseline；
//! - 判活按**字节**计（45s 无字节判死），重连退避 1s→30s 带抖动；
//! - 新会话发现走 `causation_id == command_id` 的 SessionStateChanged 事件
//!   （不学 winui 的 15s 列表轮询 diff）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use crate::protocol::envelope::RingingEventEnvelope;
use crate::protocol::timeline::{TimelineEntry, TimelinePage};
use crate::protocol::Channel;
use crate::transport::http::{ApiError, HttpClient, SSE_IDLE_TIMEOUT};
use crate::transport::sse::{backoff_delay, frame_seq, last_event_id, SseDecoder};

/// 快照窗口大小（timeline 尾页）。
pub const TIMELINE_PAGE_LIMIT: u32 = 60;
const MAX_RENEW_FAILURES: u32 = 2;

/// 连接信息（SSE 流通过 watch 感知 epoch/session 变化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnInfo {
    pub epoch: String,
    pub generation: u64,
}

impl Default for ConnInfo {
    fn default() -> Self {
        Self { epoch: String::new(), generation: 0 }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ConnEvent {
    Opening,
    /// open 成功。`epoch_changed = true` 表示 daemon 重启过（app 需 re-baseline）。
    Ready { epoch: String, epoch_changed: bool },
    /// 致命错误（token 被拒 / 协议代差）——停止重试。
    Lost(String),
    /// 非致命问题提示（renew 失败、流断开等）。
    StreamIssue { channel: Option<Channel>, error: String },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum RuntimeMsg {
    Conn(ConnEvent),
    Ringing { channel: Channel, env: Box<RingingEventEnvelope> },
    ResetRequired { channel: Channel, seed: String },
    Timeline { seed: String, entry: Box<TimelineEntry> },
    TimelineRebaseline { seed: String, page: Box<TimelinePage> },
    TimelineLost { seed: String, error: String },
}

/// 运行时编排器：拥有全部后台任务的生命周期。
#[allow(dead_code)] // client 为任务句柄
pub struct Runtime {
    pub client: Arc<HttpClient>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    conn_tx: watch::Sender<ConnInfo>,
    seeds_tx: watch::Sender<Vec<String>>,
    shutdown_tx: watch::Sender<bool>,
}

impl Runtime {
    pub fn start(client: Arc<HttpClient>, msg_tx: mpsc::UnboundedSender<RuntimeMsg>) -> Self {
        let (conn_tx, conn_rx) = watch::channel(ConnInfo::default());
        let (seeds_tx, seeds_rx) = watch::channel(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        tokio::spawn(supervisor(client.clone(), conn_tx.clone(), msg_tx.clone(), shutdown_rx.clone()));
        for channel in Channel::ALL {
            tokio::spawn(channel_stream(
                client.clone(),
                conn_rx.clone(),
                msg_tx.clone(),
                channel,
                shutdown_rx.clone(),
            ));
        }
        tokio::spawn(timeline_manager(
            client.clone(),
            conn_rx,
            seeds_rx,
            msg_tx.clone(),
            shutdown_rx.clone(),
        ));

        Self { client, msg_tx, conn_tx, seeds_tx, shutdown_tx }
    }

    /// app 维护的 open 标签页 seed 集合（驱动 per-seed timeline 流 + 重连后重 attach）。
    pub fn set_tracked_seeds(&self, seeds: Vec<String>) {
        let _ = self.seeds_tx.send(seeds);
    }

#[allow(dead_code)]
    pub fn conn_info(&self) -> ConnInfo {
        self.conn_tx.borrow().clone()
    }

#[allow(dead_code)]
    pub fn msg_sender(&self) -> mpsc::UnboundedSender<RuntimeMsg> {
        self.msg_tx.clone()
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        // 给流一点时间退出，避免终端恢复时的竞态输出。
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown.changed() => true,
    }
}

/// open + 续租循环（连接生命周期的唯一属主）。
async fn supervisor(
    client: Arc<HttpClient>,
    conn_tx: watch::Sender<ConnInfo>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    shutdown: watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let mut known_epoch: Option<String> = None;
    let mut generation: u64 = 0;
    let mut attempt: u32 = 0;

    loop {
        if *shutdown.borrow() {
            return;
        }
        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Opening));
        match client.open().await {
            Ok(open) => {
                attempt = 0;
                let epoch_changed = known_epoch.as_deref() != Some(open.server_epoch.as_str());
                generation += 1;
                let _ = conn_tx.send(ConnInfo { epoch: open.server_epoch.clone(), generation });
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Ready {
                    epoch: open.server_epoch.clone(),
                    epoch_changed,
                }));
                known_epoch = Some(open.server_epoch);

                // 续租循环。
                let interval =
                    Duration::from_millis(std::cmp::max(1000, open.renew_interval_ms / 2));
                let mut failures: u32 = 0;
                loop {
                    if sleep_or_shutdown(interval, &mut shutdown).await {
                        return;
                    }
                    match client.renew().await {
                        Ok(_) => failures = 0,
                        Err(e) if e.is_unauthorized() => {
                            let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                            return;
                        }
                        // 租约已死：跳过注定失败的 renew，直接重新协商。
                        Err(e) if e.is_lease_required() => {
                            let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                                channel: None,
                                error: format!("租约失效，重新协商：{e}"),
                            }));
                            break;
                        }
                        Err(e) => {
                            failures += 1;
                            let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                                channel: None,
                                error: format!("renew 失败（{failures}/{MAX_RENEW_FAILURES}）：{e}"),
                            }));
                            if failures >= MAX_RENEW_FAILURES {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) if e.is_unsupported_version() => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(format!(
                    "协议代差，需要更新客户端：{e}"
                ))));
                return; // 代差 → 停止重试
            }
            Err(e) => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
                    return;
                }
                attempt += 1;
            }
        }
    }
}

/// 处理一帧频道 SSE；返回 false 表示协议失配，需要重连。
fn handle_channel_frame(
    msg_tx: &mpsc::UnboundedSender<RuntimeMsg>,
    channel: Channel,
    frame: crate::transport::sse::SseFrame,
    cursor: &mut u64,
) -> bool {
    if frame.event_type == "ringing.reset_required" {
        if let Some(reset) = HttpClient::parse_reset(&frame.data) {
            let _ = msg_tx.send(RuntimeMsg::ResetRequired { channel: reset.channel, seed: reset.seed });
        }
        return true;
    }
    let Ok(env) = serde_json::from_str::<RingingEventEnvelope>(frame.data.trim()) else {
        return false;
    };
    if env.channel() != channel {
        return false;
    }
    if env.validate().is_err() {
        return false;
    }
    // 光标必须与帧 id 一致；只有通过校验的信封才推进光标。
    match frame_seq(&frame.id, channel.as_str()) {
        Some(seq) if env.stream_seq == seq => *cursor = seq,
        _ => return false,
    }
    let _ = msg_tx.send(RuntimeMsg::Ringing { channel, env: Box::new(env) });
    true
}

/// 单频道 SSE 流任务（control / conversation / tool 各一条）。
async fn channel_stream(
    client: Arc<HttpClient>,
    mut conn_rx: watch::Receiver<ConnInfo>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    channel: Channel,
    shutdown: watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let mut cursor: u64 = 0;
    let mut known_epoch = String::new();
    let mut attempt: u32 = 0;
    let path = format!("/ringing/v1/events/{}", channel.path_segment());

    loop {
        if *shutdown.borrow() {
            return;
        }
        let epoch = conn_rx.borrow().epoch.clone();
        if epoch.is_empty() {
            if sleep_or_shutdown(Duration::from_millis(200), &mut shutdown).await {
                return;
            }
            continue;
        }
        // epoch 变化（daemon 重启）→ 旧 cursor 语义失效，从 0 开始。
        if epoch != known_epoch {
            cursor = 0;
            known_epoch = epoch.clone();
        }
        let lei = (cursor > 0).then(|| last_event_id(&epoch, channel.as_str(), cursor));

        match client.sse_connect(&path, lei).await {
            Err(e) if e.is_unsupported_version() => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                return;
            }
            Err(e) => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                    channel: Some(channel),
                    error: e.to_string(),
                }));
                if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
                    return;
                }
                attempt += 1;
            }
            Ok(resp) => {
                attempt = 0;
                let mut stream = resp.bytes_stream();
                let mut decoder = SseDecoder::new();
                let mut reconnect = false;
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        result = conn_rx.changed() => {
                            match result {
                                Ok(()) => {
                                    if conn_rx.borrow().epoch != epoch {
                                        cursor = 0;
                                        reconnect = true;
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                        chunk = tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()) => {
                            match chunk {
                                // 45s 无任何字节（含 keepalive）→ 判死重连。
                                Err(_) => reconnect = true,
                                Ok(Some(Ok(bytes))) => {
                                    decoder.push(&bytes);
                                    while let Some(item) = decoder.next_frame() {
                                        match item {
                                            Ok(frame) => {
                                                if !handle_channel_frame(&msg_tx, channel, frame, &mut cursor) {
                                                    reconnect = true;
                                                    break;
                                                }
                                            }
                                            Err(()) => {}
                                        }
                                    }
                                }
                                Ok(Some(Err(_))) | Ok(None) => reconnect = true,
                            }
                        }
                    }
                    if reconnect {
                        break;
                    }
                }
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                    channel: Some(channel),
                    error: "连接断开，准备重连".into(),
                }));
            }
        }
        if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
            return;
        }
        attempt += 1;
    }
}

/// timeline 流管理：按 tracked seeds 动态增减 per-seed 流任务。
async fn timeline_manager(
    client: Arc<HttpClient>,
    conn_rx: watch::Receiver<ConnInfo>,
    mut seeds_rx: watch::Receiver<Vec<String>>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    shutdown: watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let mut tasks: HashMap<String, watch::Sender<bool>> = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                for (_, tx) in tasks.drain() { let _ = tx.send(true); }
                return;
            }
            changed = seeds_rx.changed() => {
                if changed.is_err() { return; }
                let wanted = seeds_rx.borrow_and_update().clone();
                tasks.retain(|seed, tx| {
                    if wanted.contains(seed) {
                        true
                    } else {
                        let _ = tx.send(true);
                        false
                    }
                });
                for seed in wanted {
                    if tasks.contains_key(&seed) {
                        continue;
                    }
                    let (cancel_tx, cancel_rx) = watch::channel(false);
                    tasks.insert(seed.clone(), cancel_tx);
                    tokio::spawn(timeline_stream(
                        client.clone(),
                        conn_rx.clone(),
                        msg_tx.clone(),
                        seed,
                        cancel_rx,
                        shutdown.clone(),
                    ));
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct TimelineFrameData {
    #[allow(dead_code)]
    #[serde(default)]
    schema: String,
    #[allow(dead_code)]
    #[serde(default)]
    version: u32,
    server_epoch: String,
    seed: String,
    entry: TimelineEntry,
}

/// 单 seed timeline 流任务：快照基线 → SSE 严格 +1 → gap/reset/epoch 变化时
/// 重取快照 re-baseline（watermark 为新光标）。
async fn timeline_stream(
    client: Arc<HttpClient>,
    mut conn_rx: watch::Receiver<ConnInfo>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    seed: String,
    mut cancel: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let mut attempt: u32 = 0;

    loop {
        if *cancel.borrow() || *shutdown.borrow() {
            return;
        }
        let epoch = conn_rx.borrow().epoch.clone();
        if epoch.is_empty() {
            if sleep_or_shutdown(Duration::from_millis(200), &mut shutdown).await {
                return;
            }
            continue;
        }

        // 1) 快照基线（attach 可能尚未落地：lease_required → 短退避重试）。
        let page = match client.timeline_page(&seed, None, TIMELINE_PAGE_LIMIT).await {
            Ok(page) => page,
            Err(e @ ApiError::LeaseRequired(_)) => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                    channel: None,
                    error: format!("timeline 等待 attach：{e}"),
                }));
                if sleep_or_shutdown(Duration::from_millis(400), &mut shutdown).await {
                    return;
                }
                continue;
            }
            Err(e @ ApiError::Http { status: 404, .. }) => {
                let _ = msg_tx.send(RuntimeMsg::TimelineLost { seed, error: e.to_string() });
                return; // seed 已不存在
            }
            Err(e) => {
                if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
                    return;
                }
                attempt += 1;
                let _ = e;
                continue;
            }
        };
        attempt = 0;
        let mut cursor = page.snapshot.watermark;
        let _ = msg_tx.send(RuntimeMsg::TimelineRebaseline { seed: seed.clone(), page: Box::new(page) });

        // 2) SSE 追加。
        let path = format!("/ringing/v1/sessions/{seed}/timeline/events");
        let lei = (cursor > 0).then(|| last_event_id(&epoch, "timeline", cursor));
        let resp = match client.sse_connect(&path, lei).await {
            Ok(resp) => resp,
            Err(e) => {
                if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
                    return;
                }
                attempt += 1;
                let _ = e;
                continue;
            }
        };
        attempt = 0;

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        // Recover = 回到快照基线（gap / reset / epoch 变化）。
        let mut recover = false;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = cancel.changed() => {
                    if *cancel.borrow() { return; }
                }
                result = conn_rx.changed() => {
                    match result {
                        Ok(()) => {
                            if conn_rx.borrow().epoch != epoch { recover = true; }
                        }
                        Err(_) => return,
                    }
                }
                chunk = tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()) => {
                    match chunk {
                        Err(_) => break, // 空闲判死 → 重连（cursor 保留）
                        Ok(Some(Ok(bytes))) => {
                            decoder.push(&bytes);
                            while let Some(item) = decoder.next_frame() {
                                let Ok(frame) = item else { continue };
                                if frame.event_type == "ringing.reset_required" {
                                    recover = true;
                                    break;
                                }
                                if frame.event_type != "timeline.entry" {
                                    continue;
                                }
                                let parsed = serde_json::from_str::<TimelineFrameData>(frame.data.trim());
                                let data = match parsed {
                                    Ok(data) => data,
                                    Err(_) => { recover = true; break; }
                                };
                                if data.seed != seed || data.server_epoch != epoch {
                                    recover = true;
                                    break;
                                }
                                // 严格 +1：gap 一律 re-baseline，禁止本地猜测。
                                if data.entry.timeline_seq != cursor + 1 {
                                    recover = true;
                                    break;
                                }
                                cursor = data.entry.timeline_seq;
                                let _ = msg_tx.send(RuntimeMsg::Timeline {
                                    seed: seed.clone(),
                                    entry: Box::new(data.entry),
                                });
                            }
                        }
                        Ok(Some(Err(_))) | Ok(None) => break,
                    }
                }
            }
            if recover {
                break;
            }
        }
    }
}
