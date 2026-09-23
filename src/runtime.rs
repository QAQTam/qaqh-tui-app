//! 运行时适配层：`qaqh_client::Client` ↔ 本仓 `RuntimeMsg`。
//!
//! T-01 阶段一之后，连接生命周期**不再由本仓实现**——open/续租/重新协商、
//! 三条频道 SSE 流、per-seed timeline 流（含 gap 重定基、epoch 重基线、终止帧
//! 归一、BOM 剥离、退避重连）全部由 `qaqh-client` 承担。本文件只剩三件事：
//!
//! 1. 把 `ClientHandlers` 回调转投为本仓既有的 `RuntimeMsg`（app 层因此几乎
//!    不用改）；
//! 2. 把 app 维护的 seed 集合 diff 成 `activate_timeline` / `deactivate_timeline`
//!    调用；
//! 3. 订阅 `session_ctx`（epoch/client_session_id）向 app 报告连接相位。
//!
//! 不复刻凭据同步：服务面（`Client::query`/`action`）与连接生命周期现在同属
//! 一个 `Client`，endpoint/token/session-id 只有一份，由 `qaqh-client` 自己在
//! 续租失败时重读 `daemon.json`。阶段 1.5 之前这里还额外喂着一个本仓的
//! `HttpClient`，那是双份凭据的唯一理由，已随该客户端一起删除。
//!
//! 行为契约由 `qaqh-client` 自己的测试锁定；本文件不再重复实现它们。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use qaqh_client::{
    Channel as WireChannel, ChannelStatus, Client, ClientError, ClientHandlers, ClientOptions,
    ReconnectReason, TimelineStatus,
};
use tokio::sync::mpsc;

use qaqh_client::RingingEventEnvelope;
use qaqh_client::{TimelineEntry, TimelinePage};

/// timeline 翻页窗口（`request_rebaseline` / `load_older` 使用）。
pub const TIMELINE_PAGE_LIMIT: u32 = 60;

/// 「与 daemon 失联」的判据：这么久没有任何频道处于 `Open`。
///
/// 对齐 winui 的 `compute_stall`（同样 15s）。`qaqh-client` 不会因为失败而
/// 停止生命周期，所以**没有**致命错误信号可等——失联只能靠「一直连不上」推断。
/// 判定「与 daemon 失联」的静默阈值。
///
/// **必须大于客户端的 SSE 空闲超时（`qaqh-client` 的 `SSE_IDLE_TIMEOUT = 45s`）**：
/// 一条健康但安静 >45s 的流会被客户端主动断开重连，重连成功即刷新活跃时间戳
/// ——探活由那条路径负责。本阈值只该在「连重连都没能恢复」时才触发。
/// 取 15s 时会反过来：空闲流每 45s 周期里有 30s 显示「失联」。
///
/// 20s = daemon 的 keepalive 间隔（15s，见 `qaqh-daemon` 的
/// `KeepAlive::new().interval(15s)`）留一轮余量。
///
/// 真正的信号是**每收到一块字节**（`note_daemon_activity`），而 keepalive 正好
/// 每 15s 送一块——所以「daemon 活着」这件事每 15s 被确认一次，20s 是它的一点
/// 余量。真死时 keepalive 断流，20s 后判定，灵敏度与最初的 15s 相当。
const STALL_AFTER: Duration = Duration::from_secs(20);
const STALL_TICK: Duration = Duration::from_secs(1);

/// timeline 激活时等 attach 落地的重试窗口（见 `activate_with_attach_retry`）。
const ATTACH_RETRY_INTERVAL: Duration = Duration::from_millis(400);
const ATTACH_RETRY_ATTEMPTS: u32 = 75; // ≈30s

/// 流的身份：三条主频道流 + 每个 seed 的 timeline 流。
///
/// 告警与恢复必须**按流**记账：某条 timeline 流断开的同时另一条频道流恰好重连
/// 成功，不能把前者的告警当成「一切正常」清掉（反向顺序下文案也会串成后者）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamKey {
    /// 主频道 SSE 流（control / conversation / tool）。
    Channel(WireChannel),
    /// 某个 seed 的 timeline 流。
    Timeline(String),
}

/// 连接相位事件。
#[derive(Debug, Clone)]
pub enum ConnEvent {
    /// 尚未协商出 session（daemon 不可达/冷启动中）。
    Opening,
    /// 已协商到 session。`epoch_changed = true` 表示 daemon 重启过（app 需
    /// re-baseline 并提示）。
    Ready { epoch: String, epoch_changed: bool },
    /// 与 daemon 失联（≥[`STALL_AFTER`] 无任何频道连接）——可由用户触发重连。
    Lost(String),
    /// 某条流报了非致命问题（断开重连中、流被关闭等）。
    StreamIssue { stream: StreamKey, error: String },
    /// 某条流重新 `Open`——**它自己**那条 `StreamIssue` 的对应恢复信号。
    ///
    /// 单独一个变体而不是复用 `Ready`：`Ready` 在 app 侧会触发全量 re-attach +
    /// bootstrap（那是「重新协商/daemon 重启」的代价），而流重连成功只是
    /// 「刚才那条告警可以撤了」。
    StreamRecovered { stream: StreamKey },
}

/// 运行时报文（app 消费）。
#[derive(Debug)]
pub enum RuntimeMsg {
    Conn(ConnEvent),
    /// 一条已过桥的事件。频道不另存字段——`env.channel()` 即是权威来源，
    /// 再存一份只会有漂移的机会。
    Ringing {
        env: Box<RingingEventEnvelope>,
    },
    ResetRequired {
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
        /// **结构化**原因：字符串化会抹掉「404 = 会话真的没了」与「超时/网络
        /// 错 = 会话可能还活着」的区别，app 只能把任何一次抖动都当成消失。
        reason: TimelineLostReason,
    },
}

/// timeline 流丢失的原因。
///
/// 只有 [`Self::SessionMissing`] 能证明会话已不存在（服务端 404）；其余一切
/// （401 租约尚未落地、超时、网络错、流被主动停止）都只说明**这条流**没了，
/// 会话本身可能还活着。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineLostReason {
    /// 服务端明确回答「该会话不存在」（HTTP 404）。
    SessionMissing,
    /// 其余原因：保留现状，只提示（附可读描述）。
    Other(String),
}

impl TimelineLostReason {
    /// 由 `activate_timeline` 的错误归一。**404 是唯一**的「会话不存在」判据。
    fn from_client_error(err: &ClientError) -> Self {
        match err {
            ClientError::Http { status: 404, .. } => Self::SessionMissing,
            other => Self::Other(other.to_string()),
        }
    }

    /// 展示用描述（toast）。
    pub fn describe(&self) -> String {
        match self {
            Self::SessionMissing => "会话不存在（404）".to_string(),
            Self::Other(text) => text.clone(),
        }
    }
}

/// 生命周期属主。持有当前 `Client`（可在 T-03 手动重连时原地替换）。
pub struct Runtime {
    /// 当前客户端。**只有测试替身**（[`Runtime::stub_for_test`]）会是 `None`：
    /// 生产路径 `start` / `rebuild` 始终有值，所以 [`Runtime::client`] 可以 `expect`；
    /// 「没有连接也要照跑」的路径走 [`Runtime::client_opt`]。
    client: RwLock<Option<Arc<Client>>>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    /// app 当前跟踪的 seed 集（重连后据此恢复 timeline 流）。
    tracked: std::sync::Mutex<HashSet<String>>,
    /// 重建串行化：并发重建会得到两个 Client、两套 SSE 流。
    rebuilding: AtomicBool,
    /// 每次重建递增；旧订阅者据此退出（防止旧 session 的回调继续投递）。
    generation: AtomicU64,
    launch_daemon_if_missing: bool,
    /// 最近一次「有频道连上」的时刻（stall 判据）。
    last_open: Arc<std::sync::Mutex<Instant>>,
    stalled: Arc<AtomicBool>,
    /// 自引用弱指针：`rebuild` 是 `&self` 方法，但要为**新** Client 再起一个
    /// 相位订阅任务，需要 `Arc<Self>`。用 `Weak` 打破循环，Client 侧不持有它。
    self_arc: std::sync::Weak<Runtime>,
}

impl Runtime {
    /// 连接 daemon 并启动运行时。失败即返回（调用方决定如何提示）。
    pub async fn start(
        msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
        launch_daemon_if_missing: bool,
    ) -> Result<Arc<Self>, ClientError> {
        let last_open = Arc::new(std::sync::Mutex::new(Instant::now()));
        let client = Self::connect(&msg_tx, &last_open, launch_daemon_if_missing).await?;

        let runtime = Arc::new_cyclic(|weak| Self {
            client: RwLock::new(Some(client.clone())),
            msg_tx: msg_tx.clone(),
            tracked: std::sync::Mutex::new(HashSet::new()),
            rebuilding: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            launch_daemon_if_missing,
            last_open: last_open.clone(),
            stalled: Arc::new(AtomicBool::new(false)),
            self_arc: weak.clone(),
        });

        tokio::spawn(watch_session(
            runtime.clone(),
            client,
            runtime.generation.load(Ordering::SeqCst),
        ));
        tokio::spawn(watch_stall(
            runtime.clone(),
            last_open,
            msg_tx,
            runtime.stalled.clone(),
        ));
        Ok(runtime)
    }

    async fn connect(
        msg_tx: &mpsc::UnboundedSender<RuntimeMsg>,
        last_open: &Arc<std::sync::Mutex<Instant>>,
        launch_daemon_if_missing: bool,
    ) -> Result<Arc<Client>, ClientError> {
        let handlers = build_handlers(msg_tx.clone(), last_open.clone());
        Client::connect_async(ClientOptions {
            handlers,
            launch_daemon_if_missing,
            daemon_path: None,
            start_timeout: Duration::from_secs(8),
            remote: None,
        })
        .await
        .map(Arc::new)
    }

    /// 当前客户端（`spawn_api` 取用）。重建后拿到的是新实例。
    ///
    /// 同步返回：`spawn_api` 需要在**非 async** 上下文里拿到它，而且把
    /// `&self` 借进 spawned task 是不允许的。
    pub fn client(&self) -> Arc<Client> {
        self.client_opt()
            .expect("runtime client（只有测试替身没有连接，生产路径不该走到这里）")
    }

    /// 当前客户端；没有连接时返回 `None`（只有 [`Runtime::stub_for_test`] 会这样）。
    ///
    /// 给「没有连接也要走完」的路径用：`spawn_api`（任务照起，取用连接时才失败）
    /// 与 [`Runtime::set_tracked_seeds`]（没有连接就没有 timeline 流可建/可撤）。
    pub fn client_opt(&self) -> Option<Arc<Client>> {
        self.client.read().expect("client lock").clone()
    }

    /// **测试替身**：没有连接的 `Runtime`，供 `App` 层按键/事件路径的单测使用
    /// （见 `App::new_for_test`）。
    ///
    /// 只保证「不 panic 地走完同步路径」：`client_opt()` 返回 `None`，`spawn_api`
    /// 起的任务照常运行、取用连接时才失败（返回 `Err`），不去碰真 daemon。
    #[cfg(test)]
    pub fn stub_for_test() -> Arc<Self> {
        let (msg_tx, _rx) = mpsc::unbounded_channel();
        Arc::new_cyclic(|weak| Self {
            client: RwLock::new(None),
            msg_tx,
            tracked: std::sync::Mutex::new(HashSet::new()),
            rebuilding: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            launch_daemon_if_missing: false,
            last_open: Arc::new(std::sync::Mutex::new(Instant::now())),
            stalled: Arc::new(AtomicBool::new(false)),
            self_arc: weak.clone(),
        })
    }

    /// app 维护的 open 标签页/子代理 seed 集合。
    pub fn set_tracked_seeds(&self, seeds: Vec<String>) {
        let (to_add, to_remove) = {
            let mut guard = self.tracked.lock().expect("tracked lock");
            let wanted: HashSet<String> = seeds.into_iter().collect();
            let add: Vec<String> = wanted.difference(&guard).cloned().collect();
            let remove: Vec<String> = guard.difference(&wanted).cloned().collect();
            *guard = wanted;
            (add, remove)
        };
        if to_add.is_empty() && to_remove.is_empty() {
            return;
        }
        // 测试替身没有连接：跟踪集合已经更新，等 `Ready` 重新 diff 即可。
        let Some(client) = self.client_opt() else {
            return;
        };
        let msg_tx = self.msg_tx.clone();
        let generation = self.generation.load(Ordering::SeqCst);
        tokio::spawn(async move {
            for seed in to_remove {
                client.deactivate_timeline(&seed).await;
            }
            for seed in to_add {
                activate_with_attach_retry(&client, &seed, &msg_tx, generation).await;
            }
        });
    }

    pub async fn shutdown(&self) {
        if let Some(client) = self.client_opt() {
            client.close();
        }
    }

    /// **手动重连**（T-03）：关掉当前客户端并重建一个，随后恢复跟踪的 timeline。
    ///
    /// 重建会重新读 `daemon.json`（带 pid 判活过滤），所以 daemon 换端口/token、
    /// 甚至没在跑（允许拉起时）都能恢复。失败时保持 `Lost`，用户可再按一次。
    pub async fn rebuild(&self) -> Result<(), String> {
        if self
            .rebuilding
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("重连已在进行中".into());
        }
        let result = self.rebuild_inner().await;
        self.rebuilding.store(false, Ordering::SeqCst);
        result
    }

    async fn rebuild_inner(&self) -> Result<(), String> {
        // 重建是生产路径（T-03 手动重连）：没有连接就没什么可关的，直接建新的。
        if let Some(old) = self.client_opt() {
            old.close();
        }
        // 让旧任务先退出，避免两套流短暂并存。
        tokio::time::sleep(Duration::from_millis(80)).await;

        let new = Self::connect(&self.msg_tx, &self.last_open, self.launch_daemon_if_missing)
            .await
            .map_err(|e| e.to_string())?;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.client.write().expect("client lock") = Some(new.clone());
        self.stalled.store(false, Ordering::SeqCst);
        *self.last_open.lock().expect("last_open lock") = Instant::now();

        // 旧 session 的回调若还在投递，其 generation 已过期 → 自行退出。
        // （`self` 是 &self，这里无法 clone 出 Arc<Self>；见 `Runtime::rebuild`
        //   的调用方——它持有 Arc<Runtime>。）
        if let Some(this) = self.self_arc.upgrade() {
            tokio::spawn(watch_session(this, new.clone(), generation));
        }

        // 恢复 timeline：daemon 侧 seed 归属随旧 client_session_id 一起消失，
        // 必须重新 attach 才能读 seed 域（否则一路 401）。app 会由
        // `ConnEvent::Ready` 触发它自己的 attach + bootstrap。
        let seeds: Vec<String> = self
            .tracked
            .lock()
            .expect("tracked lock")
            .iter()
            .cloned()
            .collect();
        let msg_tx = self.msg_tx.clone();
        tokio::spawn(async move {
            for seed in seeds {
                activate_with_attach_retry(&new, &seed, &msg_tx, generation).await;
            }
        });
        Ok(())
    }
}

/// 激活一个 seed 的 timeline；attach 尚未落地时按 401 重试。
///
/// `open_session_tab` 是「发 SessionResume 命令」与「sync_tracked」两条并行的
/// 路径，激活可能先于 attach 完成——此时 daemon 回 401。旧实现同样以 400ms
/// 间隔重试直到 attach 落地；这里保留该行为并加一个上限，避免永久重试。
async fn activate_with_attach_retry(
    client: &Client,
    seed: &str,
    msg_tx: &mpsc::UnboundedSender<RuntimeMsg>,
    generation: u64,
) {
    for attempt in 0..ATTACH_RETRY_ATTEMPTS {
        match client.activate_timeline(seed).await {
            Ok(_) => return, // 快照经 on_timeline_snapshot 转投 TimelineRebaseline
            Err(ClientError::Http { status: 401, .. }) if attempt + 1 < ATTACH_RETRY_ATTEMPTS => {
                tokio::time::sleep(ATTACH_RETRY_INTERVAL).await;
            }
            Err(e) => {
                let _ = generation;
                let _ = msg_tx.send(RuntimeMsg::TimelineLost {
                    seed: seed.to_string(),
                    reason: TimelineLostReason::from_client_error(&e),
                });
                return;
            }
        }
    }
}

/// 订阅 `session_ctx`：报告连接相位，并在每次会话变更时同步服务面凭据。
async fn watch_session(runtime: Arc<Runtime>, client: Arc<Client>, generation: u64) {
    let mut rx = client.session_ctx_rx();
    let mut known_epoch: Option<String> = None;
    loop {
        if runtime.generation.load(Ordering::SeqCst) != generation {
            return; // 已被更新的 Client 取代：别再投递旧会话的相位
        }
        let current = rx.borrow_and_update().clone();
        match current {
            Some((epoch, _)) => {
                let epoch_changed = known_epoch.as_deref().is_some_and(|e| e != epoch);
                known_epoch = Some(epoch.clone());

                let _ = runtime.msg_tx.send(RuntimeMsg::Conn(ConnEvent::Ready {
                    epoch,
                    epoch_changed,
                }));
            }
            None => {
                let _ = runtime.msg_tx.send(RuntimeMsg::Conn(ConnEvent::Opening));
            }
        }
        if rx.changed().await.is_err() {
            return; // session 已 drop（旧 Client 被替换）：订阅者退出
        }
    }
}

/// 记一次「daemon 还在说话」。
///
/// 原先只有 `ChannelStatus::Open` 会复位这个时间戳，而客户端**只在连接建立那一刻**
/// 发一次 `Open`（`qaqh-client/src/sse.rs:142`，紧跟 HTTP 响应成功之后）。
/// 于是这个判据量的其实是「**连接建立了多久**」，而不是「**多久没听到 daemon**」——
/// 任何活过 STALL_AFTER 的连接都会被判失联，**空闲与否都一样**。
///
/// 为什么它永不自愈：daemon 每 15s 发一次 keepalive，客户端每次读到字节都会重置
/// 自己的 45s 空闲计时器，于是**流永不空闲、永不重连、`Open` 永不再发**。
/// 实测现象就是「后端在干活、前端在输出，状态栏却一直 `✗ lost`」。
///
/// 正确信号一直都在线上：**字节到了就是 daemon 活着**。现在由客户端在读取循环里
/// 每消费一个 chunk 回调一次（含被解码器丢掉的 keepalive 注释行）。
fn note_daemon_activity(last_open: &Arc<std::sync::Mutex<Instant>>) {
    *last_open.lock().expect("last_open lock") = Instant::now();
}

/// 失联检测（T-03 的触发判据）。
async fn watch_stall(
    runtime: Arc<Runtime>,
    last_open: Arc<std::sync::Mutex<Instant>>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    stalled: Arc<AtomicBool>,
) {
    loop {
        tokio::time::sleep(STALL_TICK).await;
        let stale = last_open.lock().expect("last_open lock").elapsed() > STALL_AFTER;
        let was_stalled = stalled.swap(stale, Ordering::SeqCst);
        if stale && !was_stalled {
            let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Lost(format!(
                "与 daemon 失联（{}s 内无任何频道连接）——按 R 重连",
                STALL_AFTER.as_secs()
            ))));
        } else if !stale && was_stalled {
            // 恢复：把相位推回 Ready（否则 UI 会一直停在 lost）。
            if let Some(state) = runtime.client().session_state().await {
                let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::Ready {
                    epoch: state.server_epoch,
                    epoch_changed: false,
                }));
            }
        }
    }
}

/// 把 qaqh-client 的结构化重连原因转成 TUI 可诊断文案。
///
/// `None` 是普通网络断开；`Lagged` 是服务端事件缓冲溢出终止流；其他稳定 code
/// 原样透出。该文案同时覆盖 U-07 要求的 `lagged` 诊断。
fn reconnect_message(label: &str, reason: Option<&ReconnectReason>, retry_ms: u64) -> String {
    match reason {
        Some(ReconnectReason::Lagged { skipped }) => {
            format!("{label} 服务端终止流（lagged，丢弃 {skipped} 事件），{retry_ms}ms 后重连")
        }
        Some(ReconnectReason::StreamTerminated { code }) => {
            format!("{label} 服务端终止流（{code}），{retry_ms}ms 后重连")
        }
        None => format!("{label} 断开，{retry_ms}ms 后重连"),
    }
}

/// 构造回调：把 `qaqh-client` 的权威类型转投为本仓 `RuntimeMsg`。
fn build_handlers(
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    last_open: Arc<std::sync::Mutex<Instant>>,
) -> ClientHandlers {
    ClientHandlers {
        on_batch: {
            let msg_tx = msg_tx.clone();
            // 类型已权威化：信封直达 app 层，不再有「过桥失败 → 丢帧」这条路径。
            // 形状对不上现在会是**编译错误**，而不是运行时的静默丢弃。
            Arc::new(move |batch: qaqh_client::EventBatch| {
                for env in &batch.envelopes {
                    let _ = msg_tx.send(RuntimeMsg::Ringing {
                        env: Box::new(env.clone()),
                    });
                }
            })
        },
        on_status: {
            let last_open = last_open.clone();
            let msg_tx = msg_tx.clone();
            Arc::new(
                move |channel: WireChannel, status: ChannelStatus| match status {
                    ChannelStatus::Open { .. } => {
                        note_daemon_activity(&last_open);
                        // 这条流重连成功 → 只撤它自己的告警（否则 `ReadyWithIssue`
                        // 会一直挂在状态栏上，直到下一次 daemon 重启）。
                        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamRecovered {
                            stream: StreamKey::Channel(channel),
                        }));
                    }
                    ChannelStatus::Reconnecting {
                        retry_ms, reason, ..
                    } => {
                        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                            stream: StreamKey::Channel(channel),
                            error: reconnect_message("连接", reason.as_ref(), retry_ms),
                        }));
                    }
                    ChannelStatus::Closed { reason } => {
                        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                            stream: StreamKey::Channel(channel),
                            error: format!("流已关闭：{reason}"),
                        }));
                    }
                    ChannelStatus::Connecting => {}
                },
            )
        },
        on_liveness: {
            // 唯一的存活信号源：客户端每从 socket 读到一块字节就调一次，
            // **包括**解不出内容的 keepalive 注释行——空闲期只有它在说话。
            let last_open = last_open.clone();
            Arc::new(move || note_daemon_activity(&last_open))
        },
        on_reset: {
            let msg_tx = msg_tx.clone();
            Some(Arc::new(
                // 类型已权威化：直达，不再过桥。
                move |reset: qaqh_client::ResetRequired| {
                    let _ = msg_tx.send(RuntimeMsg::ResetRequired { seed: reset.seed });
                },
            ))
        },
        on_timeline_entry: {
            let msg_tx = msg_tx.clone();
            // 类型已权威化：不再过桥，回调给的就是 `qaqh_client` 的类型。
            Arc::new(move |seed: String, entry: qaqh_client::TimelineEntry| {
                let _ = msg_tx.send(RuntimeMsg::Timeline {
                    seed,
                    entry: Box::new(entry),
                });
            })
        },
        on_timeline_status: {
            let msg_tx = msg_tx.clone();
            Arc::new(move |status: TimelineStatus| match status {
                // 流结束（主动停用 / 客户端关闭）**不等于**会话消失：`reason`
                // 只是流侧描述，故归入 `Other`——app 不会再据此把子代理标 Closed。
                // 「会话真的没了」只能由 `activate_timeline` 的 404 证明。
                TimelineStatus::Closed { seed, reason } => {
                    let _ = msg_tx.send(RuntimeMsg::TimelineLost {
                        seed: seed.clone(),
                        reason: TimelineLostReason::Other(format!("timeline 流结束：{reason}")),
                    });
                    // 这条 timeline 流到此为止（会话被 GC / 停止跟踪），不会再发
                    // `Open`：若不在这里撤掉它的告警，那条告警会永久留在账本里。
                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamRecovered {
                        stream: StreamKey::Timeline(seed),
                    }));
                }
                TimelineStatus::Reconnecting {
                    seed,
                    retry_ms,
                    reason,
                    ..
                } => {
                    let label = format!("timeline[{seed}]");
                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                        stream: StreamKey::Timeline(seed),
                        error: reconnect_message(&label, reason.as_ref(), retry_ms),
                    }));
                }
                TimelineStatus::Open { seed, .. } => {
                    // timeline 重连/重定基成功：撤掉它自己的告警。
                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamRecovered {
                        stream: StreamKey::Timeline(seed),
                    }));
                }
                _ => {}
            })
        },
        on_timeline_snapshot: {
            let msg_tx = msg_tx.clone();
            Arc::new(move |page: qaqh_client::TimelinePage| {
                let seed = page.seed.clone();
                let _ = msg_tx.send(RuntimeMsg::TimelineRebaseline {
                    seed,
                    page: Box::new(page),
                });
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 驱动 `build_handlers` 的回调（不需要真 client：`ClientHandlers` 的字段是
    /// 可调用的 `Arc<dyn Fn>`），收集它们投递出去的运行时报文。
    fn handlers_and_rx() -> (ClientHandlers, mpsc::UnboundedReceiver<RuntimeMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let last_open = Arc::new(std::sync::Mutex::new(Instant::now()));
        (build_handlers(tx, last_open), rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<RuntimeMsg>) -> Vec<RuntimeMsg> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    fn is_issue(msg: &RuntimeMsg, key: &StreamKey) -> bool {
        matches!(msg, RuntimeMsg::Conn(ConnEvent::StreamIssue { stream, .. }) if stream == key)
    }

    fn is_recovered(msg: &RuntimeMsg, key: &StreamKey) -> bool {
        matches!(msg, RuntimeMsg::Conn(ConnEvent::StreamRecovered { stream }) if stream == key)
    }

    /// 阻断项 1 的运行时半边：`Open` 只能为**它自己**那条流发恢复信号。
    ///
    /// 证伪方式：把 `on_status` 改回「任意 channel 的 Open 都发一个无身份的
    /// `StreamRecovered`」（本次审查指出的旧写法）——下面「control 不得被别人的
    /// Open 恢复」与「恢复信号必须带 conversation 身份」两条断言同时变红。
    #[test]
    fn channel_open_recovers_only_its_own_stream() {
        let (handlers, mut rx) = handlers_and_rx();
        let control = StreamKey::Channel(WireChannel::Control);
        let conversation = StreamKey::Channel(WireChannel::Conversation);

        (handlers.on_status)(
            WireChannel::Control,
            ChannelStatus::Reconnecting {
                retry_ms: 500,
                last_cursor: 3,
                reason: None,
            },
        );
        (handlers.on_status)(
            WireChannel::Conversation,
            ChannelStatus::Open {
                server_epoch: "e1".into(),
                cursor: 9,
            },
        );

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| is_issue(m, &control)),
            "control 的重连必须只标 control：{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| is_recovered(m, &conversation)),
            "conversation 的 Open 必须带 conversation 身份：{msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| is_recovered(m, &control)),
            "control 仍在重连，不得被别人的 Open 当成已恢复：{msgs:?}"
        );
    }

    /// U-07：qaqh-client 已把服务端终止帧归一为结构化原因，TUI 必须把
    /// `lagged` / 稳定 code 带到用户可见的诊断文案里。
    #[test]
    fn reconnect_message_exposes_structured_reason() {
        let lagged = reconnect_message("连接", Some(&ReconnectReason::Lagged { skipped: 7 }), 500);
        assert!(lagged.contains("lagged"), "{lagged}");
        assert!(lagged.contains('7'), "{lagged}");

        let terminated = reconnect_message(
            "timeline[A]",
            Some(&ReconnectReason::StreamTerminated {
                code: "protocol_version".into(),
            }),
            800,
        );
        assert!(terminated.contains("protocol_version"), "{terminated}");
    }

    /// timeline 流的告警/恢复也必须按 seed 记账；被关闭（不再重连）的流要撤销告警，
    /// 否则那条告警会永久留在 app 的账本里。
    #[test]
    fn timeline_events_are_scoped_to_their_seed() {
        let (handlers, mut rx) = handlers_and_rx();
        let a = StreamKey::Timeline("A".into());
        let b = StreamKey::Timeline("B".into());

        (handlers.on_timeline_status)(TimelineStatus::Reconnecting {
            seed: "A".into(),
            retry_ms: 800,
            cursor: 1,
            reason: None,
        });
        (handlers.on_timeline_status)(TimelineStatus::Open {
            seed: "A".into(),
            server_epoch: "e1".into(),
            cursor: 2,
        });
        (handlers.on_timeline_status)(TimelineStatus::Closed {
            seed: "B".into(),
            reason: "stopped".into(),
        });

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| is_issue(m, &a)),
            "A 的重连只标 A：{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| is_recovered(m, &a)),
            "A 重连成功必须撤掉 A 自己的告警：{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                RuntimeMsg::TimelineLost { seed, .. } if seed == "B"
            )),
            "B 关闭仍要报 TimelineLost：{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| is_recovered(m, &b)),
            "B 已停止跟踪、不会再发 Open，必须在这里撤销它的告警：{msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| is_recovered(m, &StreamKey::Channel(WireChannel::Tool))),
            "timeline 事件不得影响频道流的账本：{msgs:?}"
        );
    }
}
