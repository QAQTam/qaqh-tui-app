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

use crate::protocol::Channel;
use crate::protocol::envelope::RingingEventEnvelope;
use crate::protocol::timeline::{TimelineEntry, TimelinePage};
use crate::transport::http::{ApiError, HttpClient, SSE_IDLE_TIMEOUT};
use crate::transport::sse::{SseDecoder, backoff_delay, frame_seq, last_event_id};

/// 快照窗口大小（timeline 尾页）。
pub const TIMELINE_PAGE_LIMIT: u32 = 60;
const MAX_RENEW_FAILURES: u32 = 2;

/// 连接信息（SSE 流通过 watch 感知 epoch/session 变化）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnInfo {
    pub epoch: String,
    pub generation: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ConnEvent {
    Opening,
    /// open 成功。`epoch_changed = true` 表示 daemon 重启过（app 需 re-baseline）。
    Ready {
        epoch: String,
        epoch_changed: bool,
    },
    /// **致命错误**（协议代差）——停止重试。凭据/租约类失败不再走这里：
    /// daemon 重启会换 token/端口，重读 discovery 即可自愈（BUG-2026-09-14-01）。
    Lost(String),
    /// 非致命问题提示（renew 失败、流断开等）。
    StreamIssue {
        channel: Option<Channel>,
        error: String,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum RuntimeMsg {
    Conn(ConnEvent),
    Ringing {
        channel: Channel,
        env: Box<RingingEventEnvelope>,
    },
    ResetRequired {
        channel: Channel,
        seed: String,
    },
    Timeline {
        seed: String,
        entry: Box<TimelineEntry>,
    },
    TimelineRebaseline {
        seed: String,
        page: Box<TimelinePage>,
    },
    TimelineLost {
        seed: String,
        error: String,
    },
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

        tokio::spawn(supervisor(
            client.clone(),
            conn_tx.clone(),
            msg_tx.clone(),
            shutdown_rx.clone(),
        ));
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

        Self {
            client,
            msg_tx,
            conn_tx,
            seeds_tx,
            shutdown_tx,
        }
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

/// `supervisor` 单次失败后的动作（纯决策，便于回归测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorAction {
    /// 永久停止连接生命周期（仅协议代差）。
    Stop,
    /// 跳出续租循环，重新 open 换新租约。
    Reconnect,
    /// 继续下一个 tick（可容忍的偶发失败）。
    Retry,
}

/// 由错误分类决定 supervisor 的动作——**本函数是 BUG-2026-09-14-01 的回归锁**。
///
/// 旧实现里 renew 的 plain 401（`lease expired or unknown`）被归为
/// `Unauthorized` 后直接 `return`，supervisor 永久退出 → 客户端再也无法回连。
/// 现在只有协议代差（`is_fatal`）才允许 `Stop`。
fn supervisor_action(error: &ApiError, failures: u32) -> SupervisorAction {
    if error.is_fatal() {
        SupervisorAction::Stop
    } else if error.is_credential() || failures >= MAX_RENEW_FAILURES {
        SupervisorAction::Reconnect
    } else {
        SupervisorAction::Retry
    }
}

/// 重读 `daemon.json` 并把新 endpoint/token 写入 client（daemon 重启自愈）。
///
/// BUG-2026-09-14-01：token 由 daemon 启动时随机生成（`server.rs:119`），
/// 客户端只在启动时读一次——daemon 重启后旧 token 永远 401，而旧代码把 401
/// 当致命错误直接终止连接生命周期，导致「再也连不回去」。
///
/// 返回是否拿到记录（不代表值一定变了）。
fn refresh_credentials(client: &HttpClient) -> bool {
    let Some(discovery) = crate::transport::discovery::read_discovery() else {
        return false;
    };
    client.apply_discovery(&discovery.base_url(), &discovery.token);
    true
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
    // 启动即对齐 discovery：daemon 可能在 TUI 运行期间重启过（换 token/端口）。
    refresh_credentials(&client);

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
                let _ = conn_tx.send(ConnInfo {
                    epoch: open.server_epoch.clone(),
                    generation,
                });
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
                        Err(e) => {
                            // BUG-2026-09-14-01：renew 的 plain 401
                            // （`lease expired or unknown`）曾落入「token 错」分支被
                            // 当作致命错误 `return`，supervisor 永久退出 → 客户端
                            // 再也无法回连。现在由 `supervisor_action` 统一裁决，
                            // 仅协议代差才停止生命周期。
                            failures += 1;
                            match supervisor_action(&e, failures) {
                                SupervisorAction::Stop => {
                                    let _ = msg_tx
                                        .send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                                    return;
                                }
                                SupervisorAction::Reconnect => {
                                    // 凭据类：先重读 discovery 换新 token（daemon
                                    // 重启场景），再 break 出去重新 open 换新租约。
                                    let refreshed =
                                        e.is_credential() && refresh_credentials(&client);
                                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                                        channel: None,
                                        error: if refreshed {
                                            format!(
                                                "凭据/租约失效，已重读 daemon 记录，重新协商：{e}"
                                            )
                                        } else {
                                            format!("renew 失败（{failures}/{MAX_RENEW_FAILURES}），重新协商：{e}")
                                        },
                                    }));
                                    break;
                                }
                                SupervisorAction::Retry => {
                                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                                        channel: None,
                                        error: format!(
                                            "renew 失败（{failures}/{MAX_RENEW_FAILURES}）：{e}"
                                        ),
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            Err(e) if e.is_fatal() => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(format!(
                    "协议代差，需要更新客户端：{e}"
                ))));
                return; // 代差 → 停止重试（唯一不可自愈情形）
            }
            Err(e) => {
                // 凭据类失败：daemon 可能已重启（换 token/端口），重读 discovery
                // 后再退避重试，否则会拿着旧 token 无限 401（BUG-2026-09-14-01）。
                if e.is_credential() {
                    refresh_credentials(&client);
                }
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                if sleep_or_shutdown(backoff_delay(attempt), &mut shutdown).await {
                    return;
                }
                attempt += 1;
            }
        }
    }
}

/// daemon 因 live 广播 `Lagged`（事件环溢出）而下发的终止帧（`sse.rs`）。
///
/// BUG-2026-09-14-01：TUI 此前不识别它——频道流会把它当「坏信封」静默重连
/// （不报原因），timeline 流则因 `event_type != "timeline.entry"` 直接 continue。
/// 服务端发出此帧后**立即关流**，客户端唯一正确动作是重连 re-baseline；
/// 这里把它归一为可诊断的提示（对照 `qaqh-client/src/sse.rs:182-190`）。
pub const STREAM_TERMINATED: &str = "ringing.stream_terminated";

/// 连接信息变化后，流任务对当前流的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamRebuild {
    /// 变化与本流无关（相位、错误摘要等）——继续持有当前流。
    None,
    /// 同 epoch 重协商（租约过期 → 新 `client_session_id`）：cursor 在同 epoch
    /// 内仍有效，仅重建流以继续续传。
    Rebuild,
    /// epoch 变化（daemon 重启）：旧 cursor 语义失效，归零后重建。
    ResetAndRebuild,
}

/// 由连接信息变化决定流的处置——**本函数是 D-3（伪健康黑障）的回归锁**。
///
/// 事故链：daemon 租约过期后重新 open，`epoch` 不变而 `client_session_id` 换新
/// （`ConnInfo.generation` 递增）。旧流只比 epoch，于是既不重连也不归零，继续
/// 持已失效的 session——服务端 `is_active_session` 为假、不再投递任何事件，
/// 客户端状态却仍是 `ready`（伪健康黑障，见 report D-3）。
///
/// 两条流共用本函数：epoch 变 → 归零重建；仅 generation 变 → 保留 cursor 重建；
/// 两者都不变 → 不动（`conn_rx` 会因相位/错误摘要变化而唤醒，不得误判为重连）。
fn stream_rebuild(
    epoch: &str,
    known_generation: u64,
    new_epoch: &str,
    new_generation: u64,
) -> StreamRebuild {
    if new_epoch != epoch {
        StreamRebuild::ResetAndRebuild
    } else if new_generation != known_generation {
        StreamRebuild::Rebuild
    } else {
        StreamRebuild::None
    }
}

/// 解析终止帧的 `code` 字段（缺失/非法 JSON → `unknown`）。
fn stream_terminated_code(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data.trim())
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// 处理一帧频道 SSE；返回 false 表示需要重连。
fn handle_channel_frame(
    msg_tx: &mpsc::UnboundedSender<RuntimeMsg>,
    channel: Channel,
    frame: crate::transport::sse::SseFrame,
    cursor: &mut u64,
) -> bool {
    if frame.event_type == STREAM_TERMINATED {
        // 服务端缓冲溢出：cursor 可能已跨越丢弃区间，必须重连重定基。
        let code = stream_terminated_code(&frame.data);
        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
            channel: Some(channel),
            error: format!("服务端终止流（{code}），重连重定基"),
        }));
        return false;
    }
    if frame.event_type == "ringing.reset_required" {
        if let Some(reset) = HttpClient::parse_reset(&frame.data) {
            let _ = msg_tx.send(RuntimeMsg::ResetRequired {
                channel: reset.channel,
                seed: reset.seed,
            });
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
    let _ = msg_tx.send(RuntimeMsg::Ringing {
        channel,
        env: Box::new(env),
    });
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
        let (epoch, generation) = {
            let info = conn_rx.borrow();
            (info.epoch.clone(), info.generation)
        };
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
        // 同 epoch 内的重新协商计数（租约过期 → 新 client_session_id）。
        // BUG-2026-09-14-01：旧流只比 epoch，重协商后仍持旧 session 死等，
        // daemon 侧因旧 lease 失效而不再投递任何事件（伪健康黑障）。
        let known_generation = generation;
        let lei = (cursor > 0).then(|| last_event_id(&epoch, channel.as_str(), cursor));

        match client.sse_connect(&path, lei).await {
            Err(e) if e.is_fatal() => {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(e.to_string())));
                return;
            }
            Err(e) => {
                if e.is_credential() {
                    refresh_credentials(&client);
                }
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
                                    let (new_epoch, new_generation) = {
                                        let info = conn_rx.borrow();
                                        (info.epoch.clone(), info.generation)
                                    };
                                    match stream_rebuild(
                                        &epoch,
                                        known_generation,
                                        &new_epoch,
                                        new_generation,
                                    ) {
                                        StreamRebuild::None => {}
                                        // 同 epoch 重协商（租约过期换新 cs）：
                                        // cursor 在同 epoch 内仍有效，保留续传。
                                        StreamRebuild::Rebuild => reconnect = true,
                                        StreamRebuild::ResetAndRebuild => {
                                            cursor = 0;
                                            reconnect = true;
                                        }
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
                                        if let Ok(frame) = item
                                            && !handle_channel_frame(&msg_tx, channel, frame, &mut cursor) {
                                                reconnect = true;
                                                break;
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
        let (epoch, generation) = {
            let info = conn_rx.borrow();
            (info.epoch.clone(), info.generation)
        };
        if epoch.is_empty() {
            if sleep_or_shutdown(Duration::from_millis(200), &mut shutdown).await {
                return;
            }
            continue;
        }
        // 同 epoch 内的重新协商计数（租约过期换新 cs）——见 channel_stream 同名注释。
        let known_generation = generation;

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
                let _ = msg_tx.send(RuntimeMsg::TimelineLost {
                    seed,
                    error: e.to_string(),
                });
                return; // seed 已不存在
            }
            Err(e) => {
                // 凭据类失败：daemon 可能已重启（换 token/端口），先重读 discovery。
                if e.is_credential() {
                    refresh_credentials(&client);
                }
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
        let _ = msg_tx.send(RuntimeMsg::TimelineRebaseline {
            seed: seed.clone(),
            page: Box::new(page),
        });

        // 2) SSE 追加。
        let path = format!("/ringing/v1/sessions/{seed}/timeline/events");
        let lei = (cursor > 0).then(|| last_event_id(&epoch, "timeline", cursor));
        let resp = match client.sse_connect(&path, lei).await {
            Ok(resp) => resp,
            Err(e) => {
                if e.is_credential() {
                    refresh_credentials(&client);
                }
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
                            let (new_epoch, new_generation) = {
                                let info = conn_rx.borrow();
                                (info.epoch.clone(), info.generation)
                            };
                            // epoch 变化或同 epoch 重协商（旧 cs 已被服务端作废）
                            // 均需重建流：否则 daemon 不再向旧 session 投递事件。
                            // 判定与频道流共用 `stream_rebuild`，防止两处再次漂移。
                            if stream_rebuild(
                                &epoch,
                                known_generation,
                                &new_epoch,
                                new_generation,
                            ) != StreamRebuild::None
                            {
                                recover = true;
                            }
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
                                if frame.event_type == STREAM_TERMINATED {
                                    // 溢出终止帧：服务端发完即关流。cursor 已不可信
                                    // （可能跨过丢弃区间），必须回快照基线。
                                    let code = stream_terminated_code(&frame.data);
                                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                                        channel: None,
                                        error: format!(
                                            "timeline[{seed}] 服务端终止流（{code}），重定基"
                                        ),
                                    }));
                                    recover = true;
                                    break;
                                }
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

#[cfg(test)]
mod tests {
    //! 连接生命周期决策回归（BUG-2026-09-14-01）。
    //!
    //! 事故链：流式输出中前端 SSE 报错 → daemon 端租约过期 → renew 回
    //! **plain 401** `lease expired or unknown` → 旧分类把它当 `Unauthorized`
    //! → supervisor `return` 永久退出 → 客户端再也无法回连（后续一律 401）。
    //! 本模块锁住「只有协议代差才能停止生命周期」这一不变式；同轮修复的
    //! D-3（同 epoch 重协商不重建流 → 伪健康黑障）由 `stream_rebuild` 锁定。

    use super::*;

    /// **核心回归**：renew 的租约失效必须走重连，绝不能停止生命周期。
    #[test]
    fn lease_expiry_must_reconnect_not_stop() {
        let err = ApiError::LeaseRequired("lease expired or unknown".into());
        assert_eq!(
            supervisor_action(&err, 1),
            SupervisorAction::Reconnect,
            "租约过期必须重新协商（旧 bug 在此 return 后永久卡死）"
        );
    }

    /// token 被拒（daemon 重启换 token）同样必须重连，不得停止。
    #[test]
    fn token_rejection_must_reconnect_not_stop() {
        assert_eq!(
            supervisor_action(&ApiError::Unauthorized, 1),
            SupervisorAction::Reconnect,
            "token 被拒可经重读 discovery 自愈"
        );
    }

    /// 协议代差是唯一停止情形。
    #[test]
    fn only_protocol_drift_stops_the_lifecycle() {
        let err = ApiError::UnsupportedVersion("schema mismatch".into());
        assert_eq!(supervisor_action(&err, 1), SupervisorAction::Stop);
        assert_eq!(supervisor_action(&err, 99), SupervisorAction::Stop);
    }

    /// 偶发网络错误：未达阈值先重试，达阈值才重新协商。
    #[test]
    fn transient_failures_retry_then_reconnect_at_threshold() {
        let err = ApiError::Network("connection reset".into());
        assert_eq!(supervisor_action(&err, 1), SupervisorAction::Retry);
        assert_eq!(
            supervisor_action(&err, MAX_RENEW_FAILURES),
            SupervisorAction::Reconnect,
            "连续失败达阈值后应重新协商"
        );
    }

    /// 任何非致命错误都不允许产出 `Stop`（防未来回归）。
    #[test]
    fn no_non_fatal_error_ever_stops() {
        let cases = [
            ApiError::Unauthorized,
            ApiError::LeaseRequired("x".into()),
            ApiError::Http {
                status: 500,
                code: "internal".into(),
                message: "boom".into(),
            },
            ApiError::Network("timeout".into()),
            ApiError::Protocol("bad frame".into()),
        ];
        for err in cases {
            for failures in [1, 2, 100] {
                assert_ne!(
                    supervisor_action(&err, failures),
                    SupervisorAction::Stop,
                    "{err:?} 在 failures={failures} 时不得停止生命周期"
                );
            }
        }
    }

    /// 终止帧 code 解析：正常 / 缺字段 / 非 JSON 均不 panic。
    #[test]
    fn stream_terminated_code_parsing_is_total() {
        assert_eq!(
            stream_terminated_code(r#"{"code":"lagged","skipped":7}"#),
            "lagged"
        );
        assert_eq!(stream_terminated_code("{}"), "unknown");
        assert_eq!(stream_terminated_code("not json"), "unknown");
        assert_eq!(stream_terminated_code(""), "unknown");
    }

    /// 终止帧必须让频道流判定为「需重连」（返回 false）。
    #[test]
    fn termination_frame_forces_channel_reconnect() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut cursor = 42u64;
        let frame = crate::transport::sse::SseFrame {
            id: "ep:conversation:43".into(),
            event_type: STREAM_TERMINATED.into(),
            data: r#"{"code":"lagged","channel":"conversation","skipped":3}"#.into(),
        };
        assert!(
            !handle_channel_frame(&tx, Channel::Conversation, frame, &mut cursor),
            "终止帧必须触发重连"
        );
        assert_eq!(cursor, 42, "终止帧不得推进 cursor");
        match rx.try_recv().expect("应上报可诊断的流问题") {
            RuntimeMsg::Conn(ConnEvent::StreamIssue { channel, error }) => {
                assert_eq!(channel, Some(Channel::Conversation));
                assert!(error.contains("lagged"), "错误须携带服务端 code：{error}");
            }
            other => panic!("意外消息：{other:?}"),
        }
    }

    /// **D-3 核心回归**：同 epoch 换新 `client_session_id`（generation 递增）必须
    /// 重建流，且**保留 cursor**（同 epoch 内 cursor 语义仍有效）。
    ///
    /// 旧实现只比 epoch，此处返回「不重连」——旧流继续持失效 session 死等，
    /// daemon 不再投递事件而客户端仍显示 `ready`（伪健康黑障）。
    #[test]
    fn generation_bump_rebuilds_stream_and_keeps_cursor() {
        assert_eq!(
            stream_rebuild("ep-1", 7, "ep-1", 8),
            StreamRebuild::Rebuild,
            "同 epoch 重协商必须重建流（旧 bug 在此不重连，伪健康卡死）"
        );
    }

    /// epoch 变化（daemon 重启）→ cursor 语义失效，必须归零重建。
    #[test]
    fn epoch_change_resets_cursor_and_rebuilds() {
        assert_eq!(
            stream_rebuild("ep-1", 7, "ep-2", 7),
            StreamRebuild::ResetAndRebuild
        );
    }

    /// epoch 与 generation 同时变化时以 epoch 为准——必须归零，不得沿用旧 cursor。
    #[test]
    fn epoch_change_wins_over_generation_bump() {
        assert_eq!(
            stream_rebuild("ep-1", 7, "ep-2", 8),
            StreamRebuild::ResetAndRebuild
        );
    }

    /// `conn_rx` 因相位/错误摘要变化而唤醒时不得触发重连（否则流会反复重建）。
    #[test]
    fn unrelated_conn_info_change_does_not_rebuild() {
        assert_eq!(stream_rebuild("ep-1", 7, "ep-1", 7), StreamRebuild::None);
    }

    /// 全组合扫描：只有「epoch 与 generation 都不变」才允许不动。
    /// 含 generation 倒退（校验判定是 `!=` 而非方向性比较）与空 epoch（未就绪）。
    #[test]
    fn only_unchanged_conn_info_avoids_rebuild() {
        let cases = [
            ("ep-1", 7u64, "ep-1", 7u64),
            ("ep-1", 7, "ep-1", 8),
            ("ep-1", 8, "ep-1", 7),
            ("ep-1", 7, "ep-2", 7),
            ("ep-1", 7, "ep-2", 8),
            ("", 0, "ep-1", 0),
        ];
        for (known_epoch, known_generation, new_epoch, new_generation) in cases {
            let action = stream_rebuild(known_epoch, known_generation, new_epoch, new_generation);
            let unchanged = known_epoch == new_epoch && known_generation == new_generation;
            assert_eq!(
                action == StreamRebuild::None,
                unchanged,
                "({known_epoch},{known_generation})→({new_epoch},{new_generation}) 判定错误：{action:?}"
            );
        }
    }
}
