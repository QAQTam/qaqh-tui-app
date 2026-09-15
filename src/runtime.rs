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
//! 3. 订阅 `session_ctx`（epoch/client_session_id）向 app 报告连接相位，并在
//!    epoch 变化时同步服务面（`HttpClient`）的凭据。
//!
//! 行为契约由 `qaqh-client` 自己的测试锁定；本文件不再重复实现它们。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use qaqh_client::{
    Channel as WireChannel, ChannelStatus, Client, ClientError, ClientHandlers, ClientOptions,
    TimelineStatus,
};
use tokio::sync::mpsc;

use crate::transport::http::HttpClient;
use qaqh_client::RingingEventEnvelope;
use qaqh_client::{TimelineEntry, TimelinePage};

/// timeline 翻页窗口（`request_rebaseline` / `load_older` 使用）。
pub const TIMELINE_PAGE_LIMIT: u32 = 60;

/// 「与 daemon 失联」的判据：这么久没有任何频道处于 `Open`。
///
/// 对齐 winui 的 `compute_stall`（同样 15s）。`qaqh-client` 不会因为失败而
/// 停止生命周期，所以**没有**致命错误信号可等——失联只能靠「一直连不上」推断。
const STALL_AFTER: Duration = Duration::from_secs(15);
const STALL_TICK: Duration = Duration::from_secs(1);

/// timeline 激活时等 attach 落地的重试窗口（见 `activate_with_attach_retry`）。
const ATTACH_RETRY_INTERVAL: Duration = Duration::from_millis(400);
const ATTACH_RETRY_ATTEMPTS: u32 = 75; // ≈30s

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
    /// 非致命问题提示（流断开重连中、事件过桥失败等）。
    StreamIssue { error: String },
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
        error: String,
    },
}

/// 生命周期属主。持有当前 `Client`（可在 T-03 手动重连时原地替换）。
pub struct Runtime {
    client: RwLock<Arc<Client>>,
    msg_tx: mpsc::UnboundedSender<RuntimeMsg>,
    /// app 当前跟踪的 seed 集（重连后据此恢复 timeline 流）。
    tracked: std::sync::Mutex<HashSet<String>>,
    /// 重建串行化：并发重建会得到两个 Client、两套 SSE 流。
    rebuilding: AtomicBool,
    /// 每次重建递增；旧订阅者据此退出（防止旧 session 的回调继续投递）。
    generation: AtomicU64,
    launch_daemon_if_missing: bool,
    /// 服务面客户端：epoch 变化时同步它的端点/token/session-id。
    ///
    /// 晚期绑定：协商成功前 daemon 的端口/token 还不知道（甚至 daemon 都还没
    /// 被拉起），所以由调用方在连接后经 [`Self::attach_service_client`] 注入。
    http: RwLock<Option<Arc<HttpClient>>>,
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
            client: RwLock::new(client.clone()),
            msg_tx: msg_tx.clone(),
            tracked: std::sync::Mutex::new(HashSet::new()),
            rebuilding: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            launch_daemon_if_missing,
            http: RwLock::new(None),
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

    /// 注入服务面客户端（阶段 1.5 之前它仍由本仓承担）。注入即刻对齐一次凭据。
    pub async fn attach_service_client(&self, http: Arc<HttpClient>) {
        // 先取出 Client 的 owned 快照再 await：避免把 `&self` 带进 future。
        let session_id = self
            .client()
            .session_state()
            .await
            .map(|s| s.client_session_id);
        if let Some(session_id) = session_id {
            sync_service_credentials(&http, &session_id);
        }
        *self.http.write().expect("http lock") = Some(http);
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
        self.client.read().expect("client lock").clone()
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
        let msg_tx = self.msg_tx.clone();
        let generation = self.generation.load(Ordering::SeqCst);
        let client = self.client();
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
        self.client().close();
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
        let old = self.client();
        old.close();
        // 让旧任务先退出，避免两套流短暂并存。
        tokio::time::sleep(Duration::from_millis(80)).await;

        let new = Self::connect(&self.msg_tx, &self.last_open, self.launch_daemon_if_missing)
            .await
            .map_err(|e| e.to_string())?;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.client.write().expect("client lock") = new.clone();
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
                    error: e.to_string(),
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
            Some((epoch, client_session_id)) => {
                let epoch_changed = known_epoch.as_deref().is_some_and(|e| e != epoch);
                known_epoch = Some(epoch.clone());

                // 服务面的双头之一是本 session id：重新协商/重建后必须跟上，
                // 否则 service() 会拿着旧 cs 一直 401。端点与 token 同理
                // （daemon 重启会换掉两者）。
                if let Some(http) = runtime.http.read().expect("http lock").clone() {
                    sync_service_credentials(&http, &client_session_id);
                }

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

/// 把 daemon 的当前凭据同步给服务面客户端（`HttpClient`）。
///
/// `Client` 自己会在续租失败时重读 `daemon.json`；服务面是另一条独立路径
/// （阶段 1.5 之前它仍由本仓的 `HttpClient` 承担），必须显式跟着换。
fn sync_service_credentials(http: &HttpClient, client_session_id: &str) {
    if let Ok(discovery) = qaqh_client::read_discovery()
        && let Ok(base_url) = qaqh_client::DiscoveryExt::base_url(&discovery)
        && base_url != http.base_url()
    {
        http.apply_discovery(&base_url, &discovery.token);
    }
    http.set_session_id(client_session_id.to_string());
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
                move |_channel: WireChannel, status: ChannelStatus| match status {
                    ChannelStatus::Open { .. } => {
                        *last_open.lock().expect("last_open lock") = Instant::now();
                    }
                    ChannelStatus::Reconnecting { retry_ms, .. } => {
                        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                            error: format!("连接断开，{retry_ms}ms 后重连"),
                        }));
                    }
                    ChannelStatus::Closed { reason } => {
                        let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                            error: format!("流已关闭：{reason}"),
                        }));
                    }
                    ChannelStatus::Connecting => {}
                },
            )
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
                // 子代理会话被 GC 后消失：旧实现在快照 404 时停止并静默收口，
                // 这里保留等价的可观测信号（app 侧对子代理 seed 静默处理）。
                TimelineStatus::Closed { seed, reason } => {
                    let _ = msg_tx.send(RuntimeMsg::TimelineLost {
                        seed,
                        error: format!("timeline 流结束：{reason}"),
                    });
                }
                TimelineStatus::Reconnecting { seed, retry_ms, .. } => {
                    let _ = msg_tx.send(RuntimeMsg::Conn(ConnEvent::StreamIssue {
                        error: format!("timeline[{seed}] 断开，{retry_ms}ms 后重连"),
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
