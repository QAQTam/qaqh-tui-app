//! App 状态机：输入路由、wire 事件消费、命令发送、覆盖层。
//!
//! 事件驱动（无轮询泵）：所有后端状态变化经 runtime 消息到达后立即生效，
//! UI 在同一帧内重绘。

pub(crate) mod anim;
mod composer_ops;
pub(crate) mod export;
mod interaction;
pub(crate) mod keymap;
pub mod markdown;
mod overlay_ops;
pub(crate) mod pager;
mod paste_guard;
pub(crate) mod render;
pub mod render_line;
pub mod render_transcript;
pub(crate) mod ringing_v2;
pub mod session;
mod session_ops;
pub mod settings;
mod settings_ops;
pub mod slash;
pub mod subagent;
pub mod timeline_model;
mod transcript_ops;

use self::keymap::{GlobalKey, ModalRoute};
use self::paste_guard::PasteGuard;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::app::slash::SlashCmd;
use crate::protocol::ConfigDto;
use crate::runtime::{ConnEvent, Runtime, RuntimeMsg, StreamKey, TimelineLostReason};
use qaqh_client::TimelinePage;
use qaqh_client::{ActionRequest, QueryRequest};
use qaqh_client::{ContentRef, DomainActivityState as ActivityState, NoticeLevel, PermissionRisk};
use qaqh_client::{
    ControlCommand, ConversationCommand, ConversationInputPurpose, RingingCommand, ToolCommand,
};
use qaqh_client::{RingingCommandState as CommandState, RingingCommandStatus};
use qaqh_client::{SessionActivity, SessionListEntry};
use session::{
    AskPanel, Composer, PlanPanel, SessionState, StreamPhase, activity_from_v2,
    conversation_cache_from_v2, streaming_done, sync_streaming_from_timeline,
};

/// `ensure_render_caches` 里「refresh → 用刷新后的总行数重算视口顶端」的迭代上限。
///
/// ⚠ 与 `render::MAX_LAYOUT_PASSES` **数值相同但语义不同**（那个是「布局不动点」的轨数，
/// 本值是「视口不动点」的轨数），分居两文件：**改一个不必改另一个**，也不要当成同一个参数一起调。
///
/// 为什么要迭代：`refresh` 会把估算高度换成精确高度 ⇒ **总行数会在 refresh 中改变**，
/// 用刷新前的 total 算出的 `top` 与 `draw` 用刷新后的 total 算出的 `top` 不一致，
/// 窗口就会落到未渲染块上（issue #33）。实测首帧 2~3 趟收敛；上限只是防呆，
/// 即使超出，`TranscriptCache::viewport` 仍保证 `draw` 取到**同一份**几何——
/// 不变量不会破，最多滚动位置晚一帧收敛。
const VIEWPORT_FIXPOINT_PASSES: usize = 4;

/// 保留 timeline 模型的最近焦点标签数（LRU；超出者仅存轻状态，
/// 重新聚焦时 re-baseline 重建 transcript）。对照 opencode sync 的
/// "进入会话全量重取 + 滑动窗口" 策略。
const ACTIVE_MODELS: usize = 4;
// 单会话内存回合**不设硬上限**。
//
// 历史上这里有个 `TURNS_CAP = 400` 的计数切片。删除它的理由：
//
// 1. **内存边界已由两层机制提供**：
//    - 后端：seal 后 offload 壳化（正文截 512、`tool.output`/`diff` 清空、
//      全文落 `ringing-offload` sidecar）。实测 daemon 快照 5e34cca4：
//      102 回合 / 71 已 offload，每回合 140KB → 10KB（14×）。
//    - 前端：`SegmentCache` 只保留视口附近段的渲染结果，其余退化为估算高度。
// 2. **计数切片会丢数据且违反「丢弃必须可见」**：整回合消失，比后端 offload
//    的「保留 512 字符预览 + 全文可回取」差。
// 3. **它是性能悬崖**：`cap_turns` 每回合从头部丢 1 个，配合旧的
//    `turn_idx` 编号会让每个回合的缓存键都失效（实测丢 1 个 → 19/19 全量重渲）。
//    现已改用稳定编号 `turn_number()` 消解；`cap_turns` 函数保留作兜底能力。

/// app 后台任务回传的结果。
// 大变体承载完整协议响应；Box 化属性能优化，推迟到独立任务（不影响正确性）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ActionResult {
    Bootstrap {
        seed: String,
        result: Result<qaqh_client::ClientV2Bootstrap, String>,
    },
    /// v2 ask/plan 交互正文（content store 取回后按 body 构造挂起面板）。
    InteractionBody {
        seed: String,
        interaction_id: String,
        result: Result<Vec<u8>, String>,
    },
    CommandAck {
        seed: Option<String>,
        label: &'static str,
        result: Result<qaqh_client::RingingCommandAck, String>,
    },
    SessionList(Result<Vec<SessionListEntry>, String>),
    SessionActivity(Result<Vec<SessionActivity>, String>),
    ConfigLoaded(Result<serde_json::Value, String>),
    ConfigWrite {
        label: &'static str,
        result: Result<serde_json::Value, String>,
    },
    Uploaded {
        seed: String,
        path: String,
        result: Result<ContentRef, String>,
    },
    Rebaseline {
        seed: String,
        result: Result<TimelinePage, String>,
    },
    LoadOlder {
        seed: String,
        result: Result<TimelinePage, String>,
    },
    Receipt {
        label: &'static str,
        seed: Option<String>,
        result: Result<RingingCommandStatus, String>,
    },
    Dashboard {
        seed: String,
        result: Result<qaqh_client::DomainDashboardSnapshot, String>,
    },
}

// 小变体（Key/Mouse/Tick）与大负载变体混排；Box 化推迟到独立性能任务。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AppMsg {
    Runtime(RuntimeMsg),
    Action(ActionResult),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize,
    Tick,
}

/// 后台任务取用的 API 句柄。
///
/// T-01 阶段 1.5 后本仓**不再持有自己的 HTTP 客户端**——连接生命周期、三条
/// 频道流、per-seed timeline 流、命令面（`send_command`）与服务面
/// （`query`/`action`）全由 `qaqh-client` 一处承担，凭据热更新因此也只有一份。
#[derive(Clone)]
pub struct ApiCtx {
    /// `None` = 没有连接（只有 [`crate::runtime::Runtime::stub_for_test`] 的测试
    /// 替身会这样）。任务照常运行，取用连接时立刻返回 `Err`，而不是去碰 daemon。
    pub client: Option<Arc<qaqh_client::Client>>,
}

/// 交互应答（permission / ask / plan）的 ack 上限。
///
/// **为什么单列这一类**：这三个命令会**乐观下架 modal**（`respond_*` 先把面板置空
/// 再发命令），所以 ack 永不返回时用户既看不到 modal、也看不到任何反馈——界面看起来
/// 「什么都没发生」。daemon 是同机进程，正常应答在毫秒级，超过这个上限就是故障，
/// 必须可见（B1：丢弃必须可见）。
///
/// **为什么不加在 `spawn_api` 上**：其余命令（compact / undo / session.delete …）
/// 可能合法地长耗时，一刀切会误伤。`spawn_api` 的注释早写了「今后如需统一超时……
/// 叠加在此处」，这里刻意**不**走那条路。
///
/// 后端对应钩子：`QAQH_TEST_INTERACTION_FAULT=permission-hang|ask-hang`
/// （见后端 `docs/spec/2026-09-23-TUI契约测试钩子-spec.md` §1.2）。
pub(crate) const INTERACTION_ACK_TIMEOUT: Duration = Duration::from_secs(10);

impl ApiCtx {
    /// 取出连接；没有连接时给出可读的错误（测试替身路径）。
    fn client(&self) -> Result<&Arc<qaqh_client::Client>, String> {
        self.client
            .as_ref()
            .ok_or_else(|| "无连接（测试替身）".to_string())
    }

    /// 发送 Ringing 命令（返回权威类型的 ack）。
    ///
    /// T-01 阶段二后本仓不再持有协议镜像，故这里不再过桥——命令与回执都是
    /// `qaqh-client` 的类型。此前那层 serde 往返的存在意义（「镜像漂移会静默
    /// 丢帧」）随之消失：现在形状对不上会直接是**编译错误**。
    pub async fn send_command(
        &self,
        seed: Option<&str>,
        command: qaqh_client::RingingCommand,
        options: qaqh_client::CommandOptions,
    ) -> Result<qaqh_client::RingingCommandAck, String> {
        self.client()?
            .send_command(seed, command, options)
            .await
            .map_err(|e| e.to_string())
    }

    /// 交互应答专用：与 [`ApiCtx::send_command`] 同款，但等待命令进入终态，
    /// 并带 [`INTERACTION_ACK_TIMEOUT`] 上限。
    ///
    /// v2 的 HTTP ack 只表示命令已受理；业务完成必须轮询 receipt。若只等 ack，
    /// daemon 卡在 worker 时 UI 永远不会给出超时反馈（ask-hang 的原始回归）。
    pub async fn send_interaction_command(
        &self,
        seed: Option<&str>,
        command: qaqh_client::RingingCommand,
    ) -> Result<qaqh_client::RingingCommandAck, String> {
        let command_id = uuid::Uuid::new_v4().to_string();
        let options = qaqh_client::CommandOptions {
            command_id: Some(command_id.clone()),
            ..Default::default()
        };
        match tokio::time::timeout(INTERACTION_ACK_TIMEOUT, async {
            let ack = self.send_command(seed, command, options).await?;
            if ack.status == qaqh_client::RingingCommandAckStatus::Rejected {
                return Ok(ack);
            }
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Ok(status) = self.command_status(&command_id).await else {
                    continue;
                };
                match status.state {
                    CommandState::Succeeded => return Ok(ack.clone()),
                    CommandState::Failed | CommandState::Rejected => {
                        let detail = status.error_code.as_deref().unwrap_or("command_failed");
                        return Err(format!("命令未成功：{detail}"));
                    }
                    CommandState::Accepted | CommandState::Running => {}
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "应答超时（{}s 未收到 daemon 确认）",
                INTERACTION_ACK_TIMEOUT.as_secs()
            )),
        }
    }

    /// 拉取 timeline 快照页（纯读，不重建流）。
    ///
    /// `before_index` = 排他游标（全局回合序号），`None` = 最新一页。游标由
    /// `TimelineTurn::turn_index` 提供，**不是** `turn_id`——后者会被 worker 复用
    /// （见后端 `TimelineAppender::open_turn` 的 reopen 注释），当不了稳定游标。
    pub async fn timeline_page(
        &self,
        seed: &str,
        before_index: Option<u64>,
        limit: u32,
    ) -> Result<qaqh_client::TimelinePage, String> {
        // 类型已权威化：不再有过桥这一步，返回的就是 `qaqh_client` 的类型。
        self.client()?
            .fetch_timeline_page(seed, before_index, Some(limit))
            .await
            .map_err(|e| e.to_string())
    }

    /// 会话 bootstrap（v2 三频道 typed 快照原子恢复）。
    pub async fn bootstrap(&self, seed: &str) -> Result<qaqh_client::ClientV2Bootstrap, String> {
        let client = self.client()?;
        // 新建会话在首个 canonical 事实落盘前，v2 快照端点是 404/409（会话尚未
        // 物化）——这是**瞬态**，短退避重试而不是立刻报错。
        let mut last: Option<String> = None;
        for _ in 0..120 {
            match client.bootstrap(seed).await {
                Ok(bootstrap) => return Ok(bootstrap),
                Err(error) if error.is_session_not_ready() => {
                    last = Some(error.to_string());
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(format!(
            "bootstrap: 会话未在预期时间内物化（最后一次：{}）",
            last.unwrap_or_default()
        ))
    }

    /// 服务面查询（`session.list` / `session.activity` / `todo.status`…）。
    pub async fn query(&self, request: QueryRequest) -> Result<serde_json::Value, String> {
        self.client()?
            .query(request)
            .await
            .map_err(|e| e.to_string())
    }

    /// 服务面动作（`config.save` / `profile.apply` …）。
    pub async fn action(&self, request: ActionRequest) -> Result<serde_json::Value, String> {
        self.client()?
            .action(request)
            .await
            .map_err(|e| e.to_string())
    }

    /// 命令回执轮询（ACK ≠ 完成）。
    pub async fn command_status(&self, command_id: &str) -> Result<RingingCommandStatus, String> {
        self.client()?
            .command_status(command_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// 上传附件内容（multipart 组装在 client 侧）。
    pub async fn upload_content(
        &self,
        seed: &str,
        media_type: &str,
        data: Vec<u8>,
    ) -> Result<qaqh_client::ContentRef, String> {
        self.client()?
            .upload_content(seed, media_type, data)
            .await
            .map_err(|e| e.to_string())
    }

    /// 取回 content store 里的交互正文（v2 ask/plan）。
    pub async fn download_content_by_id(&self, content_id: &str) -> Result<Vec<u8>, String> {
        self.client()?
            .download_content_by_id(content_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnPhase {
    Opening,
    Ready,
    /// 连接本身还在（session 已协商、可继续收发），但有流报了非致命问题
    /// （断开重连中 / timeline 流结束等）。
    ///
    /// 与 [`ConnPhase::Lost`] 是**两件事**：`Lost` 是「连不上 daemon，需要
    /// 手动重连」，这里是「daemon 还连着，某条流在自愈」。所以它不能复用
    /// `Ready`（告警会被静默丢弃，用户无从得知流有问题），也不能落到 `Lost`
    /// （会误导用户去按 Ctrl+R 重连一条健康的连接）。
    ReadyWithIssue,
    Lost,
}

impl ConnPhase {
    /// 结合「当前是否有流处于告警」重算相位（纯函数，便于回归测试）。
    ///
    /// 判据：
    /// - `Ready` 家族：有告警 → `ReadyWithIssue`，告警清空 → `Ready`。
    /// - `Lost` → 不变：失联的原因比流告警更重要，相位本身也已可见。
    /// - `Opening` → 不变：还没协商出 session，「连接中」已经表达了状态。
    pub fn with_stream_issues(&self, any_issue: bool) -> ConnPhase {
        match self {
            ConnPhase::Ready | ConnPhase::ReadyWithIssue => {
                if any_issue {
                    ConnPhase::ReadyWithIssue
                } else {
                    ConnPhase::Ready
                }
            }
            other => other.clone(),
        }
    }
}

/// 进程启动意图。
///
/// `New` 显示品牌首屏，用户提交后才创建会话；`Resume` 直接进入当前 cwd 的
/// 会话列表，不在首屏做任何隐式会话创建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupIntent {
    New,
    Resume,
}

/// 「当前有哪些流在告警」的账本：流身份 → 最近一条告警文案。
///
/// **按流记账，而不是一个全局布尔**：某条 timeline 流断开的同时另一条频道流恰好
/// 重连成功，不能把前者的告警当成「一切正常」清掉（反向顺序下文案还会串成后者）。
///
/// 同一 `StreamKey` 重复告警只**覆盖**文案并把它挪到最新，不堆积条目。
#[derive(Debug, Clone, Default)]
pub struct StreamIssues {
    /// 最近告警在后的顺序（[`StreamIssues::latest`] 取末位）。
    order: Vec<StreamKey>,
    messages: HashMap<StreamKey, String>,
}

impl StreamIssues {
    /// 记一条告警；返回该流此前是否**不在**告警集合里。
    ///
    /// 返回值是**为可测性保留的观察值**：生产代码不看它（调用点只关心账本状态，
    /// 相位由 [`reconcile_conn`] 统一推导），测试用它断言「同一条流重复告警不算
    /// 新增」。不要把它当成「调用方需要知道是否新增」的契约。
    pub fn raise(&mut self, stream: StreamKey, message: String) -> bool {
        let already = self.messages.contains_key(&stream);
        if already {
            // 同一条流重复告警：只覆盖文案并挪到最新，顺序表里不堆积重复项。
            self.order.retain(|k| k != &stream);
        }
        self.order.push(stream.clone());
        self.messages.insert(stream, message);
        !already
    }

    /// 该流恢复：只移除**它自己**那条告警；返回它此前是否在告警集合里
    /// （同样是为可测性保留的观察值，生产代码不看）。
    pub fn clear(&mut self, stream: &StreamKey) -> bool {
        let existed = self.messages.remove(stream).is_some();
        if existed {
            self.order.retain(|k| k != stream);
        }
        existed
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// 最近一条告警文案（状态栏展示用）。
    pub fn latest(&self) -> Option<&str> {
        let stream = self.order.last()?;
        self.messages.get(stream).map(String::as_str)
    }
}

/// 由「当前有哪些流在告警」重算可见的连接指示（纯函数，便于回归测试）。
///
/// 返回 `(相位, conn_error)`：
/// - 相位由 [`ConnPhase::with_stream_issues`] 给出；
/// - `Lost` 相位下 `conn_error` **原样保留**——那是「为什么失联」，比「某条流在
///   重连」重要，不能被流告警覆盖（失联分支靠它给用户原因）；
/// - 其余相位下 `conn_error` 恒等于账本里最近一条告警（没有告警就是 `None`），
///   所以某条流恢复后不会残留它自己的旧文案。
pub fn reconcile_conn(
    phase: &ConnPhase,
    issues: &StreamIssues,
    conn_error: Option<String>,
) -> (ConnPhase, Option<String>) {
    let phase = phase.with_stream_issues(!issues.is_empty());
    let error = if phase == ConnPhase::Lost {
        conn_error
    } else {
        issues.latest().map(str::to_owned)
    };
    (phase, error)
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub level: NoticeLevel,
    pub text: String,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteSession(String),
    ArchiveSession(String),
    CloseTab(String),
    UndoTurn { seed: String, turn_id: String },
}

// Settings 变体内嵌完整编辑态，尺寸差较大；Box 化推迟到独立性能任务。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Overlay {
    SessionList {
        selected: usize,
        show_archived: bool,
    },
    Settings(settings::SettingsState),
    Help,
    AttachPath {
        input: Vec<char>,
        cursor: usize,
        seed: String,
    },
    Confirm {
        action: ConfirmAction,
    },
    /// 二级：/new 的 cwd 输入（/ 本身的一级菜单为 inline 浮层，非 overlay）
    CwdInput {
        input: Vec<char>,
        cursor: usize,
    },
    /// `/history`：按回合浏览当前会话。
    ///
    /// 数据源是 **app 自己的 timeline 模型**，不是终端 scrollback —— 后者读不回来
    /// （没有标准序列），而且会被终端 evict。`detail` 为真时进入该回合的只读
    /// 详情视图（`selected` 指向 `timeline.turns` 的下标）。
    History {
        selected: usize,
        detail: bool,
        scroll: usize,
    },
    /// 思考回放浮层（§4.5）：当前活动回合的 reasoning body（内存零抓取）。
    /// 只读 + 滚动；Esc 关闭。body 在推入时快照（后续 delta 不刷新——回放语义）。
    Thinking {
        seed: String,
        scroll: usize,
        body: String,
    },
}

impl ConfirmAction {
    /// 该确认动作最终作用的会话 seed。
    pub fn seed(&self) -> &str {
        match self {
            ConfirmAction::DeleteSession(seed)
            | ConfirmAction::ArchiveSession(seed)
            | ConfirmAction::CloseTab(seed) => seed,
            ConfirmAction::UndoTurn { seed, .. } => seed,
        }
    }
}

impl Overlay {
    /// 该 overlay 绑定的会话 seed；`None` = 全局 overlay，与活动标签无关。
    ///
    /// 判据是「它的确认/提交动作会落到哪个 seed 上」，不是「它看起来像不像弹窗」：
    ///
    /// - **seed 绑定**：`Confirm`（三个动作都携带 seed，按 `y` 直接作用于那个
    ///   seed）与 `AttachPath`（附件上传落在某个会话上）。切标签后它们就是过期
    ///   指令——确认类 overlay 绝不能跨标签生效。
    /// - **全局**：`SessionList`（daemon 全局会话列表）、`Settings`（全局配置）、
    ///   `Help`、`CwdInput`（`/new` 的 cwd，此时还没有 seed）。它们与当前标签
    ///   无关，切标签**不该**把它们关掉——一刀切清空会误伤设置页这类全局面板。
    ///
    /// `AttachPath` 的判据与执行必须同源：它的上传目标由
    /// [`Overlay::attach_submit`] 给出，取的就是这里的 `seed`（不是提交那一刻的
    /// 活动标签）。此前 `bound_seed()` 说「属于 seed X」而上传查 `active_seed()`，
    /// 两条路径结论相反，`AttachPath.seed` 沦为死字段。
    pub fn bound_seed(&self) -> Option<&str> {
        match self {
            Overlay::Confirm { action } => Some(action.seed()),
            Overlay::AttachPath { seed, .. } => Some(seed),
            Overlay::Thinking { seed, .. } => Some(seed),
            // History 渲染的是**当前活动会话**的 timeline，自己不持有 seed：
            // 切标签时让它跟着走，不关掉（与 SessionList/Settings 同属全局面）。
            Overlay::SessionList { .. }
            | Overlay::Settings(_)
            | Overlay::Help
            | Overlay::History { .. }
            | Overlay::CwdInput { .. } => None,
        }
    }

    /// `AttachPath` 按 Enter 的提交动作：目标 seed + 去空白后的路径。
    ///
    /// 取 `AttachPath` 的**字段**而不是 `&self`：只有 Enter 分支需要它，且只可能是
    /// `AttachPath`——放在 `overlay_key` 的 `match` 之前会让每次按键（包括输入路径
    /// 的每个字符）都白做一次 `String` 分配，也让「只有 AttachPath 有这个动作」
    /// 这件事变成运行期判断。
    ///
    /// - 目标 seed **恒取 overlay 自己存的那个**——这是附件目标 seed 的单一事实源，
    ///   与 [`Overlay::bound_seed`] 的判据一致（判据说属于谁，执行就落在谁身上）；
    /// - 空路径（或纯空白）→ `None`：不提交。
    pub fn attach_submit(input: &[char], seed: &str) -> Option<AttachSubmit> {
        let path = input.iter().collect::<String>().trim().to_owned();
        (!path.is_empty()).then(|| AttachSubmit {
            target_seed: seed.to_owned(),
            path,
        })
    }
}

/// 附件上传的目标：**只能**由 [`Overlay::attach_submit`] 产生（`overlay_key` 的
/// `AttachPath` Enter 分支是唯一调用点）。
///
/// 这样「上传挂到哪个会话」不可能与 overlay 的身份漂移：上传入口拿不到
/// `active_seed()`，只有一个来源。判据（`bound_seed()`）、剪枝（切标签即关闭）、
/// 执行（用存量 seed 上传）三条路径由此自洽。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSubmit {
    target_seed: String,
    path: String,
}

impl AttachSubmit {
    /// 拆成 `(目标 seed, 路径)`——上传入口只通过它取值。
    pub fn into_parts(self) -> (String, String) {
        (self.target_seed, self.path)
    }
}

/// 清掉「不属于 `seed`」的 seed 绑定 overlay（纯函数，便于回归测试）。
///
/// 全局 overlay（`bound_seed() == None`）原样保留且保持相对顺序；删掉的若是栈顶，
/// 下面那层自然接管——渲染与按键路由都取 `overlays.last()`，两处一致。
pub fn prune_seed_bound_overlays(overlays: &mut Vec<Overlay>, seed: Option<&str>) {
    overlays.retain(|o| match (o.bound_seed(), seed) {
        (None, _) => true,
        (Some(bound), Some(active)) => bound == active,
        (Some(_), None) => false,
    });
}

use self::settings::{FieldKind, SettingsState};

/// 主循环帧统计（`QAQH_TUI_DEBUG=1` 展示）。
///
/// 由 `main.rs` 主循环每秒结算一次：把「wire 事件率」与「终端实际刷新率」
/// 两个数字分开，再把整帧耗时拆成三段——渲染管线 / 终端写入 / 事件处理，
/// 用于定位观感瓶颈到底在应用侧还是终端侧。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// 最近一秒完成的帧数（每帧 = 一次 `terminal.draw`）。
    pub fps: u32,
    /// 最近一秒消费的运行时消息数（`AppMsg::Runtime`；≈ timeline 事件率）。
    pub events_per_s: u32,
    /// 最近一秒的平均整帧耗时（`ref_us + term_us`）。
    pub draw_us: u64,
    /// 其中：`ensure_render_caches`（渲染管线：段对齐 + 物化 + 淘汰）。
    pub ref_us: u64,
    /// 其中：`terminal.draw`（`ui::draw` + ratatui diff + 终端写入/刷新）。
    pub term_us: u64,
    /// 其中：`app.handle`（每帧平均；批量事件合计摊到帧）。
    pub handle_us: u64,
    /// 最近一秒内单次 refresh 的最大重渲块数（⟂ 峰值）。
    pub peak_rebuilt: u32,
}

/// 弹窗里一个**可点目标**。
///
/// 这是"语义目标"，不是坐标：坐标由渲染层按同一套布局算（见
/// `ui::v2::modal::hit_test`），命中后回填到这里，绘制时按它上色。
/// 刻意不含任何 ratatui 类型，保持 app 层不依赖渲染细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalHit {
    /// ask 的第 `question` 题的第 `option` 个选项。
    AskOption {
        question: usize,
        option: usize,
    },
    /// ask 的「自定义输入」项。
    AskCustom {
        question: usize,
    },
    PermissionApprove,
    PermissionDeny,
    PermissionTrust,
    PlanApprove,
    PlanApproveAutonomous,
    PlanReject,
}

/// Workspace 里一个可点目标。
///
/// 与 [`ModalHit`] 一样只保存语义索引，不保存终端坐标；实际坐标由
/// `ui::v2::workspace` 的共享几何函数在绘制与命中测试时各算一次，但两边
/// 使用同一份布局/窗口算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceHit {
    SessionRow(usize),
    HistoryTurn(usize),
    TodoTask(usize),
    SettingsRow(usize),
    Back,
}

pub struct App {
    pub quit: bool,
    pub runtime: Arc<Runtime>,
    pub msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,

    pub tabs: Vec<String>,
    pub sessions: HashMap<String, SessionState>,
    pub active: usize,

    pub overlays: Vec<Overlay>,
    pub conn_phase: ConnPhase,
    pub epoch: String,
    pub conn_error: Option<String>,
    /// 处于告警状态的流账本（相位与 `conn_error` 由它推导，见 [`reconcile_conn`]）。
    pub stream_issues: StreamIssues,

    /// 鼠标当前悬停的弹窗目标（`?1003h` 移动上报；只对弹窗有意义）。
    pub modal_hover: Option<ModalHit>,
    /// 鼠标**按下且未松开**的目标。松开时若仍命中同一目标才提交——
    /// 这是按钮的基本语义（按下后拖出去 = 取消）。
    pub modal_pressed: Option<ModalHit>,
    /// Workspace 鼠标悬停/按下目标；绘制与点击共用语义。
    pub workspace_hover: Option<WorkspaceHit>,
    pub workspace_pressed: Option<WorkspaceHit>,
    /// 本进程已提交的交互 id（ask / plan / permission）。提交后不能再被
    /// bootstrap 快照复活成幽灵面板；超时反馈必须能留在可见的 Agent 状态栏。
    suppressed_interactions: HashSet<String>,

    pub toasts: VecDeque<Toast>,
    /// 新建会话的 command_id → 发起时间（等 causation_id 关联）。
    pub pending_creates: HashMap<String, Instant>,
    pub session_list_cache: Vec<SessionListEntry>,
    pub session_list_at: Option<Instant>,
    /// `qaqh-tui resume` 的当前 cwd 过滤；普通启动为 `None`。
    pub session_cwd_filter: Option<String>,
    /// 无活动会话时的品牌首屏输入框。提交后才创建会话。
    pub draft_composer: Composer,
    /// 品牌首屏提交的首条消息：等 `SessionCreate` 落成后自动发送。
    pub pending_initial_prompt: Option<String>,
    pub startup_intent: StartupIntent,
    pub activity_cache: HashMap<String, ActivityState>,
    dashboard_fetching: HashSet<String>,
    /// config.load 的 typed 快照（ConfigDto 镜像；ConfigChanged 到达时重拉）。
    pub config: Option<ConfigDto>,
    /// settings 保存请求在途标记（防 config.save 双发——事故 R4）。
    pub settings_saving: bool,
    /// F3（M2 重定义）：ActivityBar 显隐（§4.4）。旧「思考链全局展开」语义废止。
    pub show_activity: bool,
    /// 右侧 workspace 面板开关（F4；窄终端自动隐藏）。
    pub show_workspace: bool,
    pub tracked_seeds: HashSet<String>,
    /// 最近焦点顺序（MRU，头 = 最近）。
    pub focus_order: Vec<String>,
    pub last_focused: Option<String>,
    /// todo 详情折叠（F6）。
    pub show_todo_detail: bool,
    pub last_tick: Instant,
    /// 粘贴护栏：按键洪流检测（抑制不支持括号粘贴终端的 Enter 自动发送）。
    pub paste_guard: PasteGuard,
    /// Ctrl+C 二次确认。
    pub quit_armed: Option<Instant>,
    /// 首页会话列表选中与归档显隐（tabs.is_empty() 时生效）。
    pub home_selected: usize,
    pub home_show_archived: bool,
    /// 一级斜杠菜单选中（Tab/↑↓ 循环）
    pub slash_selected: usize,
    /// 启动时的进程 cwd（hybrid 回退 3），捕获后不再随 cd 变化
    pub initial_cwd: Option<String>,
    /// 子代理观测视图栈：当前正在查看的子代理 seed（None = 正常标签视图）。
    pub inspect: Option<String>,
    /// 正在跟踪 timeline 流的子代理 seed 集（attach 后建立，终态后移除）。
    pub(crate) subagent_seeds: HashSet<String>,
    /// M4（T15）：待交给 `$PAGER` 的文本——Ctrl+T 浮层按 `e` 置位；
    /// main.rs 主循环在帧间消费（挂起终端 → 分页器 → 恢复）。
    pub pending_pager: Option<String>,
    /// 主循环帧统计（`QAQH_TUI_DEBUG=1` 时在状态栏展示）。
    pub frame_stats: FrameStats,
    /// 后端回合终态到达时置位；主循环下一帧清 viewport 后强制重绘。
    pub force_redraw: bool,
}

/// `TimelineLost` 的处置结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineLostEffect {
    /// 会话确实不存在（404）：子代理收口为 `Closed` 并停止跟踪。
    CloseSubagent,
    /// 其他错误：**保留原状态**，只给出可见提示。
    Notice,
}

/// `TimelineLost` 的判据（纯函数，便于回归）。
///
/// 只有「seed 属于子代理」**且**「原因确为会话不存在（404）」才收口；任何
/// 网络抖动（401/超时/流结束）都不足以证明会话消失，误判会把仍在运行的子代理
/// 标成 `Closed` 并停掉它的 timeline 流（issue #2 缺陷 1）。
pub(crate) fn timeline_lost_effect(
    is_subagent: bool,
    reason: &TimelineLostReason,
) -> TimelineLostEffect {
    if is_subagent && *reason == TimelineLostReason::SessionMissing {
        TimelineLostEffect::CloseSubagent
    } else {
        TimelineLostEffect::Notice
    }
}

impl App {
    pub fn new(runtime: Arc<Runtime>, msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>) -> Self {
        Self::new_with_cwd(
            runtime,
            msg_tx,
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        )
    }

    /// **测试用构造**：没有连接的 `Runtime` 替身 + 一条可观察的 `AppMsg` 通道。
    ///
    /// 返回 `(App, rx)`：测试从 `rx` 读回后台任务投递的结果（例如
    /// `ActionResult::Uploaded { seed, .. }` 里的归属 seed）。只用于 `App` 层
    /// 按键/事件路径的单测——`overlay_key`、`handle`、`upload_attachment` 这些
    /// 以前只能靠纯函数间接覆盖的路径。
    #[cfg(test)]
    pub fn new_for_test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Self::new_with_cwd(Runtime::stub_for_test(), tx, None);
        (app, rx)
    }

    pub fn new_with_cwd(
        runtime: Arc<Runtime>,
        msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
        initial_cwd: Option<String>,
    ) -> Self {
        Self {
            quit: false,
            runtime,
            msg_tx,
            tabs: Vec::new(),
            sessions: HashMap::new(),
            active: 0,
            overlays: Vec::new(),
            conn_phase: ConnPhase::Opening,
            epoch: String::new(),
            conn_error: None,
            stream_issues: StreamIssues::default(),
            modal_hover: None,
            modal_pressed: None,
            workspace_hover: None,
            workspace_pressed: None,
            suppressed_interactions: HashSet::new(),
            toasts: VecDeque::new(),
            pending_creates: HashMap::new(),
            session_list_cache: Vec::new(),
            session_list_at: None,
            session_cwd_filter: None,
            draft_composer: Composer::default(),
            pending_initial_prompt: None,
            startup_intent: StartupIntent::New,
            activity_cache: HashMap::new(),
            dashboard_fetching: HashSet::new(),
            config: None,
            settings_saving: false,
            show_activity: true,
            show_workspace: true,
            tracked_seeds: HashSet::new(),
            focus_order: Vec::new(),
            last_focused: None,
            show_todo_detail: true,
            last_tick: Instant::now(),
            paste_guard: PasteGuard::default(),
            quit_armed: None,
            home_selected: 0,
            home_show_archived: false,
            slash_selected: 0,
            initial_cwd,
            inspect: None,
            subagent_seeds: HashSet::new(),
            pending_pager: None,
            frame_stats: FrameStats::default(),
            force_redraw: false,
        }
    }

    // ───────────────────────── 消息入口 ─────────────────────────

    pub fn handle(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Runtime(m) => self.handle_runtime(m),
            AppMsg::Action(a) => self.handle_action(a),
            AppMsg::Key(k) => self.handle_key(k),
            AppMsg::Mouse(m) => self.handle_mouse(m),
            AppMsg::Paste(text) => self.handle_paste(text),
            AppMsg::Resize => {
                // 宽度变化 → 渲染缓存全部失效。
                for s in self.sessions.values_mut() {
                    s.block_cache = None;
                }
            }
            AppMsg::Tick => self.handle_tick(),
        }
    }

    fn handle_tick(&mut self) {
        self.last_tick = Instant::now();
        // 过期 toast / 未命中的 create 关联。
        while let Some(front) = self.toasts.front() {
            if front.at.elapsed() > Duration::from_secs(6) {
                self.toasts.pop_front();
            } else {
                break;
            }
        }
        self.pending_creates
            .retain(|_, at| at.elapsed() < Duration::from_secs(15));
        if let Some(armed) = self.quit_armed
            && armed.elapsed() > Duration::from_secs(3)
        {
            self.quit_armed = None;
        }
        // 首页自动刷新：无 tab 时保持列表新鲜（对齐 opencode Home 的常驻列表感）。
        // 有在途 create 时**同样**刷新：新会话的落地结果靠列表兜底发现，
        // 而那时用户可能已经有别的 tab 开着（`tabs` 非空）。
        if self.tabs.is_empty() || !self.pending_creates.is_empty() {
            let stale = self
                .session_list_at
                .map(|t| t.elapsed() > Duration::from_secs(3))
                .unwrap_or(true);
            if stale {
                self.fetch_session_list();
            }
            // 选中越界时回绕
            let count = self.filtered_sessions(self.home_show_archived).len();
            if count > 0 && self.home_selected >= count {
                self.home_selected = count - 1;
            }
        }
    }

    fn handle_mouse(&mut self, m: MouseEvent) {
        use ratatui::crossterm::event::MouseEventKind;
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            MouseEventKind::Down(kind)
                if kind == ratatui::crossterm::event::MouseButton::Left && m.row == 0 =>
            {
                self.click_tab(m.column);
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: String) {
        // 覆盖层输入框优先。
        if let Some(Overlay::AttachPath { input, .. }) = self.overlays.last_mut() {
            for ch in text.chars() {
                if ch != '\n' && ch != '\r' {
                    input.push(ch);
                }
            }
            return;
        }
        // 设置页编辑态：粘贴进当前字段缓冲。
        if let Some(Overlay::Settings(st)) = self.overlays.last_mut()
            && let Some(buf) = st.editing.as_mut()
        {
            for ch in text.chars() {
                if ch != '\n' && ch != '\r' {
                    buf.buf.insert(buf.cursor.min(buf.buf.len()), ch);
                    buf.cursor += 1;
                }
            }
            return;
        }
        if self.overlays.is_empty() && self.tabs.is_empty() {
            self.draft_composer.insert_str(&text);
            return;
        }
        if self.inspecting() {
            // 观测模式只读：粘贴不落到隐藏的父会话 composer。
            return;
        }
        let Some(sess) = self.active_session_mut() else {
            return;
        };
        if let Some(panel) = sess.pending_ask.as_mut()
            && panel.editing_custom.is_some()
        {
            panel.input.push_str(&text);
            return;
        }
        if let Some(panel) = sess.pending_plan.as_mut()
            && panel.entering_message
        {
            panel.message.push_str(&text);
            return;
        }
        sess.composer.insert_str(&text);
    }

    fn handle_runtime(&mut self, msg: RuntimeMsg) {
        match msg {
            RuntimeMsg::Conn(ev) => self.handle_conn(ev),
            RuntimeMsg::V2Event { seed, event } => self.handle_v2_event(seed, *event),
            RuntimeMsg::V2StreamOpen { seed } => {
                // 规范顺序 open -> bootstrap -> subscribe：流只从 bootstrap 的
                // snapshot cursor 起放 replay，快照本身不进流。故每次流建立/重连
                // 都重新 bootstrap，补齐「流未连上期间」写入的交互/状态。
                self.spawn_bootstrap(seed);
            }
            RuntimeMsg::ResetRequired { seed } => {
                // 频道级 reset → 重新 bootstrap 该会话（timeline 流自会 re-baseline）。
                self.spawn_bootstrap(seed);
            }
            RuntimeMsg::Timeline { seed, entry } => {
                // 子代理发现：spawn_subagent 工具卡（增量，先于 apply 检查）。
                let todo_tool_touched = matches!(
                    &entry.event,
                    qaqh_client::TimelineEvent::ToolUpdated { tool, .. }
                        if tool.name.starts_with("todo")
                );
                if let qaqh_client::TimelineEvent::ToolUpdated { tool, .. } = &entry.event {
                    self.discover_spawn_tool(&seed, tool);
                }
                let Some(sess) = self.sessions.get_mut(&seed) else {
                    return;
                };
                // TurnSealed 是 timeline 上的权威终态：立即收口 streaming，不等
                // 对话频道 TurnCompleted（两条独立 SSE，可能乱序或丢失）。
                let turn_sealed =
                    matches!(&entry.event, qaqh_client::TimelineEvent::TurnSealed { .. });
                if let Some(terminal) = sess.timeline.apply(&entry) {
                    streaming_done(sess, Some(&terminal.turn_id));
                }
                sync_streaming_from_timeline(sess);
                if !sess.scroll.follow {
                    // 非跟随模式：内容增长等价于视口上移。
                    sess.scroll.offset = sess.scroll.offset.saturating_add(0);
                }
                if turn_sealed {
                    self.force_redraw = true;
                }
                if todo_tool_touched {
                    // v2 投影没有 dashboard 增量；todo 工具的 timeline 更新是
                    // 前端可依赖的刷新触发点。
                    self.fetch_dashboard(seed);
                }
            }
            RuntimeMsg::TimelineRebaseline { seed, page } => {
                let Some(sess) = self.sessions.get_mut(&seed) else {
                    return;
                };
                // 重基线 = 权威时间线已就绪：压缩动画兜底清除。
                sess.compact_anim = None;
                let was_follow = sess.scroll.follow;
                let first_load = !sess.ready;
                sess.needs_rebaseline = false;
                sess.timeline.replace_from_page(&page);
                sess.ready = true;
                sync_streaming_from_timeline(sess);
                if first_load || was_follow {
                    sess.scroll.follow = true;
                    sess.scroll.offset = 0;
                }
                sess.block_cache = None;
                // 子代理：重扫工具卡 + 终态兑底推导。
                self.handle_subagent_rebaseline(&seed);
            }
            RuntimeMsg::TimelineLost { seed, reason } => self.handle_timeline_lost(&seed, &reason),
        }
    }

    /// timeline 流丢失：只有 404（会话不存在）才收口子代理，其余保留现状并提示。
    fn handle_timeline_lost(&mut self, seed: &str, reason: &TimelineLostReason) {
        match timeline_lost_effect(self.subagent_seeds.contains(seed), reason) {
            TimelineLostEffect::CloseSubagent => {
                // 子代理会话已消失（404）：静默标记关闭，不再重试。
                self.mark_subagent_closed(seed);
            }
            TimelineLostEffect::Notice => {
                // 401/超时/网络错/流结束：会话可能还活着——状态不动，但必须可见。
                self.toast(
                    NoticeLevel::Error,
                    format!("timeline 断开[{seed}]: {}", reason.describe()),
                );
            }
        }
    }

    /// **手动重连**（T-03）：关掉当前客户端并重建一个。
    ///
    /// 只在失联相位可用——`qaqh-client` 自己会为租约过期/daemon 重启重连，
    /// 健康的连接上按这个键只会把好连接推倒重来。重建会重读 `daemon.json`
    /// （含 pid 判活），所以 daemon 换了端口/token 也能恢复；成功后的相位由
    /// 运行时推 `Ready` 回来（并触发一次全量 attach + bootstrap）。
    fn request_reconnect(&mut self) {
        if self.conn_phase != ConnPhase::Lost {
            self.toast(NoticeLevel::Info, "连接正常，无需重连");
            return;
        }
        let runtime = self.runtime.clone();
        let tx = self.msg_tx.clone();
        self.toast(NoticeLevel::Info, "正在重连 daemon…");
        tokio::spawn(async move {
            if let Err(e) = runtime.rebuild().await {
                // 失败就留在 Lost 并把原因显示出来（状态栏会渲染 conn_error）。
                let _ = tx.send(AppMsg::Runtime(RuntimeMsg::Conn(ConnEvent::Lost(format!(
                    "重连失败：{e}"
                )))));
            }
        });
    }

    fn handle_conn(&mut self, ev: ConnEvent) {
        match ev {
            ConnEvent::Opening => {
                self.conn_phase = ConnPhase::Opening;
            }
            ConnEvent::Ready {
                epoch,
                epoch_changed,
            } => {
                self.conn_phase = ConnPhase::Ready;
                if self.epoch != epoch {
                    self.epoch = epoch.clone();
                }
                self.conn_error = None;
                // 新 session（首次协商 / 租约重建 / daemon 重启）：所有流都随旧
                // client 作废，告警账本整体清空——否则旧代遗留的 timeline 告警
                // 会永久挂在状态栏上（那条流再也不会发 `Open` 来撤自己）。
                self.stream_issues = StreamIssues::default();
                // 重 open（租约重建 / daemon 重启）：重新 attach 全部 open seeds
                // 并 re-baseline；epoch 变化时 timeline 流自行重放。
                // 子代理 seed 走 SessionAttach（无 actor 副作用，运行中的子代理
                // 不能被 resume）。
                let seeds = self.tabs.clone();
                let sub_seeds: Vec<String> = self.subagent_seeds.iter().cloned().collect();
                if !seeds.is_empty() || !sub_seeds.is_empty() {
                    self.spawn_api(move |api, tx| async move {
                        for seed in seeds {
                            let attach = api
                                .send_command(
                                    Some(&seed),
                                    RingingCommand::Control(ControlCommand::SessionResume {
                                        seed: seed.clone(),
                                    }),
                                    Default::default(),
                                )
                                .await;
                            if let Err(e) = attach {
                                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                                    seed: Some(seed.clone()),
                                    label: "resume",
                                    result: Err(e),
                                }));
                                continue;
                            }
                            let result = api.bootstrap(&seed).await;
                            let _ =
                                tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
                        }
                        for seed in sub_seeds {
                            let attach = api
                                .send_command(
                                    Some(&seed),
                                    RingingCommand::Control(ControlCommand::SessionAttach {
                                        seed: seed.clone(),
                                    }),
                                    Default::default(),
                                )
                                .await;
                            if let Err(e) = attach {
                                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                                    seed: Some(seed.clone()),
                                    label: "attach",
                                    result: Err(e),
                                }));
                                continue;
                            }
                            let result = api.bootstrap(&seed).await;
                            let _ =
                                tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
                        }
                    });
                }
                if epoch_changed {
                    self.toast(NoticeLevel::Warn, "daemon 重启：已重建连接并恢复会话");
                }
            }
            ConnEvent::Lost(reason) => {
                self.conn_phase = ConnPhase::Lost;
                self.conn_error = Some(reason);
            }
            ConnEvent::StreamIssue { stream, error } => {
                // 按流记账：只把**这条**流标成告警，相位/文案由账本统一推导。
                self.stream_issues.raise(stream, error);
                self.reconcile_conn_view();
            }
            ConnEvent::StreamRecovered { stream } => {
                // 只撤这条流自己的告警——别的流仍在自愈时必须继续显示告警，
                // 否则「A 流断开 + B 流重连成功」会把 A 的告警静默吞掉。
                self.stream_issues.clear(&stream);
                self.reconcile_conn_view();
            }
        }
    }

    /// 依据流告警账本刷新相位与 `conn_error`（唯一入口，规则见 [`reconcile_conn`]）。
    fn reconcile_conn_view(&mut self) {
        let (phase, error) = reconcile_conn(
            &self.conn_phase,
            &self.stream_issues,
            self.conn_error.take(),
        );
        self.conn_phase = phase;
        self.conn_error = error;
    }

    // ───────────────────────── canonical v2 投影事件 ─────────────────────────

    /// 一条 canonical v2 投影事件。
    ///
    /// 五个 payload family 各自分派；timeline 家族由独立的 per-seed timeline
    /// 流承载（transcript 权威），这里忽略以免双写。
    fn handle_v2_event(&mut self, seed: String, event: qaqh_client::ClientV2Event) {
        let causation_id = event.causation_id.clone();
        match event.payload {
            qaqh_client::ClientV2Payload::ControlDelta(delta) => {
                self.handle_control_delta(seed, causation_id, delta)
            }
            qaqh_client::ClientV2Payload::ConversationDelta(delta) => {
                self.handle_conversation_delta(seed, delta)
            }
            qaqh_client::ClientV2Payload::MetaDelta(delta) => self.handle_meta_delta(seed, delta),
            qaqh_client::ClientV2Payload::ResourceDelta(delta) => {
                self.handle_resource_delta(seed, delta)
            }
            qaqh_client::ClientV2Payload::TimelineDelta(_)
            | qaqh_client::ClientV2Payload::AuditRef(_)
            | qaqh_client::ClientV2Payload::Unknown(_) => {}
        }
    }

    // ───────────────────────── control 投影 ─────────────────────────

    fn handle_control_delta(
        &mut self,
        seed: String,
        causation_id: Option<String>,
        delta: qaqh_client::ClientV2ControlDelta,
    ) {
        use qaqh_client::ClientV2ControlDelta as D;
        match delta {
            D::SessionCreated { .. } => {
                // 新会话经信封 causation_id == command_id 关联（不轮询列表）。
                if let Some(cid) = causation_id
                    && self.pending_creates.remove(&cid).is_some()
                {
                    self.open_session_tab(&seed);
                    self.transfer_pending_initial_prompt();
                    self.toast(NoticeLevel::Info, format!("新会话已创建 {seed}"));
                }
            }
            D::Activity { state, .. } => {
                let state = activity_from_v2(state);
                self.activity_cache.insert(seed.clone(), state);
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.activity = Some(state);
                }
                if state == ActivityState::WaitingUser && !self.tabs.contains(&seed) {
                    self.toast(NoticeLevel::Warn, format!("会话 {seed} 等待输入"));
                }
            }
            D::InteractionRequested {
                interaction_id,
                call_id,
                kind,
                request,
                ..
            } => self.request_interaction(
                seed,
                interaction_id.as_str().to_string(),
                call_id,
                kind,
                request,
            ),
            D::InteractionResolved { interaction_id, .. }
            | D::InteractionExpired { interaction_id, .. } => {
                self.clear_interaction(&seed, interaction_id.as_str());
            }
            D::ToolFinished { call_id, .. } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.resolve_permission(call_id.as_str());
                }
            }
            D::SubagentSpawned {
                child_session_id,
                parent_call_id,
                ..
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    subagent::bind_seed(sess, parent_call_id.as_str(), child_session_id.as_str());
                }
            }
            D::SubagentFinished {
                child_session_id,
                status,
                ..
            } => {
                // 终态标签：同步条目并停止对应 seed 的 timeline 跟踪。
                let mut done_seed = None;
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    done_seed = subagent::apply_terminal(sess, child_session_id.as_str(), status);
                }
                if let Some(s) = done_seed {
                    self.untrack_subagent(&s);
                }
            }
            // 其余 control 增量（round / tool intent / driver / recovered）
            // 暂不驱动 UI：工具卡与子代理面板由 timeline 侧承载。
            D::Round { .. }
            | D::ToolIntent { .. }
            | D::DriverChanged { .. }
            | D::SessionRecovered { .. } => {}
        }
    }

    /// v2 `InteractionRequested` → 本仓挂起面板。
    ///
    /// - permission：正文为 `None`，详情来自 timeline 上同一 `call_id` 的工具卡；
    /// - ask / plan：正文在 content store（`ContentValue::Ref`），取回后按 body 的
    ///   `kind` 构造面板（body 由 `qaqh-domain` 的 `interaction_body` 单点序列化）。
    fn request_interaction(
        &mut self,
        seed: String,
        interaction_id: String,
        call_id: Option<qaqh_client::ClientV2ToolCallId>,
        kind: qaqh_client::ClientV2DeltaInteractionKind,
        request: qaqh_client::ClientV2ContentValue,
    ) {
        use qaqh_client::ClientV2DeltaInteractionKind as K;
        match kind {
            K::Permission => {
                if let Some(call_id) = call_id {
                    self.queue_permission_from_timeline(&seed, call_id.as_str());
                }
            }
            K::Ask | K::Plan => match request {
                qaqh_client::ClientV2ContentValue::Inline { text } => {
                    self.apply_interaction_body(&seed, &interaction_id, text.as_bytes());
                }
                qaqh_client::ClientV2ContentValue::Ref { content_ref } => {
                    self.download_interaction_body(
                        seed,
                        interaction_id,
                        content_ref.hash().as_str(),
                    );
                }
                qaqh_client::ClientV2ContentValue::Unavailable(_) => {}
            },
        }
    }

    /// v2 bootstrap 中的挂起交互 → 本仓面板。
    ///
    /// 这条路径不是实时事件的重复实现：v2 客户端按 snapshot cursor 起流，
    /// snapshot 与 subscribe 之间的事实不会再从流里 replay，必须由 bootstrap
    /// 补齐。permission 的详情仍来自 timeline；ask / plan 的正文按 bootstrap
    /// 携带的 content value 取回。
    fn restore_pending_interaction(
        &mut self,
        seed: String,
        interaction: qaqh_client::ClientV2PendingInteraction,
    ) {
        use qaqh_client::ClientV2InteractionKind as K;
        if self
            .suppressed_interactions
            .contains(&interaction.interaction_id)
            || (!interaction.call_id.is_empty()
                && self.suppressed_interactions.contains(&interaction.call_id))
        {
            return;
        }
        match interaction.kind {
            K::Permission => {
                if interaction.call_id.is_empty() {
                    return;
                }
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    let panel = sess.permission_panel_for(&interaction.call_id);
                    sess.restore_permission_from_snapshot(panel);
                }
            }
            K::Ask | K::PlanReview => {
                if self.has_pending_interaction(&seed, &interaction.interaction_id) {
                    return;
                }
                match interaction.request {
                    Some(qaqh_client::ClientV2PendingContentValue::Inline { text }) => {
                        self.apply_interaction_body(
                            &seed,
                            &interaction.interaction_id,
                            text.as_bytes(),
                        );
                    }
                    Some(qaqh_client::ClientV2PendingContentValue::Ref { content_ref }) => {
                        self.download_interaction_body(
                            seed,
                            interaction.interaction_id,
                            &content_ref,
                        );
                    }
                    Some(qaqh_client::ClientV2PendingContentValue::Unavailable(_)) | None => {}
                }
            }
        }
    }

    fn has_pending_interaction(&self, seed: &str, interaction_id: &str) -> bool {
        self.sessions.get(seed).is_some_and(|sess| {
            sess.pending_ask
                .as_ref()
                .is_some_and(|panel| panel.interaction_id == interaction_id)
                || sess
                    .pending_plan
                    .as_ref()
                    .is_some_and(|panel| panel.interaction_id == interaction_id)
        })
    }

    fn download_interaction_body(
        &mut self,
        seed: String,
        interaction_id: String,
        content_id: &str,
    ) {
        let content_id = content_id.to_string();
        self.spawn_api(move |api, tx| async move {
            let result = match api.download_content_by_id(&content_id).await {
                Ok(bytes) => Ok(bytes),
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(AppMsg::Action(ActionResult::InteractionBody {
                seed,
                interaction_id,
                result,
            }));
        });
    }

    /// 从 timeline 工具卡补齐 permission 面板详情（v2 交互正文不含 permission 详情）。
    fn queue_permission_from_timeline(&mut self, seed: &str, call_id: &str) {
        let Some(sess) = self.sessions.get_mut(seed) else {
            return;
        };
        let panel = sess.permission_panel_for(call_id);
        sess.queue_permission(panel);
    }

    /// ask / plan 交互正文（`qaqh-domain` 的 `interaction_body` JSON）→ 挂起面板。
    fn apply_interaction_body(&mut self, seed: &str, interaction_id: &str, bytes: &[u8]) {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return;
        };
        let kind = body
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        if self.suppressed_interactions.contains(interaction_id) {
            return;
        }
        let Some(sess) = self.sessions.get_mut(seed) else {
            return;
        };
        match kind {
            "ask" => {
                let mode = serde_json::from_value(body.get("mode").cloned().unwrap_or_default())
                    .unwrap_or(qaqh_client::AskMode::Single);
                let questions =
                    serde_json::from_value(body.get("questions").cloned().unwrap_or_default())
                        .unwrap_or_default();
                sess.pending_ask = Some(AskPanel::new(
                    interaction_id.to_string(),
                    String::new(),
                    mode,
                    questions,
                ));
                sess.scroll.follow = true;
            }
            "plan" => {
                let plan_content = body
                    .get("plan_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let review_type = body
                    .get("review_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("plan")
                    .to_string();
                let todo_items = serde_json::from_value(
                    body.get("todo_items")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                )
                .unwrap_or_default();
                sess.pending_plan = Some(PlanPanel {
                    interaction_id: interaction_id.to_string(),
                    turn_id: String::new(),
                    plan_content,
                    review_type,
                    todo_items,
                    message: String::new(),
                    entering_message: false,
                    scroll: 0,
                });
            }
            _ => {}
        }
    }

    /// 交互 resolved / expired：按 `interaction_id` 清对应挂起面板。
    fn clear_interaction(&mut self, seed: &str, interaction_id: &str) {
        let Some(sess) = self.sessions.get_mut(seed) else {
            return;
        };
        if sess
            .pending_ask
            .as_ref()
            .is_some_and(|p| p.interaction_id == interaction_id)
        {
            sess.pending_ask = None;
        }
        if sess
            .pending_plan
            .as_ref()
            .is_some_and(|p| p.interaction_id == interaction_id)
        {
            sess.pending_plan = None;
        }
    }

    // ───────────────────────── conversation 投影 ─────────────────────────

    fn handle_conversation_delta(
        &mut self,
        seed: String,
        delta: qaqh_client::ClientV2ConversationDelta,
    ) {
        use qaqh_client::ClientV2ConversationDelta as D;
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        let mut force_redraw = false;
        match delta {
            D::TurnStarted { turn_id, .. } => {
                sess.streaming = Some(session::StreamingState {
                    turn_id: turn_id.as_str().to_string(),
                    phase: StreamPhase::Thinking,
                    round_num: 0,
                    tool_name: None,
                    armed_at: Instant::now(),
                });
                sess.last_error = None;
                sess.scroll.follow = true;
                sess.scroll.offset = 0;
            }
            D::ToolCallDeclared { tool_name, .. } => {
                if let Some(s) = sess.streaming.as_mut() {
                    s.phase = StreamPhase::ToolCalling;
                    s.tool_name = Some(tool_name);
                }
            }
            D::ToolFinished { call_id, .. } => {
                sess.resolve_permission(call_id.as_str());
            }
            D::AssistantBlockSealed { model, usage, .. } => {
                let conv = sess
                    .conversation
                    .get_or_insert_with(qaqh_client::ConversationState::default);
                conv.model = Some(model);
                if let Some(u) = usage {
                    sess.usage = Some(u.clone());
                    conv.usage = Some(u);
                }
            }
            D::TurnFinished { turn_id, usage, .. } => {
                streaming_done(sess, Some(turn_id.as_str()));
                force_redraw = true;
                if let Some(u) = usage {
                    sess.usage = Some(u.clone());
                    if let Some(conv) = sess.conversation.as_mut() {
                        conv.usage = Some(u);
                    }
                }
            }
            D::TurnInterrupted { turn_id, .. } => {
                streaming_done(sess, Some(turn_id.as_str()));
                force_redraw = true;
            }
            D::CompactionApplied { .. } => {
                sess.compact_anim = None;
            }
            D::InputAccepted { .. } => {}
        }
        if force_redraw {
            self.force_redraw = true;
        }
    }

    // ───────────────────────── meta / resource 投影 ─────────────────────────

    fn handle_meta_delta(&mut self, seed: String, delta: qaqh_client::ClientV2MetaDelta) {
        use qaqh_client::ClientV2MetaDelta as D;
        match delta {
            D::TitleChanged { title, .. } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.title = Some(title);
                }
                self.session_list_at = None;
            }
            D::Deleted { .. } => {
                let was_tab = self.tabs.contains(&seed);
                self.close_tab_by_seed(&seed);
                if was_tab {
                    self.toast(NoticeLevel::Info, format!("会话 {seed} 已删除"));
                }
                self.session_list_at = None;
            }
            D::Created { .. }
            | D::MetadataChanged { .. }
            | D::ContextRevision { .. }
            | D::Recovered { .. } => {}
        }
    }

    fn handle_resource_delta(&mut self, _seed: String, _delta: qaqh_client::ClientV2ResourceDelta) {
        // workspace 面板由 `session.dashboard` RPC 拉取；resource 增量暂不驱动 UI。
    }

    // ───────────────────────── 后台结果 ─────────────────────────

    fn handle_action(&mut self, action: ActionResult) {
        match action {
            ActionResult::Bootstrap { seed, result } => match result {
                Ok(b) => {
                    let bootstrap_seed = seed.clone();
                    let mut pending_interactions = Vec::new();
                    if let Some(sess) = self.sessions.get_mut(&bootstrap_seed) {
                        // 纯 v2：bootstrap 是 canonical 三频道 typed 快照
                        // （`control` / `conversation` / `tool`），不再是 v1 领域
                        // `state`。v2 control 投影的 activity 词汇比领域粗
                        // （idle / running / interrupted），映射见下方 helper；
                        // 挂起交互直接从 `control.state.interactions` 恢复。
                        let ctl = &b.control.state;
                        sess.activity = Some(activity_from_v2(ctl.activity));
                        // bootstrap 是 control 域快照的权威刷新点；这里与 timeline
                        // 收敛一次，避免上一次连接遗留的 Working/Starting 与
                        // streaming 状态把 UI 钉在 working（timeline 空则不误判）。
                        sync_streaming_from_timeline(sess);
                        // 会话模式的实际来源是 transcript_ops.rs 的乐观更新 +
                        // SessionMetaChanged 刷新，bootstrap 不携带该字段。
                        // conversation 快照里本仓只缓存 model/usage（v2 投影不再
                        // 有聚合的 usage_totals / context_limit）。
                        let conv = conversation_cache_from_v2(&b.conversation.state);
                        sess.usage = conv.usage.clone();
                        sess.usage_totals = conv.usage_totals.clone();
                        sess.context_limit =
                            conv.context_limit.map(|v| v.min(u32::MAX as u64) as u32);
                        sess.conversation = Some(conv);
                        // v2 bootstrap 的 `interactions` 已由 daemon 过滤为未决集合。
                        // 不能只恢复 permission：v2 流的 snapshot cursor 会跳过
                        // “快照中已经挂起”的事实，ask / plan 也必须从这里补面板。
                        pending_interactions = ctl.interactions.clone();
                        sess.block_cache = None;
                    }
                    for interaction in pending_interactions {
                        self.restore_pending_interaction(bootstrap_seed.clone(), interaction);
                    }
                    // v2 control 投影不携带 dashboard 快照（v1 领域 control state
                    // 才有），workspace 面板一律回退到 `session.dashboard` 拉取。
                    self.fetch_dashboard(bootstrap_seed);
                }
                Err(e) => self.toast(NoticeLevel::Error, format!("bootstrap 失败[{seed}]: {e}")),
            },
            ActionResult::InteractionBody {
                seed,
                interaction_id,
                result,
            } => match result {
                Ok(bytes) => self.apply_interaction_body(&seed, &interaction_id, &bytes),
                Err(e) => self.toast(NoticeLevel::Warn, format!("交互正文取回失败[{seed}]: {e}")),
            },
            ActionResult::CommandAck {
                seed,
                label,
                result,
            } => match result {
                Ok(ack) => {
                    if ack.status == qaqh_client::RingingCommandAckStatus::Rejected {
                        // 缺陷 1（CNB issue #4）：拒绝是终态，不会再有
                        // `causation_id == command_id` 的 `Created` 事件。不撤销
                        // pending create 的话，状态栏的 `· creating…` 会一直挂到
                        // 15s 过期（`handle_tick`），用户以为还在创建。
                        //
                        // 是否 create 命令按 `seed.is_none()` 判：wire 层
                        // `RingingCommandEnvelope::validate` 要求 seed 缺失时命令
                        // 必须是 `SessionCreate`（`qaqh-ringing/src/envelope.rs:177`），
                        // 故这是权威判据，不靠 label 猜。
                        let is_create = seed.is_none();
                        let msg = session_ops::apply_rejected_ack(
                            &mut self.pending_creates,
                            label,
                            &ack,
                            is_create,
                        );
                        if is_create && let Some(text) = self.pending_initial_prompt.take() {
                            self.draft_composer.insert_str(&text);
                        }
                        self.toast(NoticeLevel::Error, msg.clone());
                        if let Some(seed) = seed
                            && let Some(sess) = self.sessions.get_mut(&seed)
                        {
                            sess.composer.input = msg.chars().collect(); // 不丢内容
                            sess.composer.cursor = sess.composer.input.len();
                        }
                    }
                }
                Err(e) => {
                    let lease_dead = e.contains("lease");
                    self.toast(NoticeLevel::Error, format!("{label}: {e}"));
                    if lease_dead {
                        // 等 supervisor 重新 open 后 Ready 处理器会重 attach。
                    }
                }
            },
            ActionResult::SessionList(Ok(list)) => {
                // 新建会话的**兜底发现**：`Ctrl+N` 后不能只等
                // `SessionStateEvent::Created` 那条可靠事件——实测（真 daemon，
                // 锚点 b77c251）daemon 侧收据 state=succeeded、journal 里
                // `session_state_changed/created` 带正确 causation_id，但 TUI 侧
                // 既不开 tab 也不 toast，v2 Agent View 首屏没有列表面，于是
                // 「按 Ctrl+N 什么都没发生、再按一次又多建一个会话」。
                //
                // 这里用列表兜底：有在途 create 时，把**最新建的那个**会话开出来。
                // 判据取 `created_at` 最大者，且必须是本地还没有 tab 的 seed ——
                // 否则会去开一个用户没要求的旧会话。
                if !self.pending_creates.is_empty()
                    && let Some(entry) = list.iter().max_by_key(|e| e.meta.created_at)
                {
                    let seed = entry.meta.seed.clone();
                    if !self.tabs.contains(&seed) {
                        self.open_session_tab(&seed);
                        self.pending_creates.clear();
                        self.transfer_pending_initial_prompt();
                        self.toast(NoticeLevel::Info, format!("新会话已创建 {seed}"));
                    }
                }
                self.session_list_cache = list;
                self.session_list_at = Some(Instant::now());
                // 首页选中越界回绕
                let count = self.filtered_sessions(self.home_show_archived).len();
                if count > 0 && self.home_selected >= count {
                    self.home_selected = count - 1;
                }
            }
            ActionResult::SessionList(Err(e)) => {
                self.toast(NoticeLevel::Error, format!("session.list: {e}"))
            }
            // 权威类型 `SessionActivity`（生产者本就是 `Vec<SessionActivity>`）：
            // 此前这里是 `item.get("seed")` + `item.get("state")` 手取两个键，
            // 与 G2 删掉的那类手解同款。现在字段在类型上，改形状会编译报错。
            ActionResult::SessionActivity(Ok(items)) => {
                for item in items {
                    self.activity_cache.insert(item.seed.clone(), item.state);
                }
            }
            ActionResult::SessionActivity(Err(_)) => {}
            ActionResult::ConfigLoaded(Ok(v)) => match serde_json::from_value::<ConfigDto>(v) {
                Ok(dto) => self.config = Some(dto),
                Err(e) => self.toast(NoticeLevel::Error, format!("config.load 解析失败: {e}")),
            },
            ActionResult::ConfigLoaded(Err(e)) => {
                self.toast(NoticeLevel::Error, format!("config.load: {e}"));
            }
            ActionResult::ConfigWrite { label, result } => match result {
                Ok(_) => {
                    self.toast(NoticeLevel::Info, format!("{label} 已保存"));
                    if label == "设置" {
                        // 保存成功：清草稿（loaded 由 ConfigChanged 重拉替换，
                        // 此处显式重拉一次兜底 SSE 延迟）。
                        self.settings_saving = false;
                        if let Some(Overlay::Settings(st)) = self.overlays.last_mut() {
                            st.draft = settings::SettingsState::default().draft;
                        }
                        self.fetch_config();
                    }
                }
                Err(e) => {
                    if label == "设置" {
                        self.settings_saving = false;
                    }
                    self.toast(NoticeLevel::Error, format!("{label}: {e}"));
                }
            },
            ActionResult::Uploaded { seed, path, result } => match result {
                Ok(content) => {
                    if let Some(sess) = self.sessions.get_mut(&seed) {
                        let name = std::path::Path::new(&path)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.clone());
                        sess.composer.attachments.push(session::Attachment {
                            path: name,
                            content,
                        });
                        self.toast(NoticeLevel::Info, "附件已上传".to_string());
                    } else {
                        // 上传途中目标会话被关闭（异步竞态）：上传成功也无处可挂，
                        // 必须说出来而不是静默丢弃。
                        self.toast(
                            NoticeLevel::Error,
                            format!("附件已上传但目标会话已关闭：{seed}"),
                        );
                    }
                }
                Err(e) => {
                    // 修正 winui 的静默吞错：上传失败必须可见。
                    self.toast(NoticeLevel::Error, format!("附件上传失败 {path}: {e}"));
                }
            },
            ActionResult::Rebaseline { seed, result } => {
                if let (Some(sess), Ok(page)) = (self.sessions.get_mut(&seed), result) {
                    sess.timeline.replace_from_page(&page);
                    sess.scroll.follow = true;
                    sess.scroll.offset = 0;
                    sess.block_cache = None;
                }
            }
            ActionResult::LoadOlder { seed, result } => {
                if let (Some(sess), Ok(page)) = (self.sessions.get_mut(&seed), result) {
                    sess.loading_older = false;
                    // prepend 而非 replace：已加载的窗口内容保留；
                    // offset（距底行数）不变，视口内容相对稳定。
                    sess.timeline.prepend_older(&page);
                    sess.block_cache = None;
                } else if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.loading_older = false;
                }
            }
            ActionResult::Receipt {
                label,
                seed,
                result,
                ..
            } => match result {
                Ok(status) if status.state == CommandState::Succeeded => {
                    self.toast(NoticeLevel::Info, format!("{label} 完成"));
                    if let Some(seed) = seed {
                        self.request_rebaseline(&seed);
                    }
                }
                Ok(status) => {
                    self.toast(NoticeLevel::Warn, format!("{label}: {:?}", status.state));
                }
                Err(e) => self.toast(NoticeLevel::Error, format!("{label}: {e}")),
            },
            ActionResult::Dashboard { seed, result } => {
                self.dashboard_fetching.remove(&seed);
                match result {
                    Ok(dash) => {
                        if let Some(sess) = self.sessions.get_mut(&seed)
                            && (!dash.tasks.is_empty()
                                || !dash.recent_edits.is_empty()
                                || !dash.documents.is_empty())
                        {
                            // 同上：dashboard 更新不触碰 transcript 渲染缓存。
                            sess.dashboard = Some(dash);
                        }
                    }
                    Err(_e) => {}
                }
            }
        }
    }

    // ───────────────────────── 标签页 / 会话 ─────────────────────────

    fn sync_tracked(&mut self) {
        // 打开的标签 + 正在跟踪的子代理 seed（各自独立 timeline 流）。
        let mut seeds: Vec<String> = self.tabs.clone();
        for s in &self.subagent_seeds {
            if !seeds.contains(s) {
                seeds.push(s.clone());
            }
        }
        self.tracked_seeds = seeds.iter().cloned().collect();
        self.runtime.set_tracked_seeds(seeds);
    }

    pub fn toast(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.toasts.push_back(Toast {
            level,
            text: text.into(),
            at: Instant::now(),
        });
        while self.toasts.len() > 8 {
            self.toasts.pop_front();
        }
    }

    // ───────────────────────── 滚动 ─────────────────────────

    /// app → daemon 异步出口的唯一入口：集中克隆 client/msg_tx 并 spawn。
    /// 今后如需统一超时/退避/取消/指标，只需叠加在此处。
    ///
    /// 没有连接（测试替身）时**任务照起**：取用连接的那一步会返回 `Err`，结果照常
    /// 经 `AppMsg::Action` 回来——「上传目标是哪个 seed」这类归属信息因此仍可断言。
    /// 后台重新 bootstrap 一个 seed（v2 流建立/重连、或 reset 后）。
    fn spawn_bootstrap(&mut self, seed: String) {
        self.spawn_api(move |api, tx| async move {
            let result = api.bootstrap(&seed).await;
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
        });
    }

    pub(super) fn spawn_api<F, Fut>(&self, task: F)
    where
        F: FnOnce(ApiCtx, tokio::sync::mpsc::UnboundedSender<AppMsg>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let api = ApiCtx {
            client: self.runtime.client_opt(),
        };
        let tx = self.msg_tx.clone();
        tokio::spawn(async move { task(api, tx).await });
    }

    fn handle_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::KeyModifiers;

        // 粘贴护栏：记录按键节拍（洪流判定与抑制在 composer_key 的 Enter 路径）。
        self.paste_guard.observe(Instant::now());

        let global = keymap::map_global_key(&key);
        let blocking_modal = self.active_session().is_some_and(|session| {
            session.active_permission().is_some()
                || session.pending_ask.is_some()
                || session.pending_plan.is_some()
        });
        // 阻塞式交互期间只放行退出键，避免 Ctrl+L/Ctrl+,/F1 等全局键把
        // overlay 压在 Modal 下方，随后 Esc 误落到 permission/ask/plan。
        if blocking_modal
            && !matches!(global, Some(GlobalKey::QuitArmed | GlobalKey::QuitNow))
            && self.modal_key(key)
        {
            return;
        }

        // 退出与全局键：先经 keymap 纯映射（可单测），再做状态副作用。
        match global {
            Some(GlobalKey::QuitArmed) => {
                if self.quit_armed.is_some() {
                    self.quit = true;
                } else {
                    self.quit_armed = Some(Instant::now());
                    self.toast(NoticeLevel::Info, "再按一次 Ctrl+C 退出");
                }
                return;
            }
            Some(GlobalKey::QuitNow) => {
                self.quit = true;
                return;
            }
            Some(GlobalKey::NewSession) => {
                if self.tabs.is_empty() && self.startup_intent == StartupIntent::New {
                    self.start_draft_conversation();
                } else {
                    self.new_session();
                }
                return;
            }
            Some(GlobalKey::ThinkingOverlay) => {
                self.open_thinking_overlay();
                return;
            }
            Some(GlobalKey::CloseTab) => {
                if let Some(seed) = self.active_seed() {
                    self.overlays.push(Overlay::Confirm {
                        action: ConfirmAction::CloseTab(seed),
                    });
                }
                return;
            }
            Some(GlobalKey::SessionList) => {
                self.open_session_list();
                return;
            }
            Some(GlobalKey::ToggleSettings) => {
                self.toggle_settings();
                return;
            }
            Some(GlobalKey::Help) => {
                self.toggle_overlay(Overlay::Help);
                return;
            }
            Some(GlobalKey::ToggleReasoning) => {
                self.show_activity = !self.show_activity;
                for s in self.sessions.values_mut() {
                    s.block_cache = None;
                }
                return;
            }
            Some(GlobalKey::ToggleWorkspace) => {
                let opening = !self.show_workspace;
                self.show_workspace = !self.show_workspace;
                if opening && let Some(seed) = self.active_seed() {
                    self.fetch_dashboard(seed);
                }
                return;
            }
            Some(GlobalKey::ToggleTodoDetail) => {
                self.show_todo_detail = !self.show_todo_detail;
                return;
            }
            Some(GlobalKey::ToggleToolExpand) => {
                self.toggle_tool_expand();
                return;
            }
            Some(GlobalKey::Reconnect) => {
                self.request_reconnect();
                return;
            }
            None => {}
        }

        // Alt+数字 / Alt+方向：标签切换（目标计算为纯函数，见 keymap）。
        if key.modifiers.contains(KeyModifiers::ALT)
            && let Some(next) = keymap::alt_tab_target(self.active, self.tabs.len(), key.code)
        {
            self.active = next;
            // 切标签即退出子代理观测（观测作用域属于原标签）。
            if self.inspecting() {
                self.exit_inspect();
            }
            // 上一条标签遗留的确认/附件 overlay 属于旧 seed：不能跟着切过来。
            self.prune_overlays_for_active_seed();
            return;
        }

        // 交互弹窗（permission > ask > plan）吃掉全部按键。
        if self.modal_key(key) {
            return;
        }

        // 覆盖层。
        if self.overlay_key(key) {
            return;
        }

        // 子代理视图栈导航（弹窗/覆盖层优先级之后）：Ctrl+↑ 深入/下一个，
        // Ctrl+↓ 返回父会话，Esc 退出观测。
        if self.subagent_nav_key(key) {
            return;
        }

        // 观测模式（只读）：滚动键作用于子代理视图，其余按键一律吞掉，
        // 避免误输入到隐藏的父会话 composer。
        if self.inspecting() {
            self.inspect_key(key);
            return;
        }

        // 首页（无 tab 且无覆盖层时）。
        //
        // 普通启动的品牌首屏把按键全部交给 draft composer：用户输入第一句后
        // 才创建会话，不再要求先按 Ctrl+N。`resume` 的列表关闭后也回到同一输入框。
        if self.tabs.is_empty() {
            if self.overlays.is_empty() {
                self.draft_key(key);
                return;
            }
            if self.home_key(key) {
                return;
            }
        }

        // Composer。
        self.composer_key(key);
    }

    fn toggle_overlay(&mut self, overlay: Overlay) {
        let same = self
            .overlays
            .last()
            .map(|o| std::mem::discriminant(o) == std::mem::discriminant(&overlay))
            .unwrap_or(false);
        if same {
            self.overlays.pop();
        } else {
            self.overlays.push(overlay);
        }
    }

    /// 每帧前维护：焦点变化时执行 LRU 内存回收；只为 active 会话维护渲染缓存。
    ///
    /// 四个关键点（对应 A1/A2/A3 + 虚拟化）：
    ///
    /// - **A1 宽度对齐**：内容宽取自 `ui::transcript_viewport`（与
    ///   `transcript::draw` 同一事实源），而不是终端全宽。历史缺陷就是两者
    ///   永不相等 → 缓存 100% 失效 → 每帧两次全量渲染。
    /// - **A2 无变化不重渲**：只在内容键变化（或存在动画段）时才动；空闲
    ///   帧不重建任何段。
    /// - **A3 分段缓存**：按回合分段，只重渲键变了的段，其余复用 `Arc`。
    ///   流式只重渲正在增长的那一段，不再扫全量历史。
    /// - **虚拟化**：只保留视口附近段的渲染结果，离屏段退化为估算高度；
    ///   滚动几何由高度之和给出，无需渲染全量历史。
    pub fn ensure_render_caches(&mut self, area: ratatui::layout::Rect) {
        let Some(active) = self.view_seed() else {
            return;
        };
        if self.last_focused.as_deref() != Some(active.as_str()) {
            self.touch_focus(&active);
            self.last_focused = Some(active.clone());
        }
        let show_workspace = self.show_workspace;
        let composer_height = crate::ui::composer::height(self);
        let activity_height = crate::ui::activity_bar::height(self);
        let (width, height) =
            crate::ui::transcript_viewport(area, composer_height, activity_height, show_workspace);
        let Some(sess) = self.sessions.get_mut(&active) else {
            return;
        };
        // 缓存内嵌于 SessionState：take 出来以满足 refresh 的 &SessionState 借用
        // （结构体 move 是指针搬运，O(1)）。
        let mut cache = sess
            .block_cache
            .take()
            .unwrap_or_else(|| render::TranscriptCache::new(width));
        refresh_at_viewport(sess, width, height, &mut cache);
        sess.block_cache = Some(cache);
    }
}

/// 视口不动点：在 `cache` 上把视口覆盖到的块渲染齐，供本帧 `draw` 取窗口。
///
/// 为什么要迭代：`refresh` 会把估算高度换成精确高度 ⇒ **总行数会在 refresh 中改变**，
/// 用刷新前的 total 算出的 `top` 与 `draw` 用刷新后的 total 算出的 `top` 不一致，
/// 窗口就会落到未渲染块上（issue #33：debug 直接 panic，release 静默空屏）。
/// 所以迭代到「用刷新后的 total 算出的 top 与上一趟相同」为止；实测首帧 2~3 趟收敛，
/// 第 2 趟起几乎全是复用（O(keep)）。上限只是防呆——即使超出，
/// `TranscriptCache::viewport` 仍保证 `draw` 取到**同一份**几何，不变量不会破。
///
/// 抽成自由函数是为了**可测**：`app::render` 的回归锁直接调它，不必复刻这套循环。
pub(crate) fn refresh_at_viewport(
    sess: &SessionState,
    width: u16,
    height: usize,
    cache: &mut render::TranscriptCache,
) {
    let mut top = crate::ui::viewport_top(
        cache.total_lines(),
        height,
        sess.scroll.follow,
        sess.scroll.offset,
    );
    for _ in 0..VIEWPORT_FIXPOINT_PASSES {
        render::refresh(sess, width, Some((top, height)), cache);
        let next = crate::ui::viewport_top(
            cache.total_lines(),
            height,
            sess.scroll.follow,
            sess.scroll.offset,
        );
        if next == top {
            break;
        }
        top = next;
    }
}

pub fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

pub fn guess_media_type(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        _ => "text/plain",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{RingingCommandAck, RingingCommandAckStatus, SessionMeta};

    fn list_entry(seed: &str, created_at: u64) -> SessionListEntry {
        SessionListEntry {
            meta: SessionMeta {
                seed: seed.to_string(),
                created_at,
                ..SessionMeta::default()
            },
            running: false,
            workspace_id: None,
        }
    }

    fn list_entry_with_cwd(seed: &str, cwd: Option<&str>) -> SessionListEntry {
        SessionListEntry {
            meta: SessionMeta {
                seed: seed.to_string(),
                cwd: cwd.map(str::to_owned),
                ..SessionMeta::default()
            },
            running: false,
            workspace_id: None,
        }
    }

    #[tokio::test]
    async fn bootstrap_restores_pending_ask_from_snapshot() {
        let (mut app, _rx) = App::new_for_test();
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));

        let cursor = qaqh_client::ClientV2CursorToken::encode_snapshot(
            &qaqh_client::ClientV2Cursor::snapshot("log-1", 1),
        )
        .expect("snapshot cursor");
        let mut control = serde_json::to_value(qaqh_client::ClientV2ControlState::default())
            .expect("control baseline");
        control["interactions"] = serde_json::json!([{
            "interaction_id": "int-ask-1",
            "call_id": "call-ask-1",
            "turn_id": "turn-1",
            "kind": "ask",
            "request": {
                "kind": "inline",
                "data": {
                    "text": serde_json::json!({
                        "kind": "ask",
                        "mode": "single",
                        "questions": [{
                            "id": "q1",
                            "question": "选哪个？",
                            "options": ["甲", "乙"],
                            "allow_custom": false
                        }]
                    }).to_string()
                }
            }
        }]);
        let bootstrap: qaqh_client::ClientV2Bootstrap = serde_json::from_value(serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 2,
            "server_epoch": "epoch-1",
            "seed": "seed",
            "snapshot_cursor": cursor.as_str(),
            "control": {
                "channel": "control",
                "state_revision": 1,
                "snapshot_version": 1,
                "state": control
            },
            "conversation": {
                "channel": "conversation",
                "state_revision": 1,
                "snapshot_version": 1,
                "state": serde_json::to_value(qaqh_client::ClientV2ConversationState::default())
                    .expect("conversation baseline")
            },
            "tool": {
                "channel": "tool",
                "state_revision": 1,
                "snapshot_version": 1,
                "state": serde_json::to_value(qaqh_client::ClientV2ToolState::default())
                    .expect("tool baseline")
            }
        }))
        .expect("client bootstrap");

        app.handle(AppMsg::Action(ActionResult::Bootstrap {
            seed: "seed".into(),
            result: Ok(bootstrap),
        }));

        let ask = app.sessions["seed"]
            .pending_ask
            .as_ref()
            .expect("ask restored from bootstrap");
        assert_eq!(ask.interaction_id, "int-ask-1");
        assert_eq!(ask.questions[0].question, "选哪个？");
    }

    #[test]
    fn turn_completed_requests_forced_redraw() {
        let (mut app, _rx) = App::new_for_test();
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));

        app.handle(AppMsg::Runtime(v2_event(
            "seed",
            qaqh_client::ClientV2Payload::ConversationDelta(
                qaqh_client::ClientV2ConversationDelta::TurnFinished {
                    revision: 1,
                    turn_id: qaqh_client::ClientV2TurnId::new("turn-1"),
                    terminal: qaqh_client::ClientV2TurnTerminal::Completed,
                    usage: None,
                    error: None,
                },
            ),
        )));

        assert!(app.force_redraw, "回合终态必须触发下一帧强制重绘");
    }

    #[test]
    fn timeline_turn_sealed_requests_forced_redraw() {
        let (mut app, _rx) = App::new_for_test();
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));

        app.handle_runtime(RuntimeMsg::Timeline {
            seed: "seed".into(),
            entry: Box::new(qaqh_client::TimelineEntry {
                timeline_seq: 1,
                turn_id: "turn-1".into(),
                round_num: Some(0),
                event: qaqh_client::TimelineEvent::TurnSealed {
                    state: qaqh_client::TimelineTurnState::Completed,
                    failure: None,
                },
            }),
        });

        assert!(app.force_redraw, "timeline 终态也必须触发下一帧强制重绘");
    }

    #[tokio::test]
    async fn draft_enter_queues_first_prompt_before_create() {
        let (mut app, _rx) = App::new_for_test();
        app.draft_composer.insert_str("第一句");
        app.handle(AppMsg::Key(KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        )));

        assert!(app.draft_composer.is_empty());
        assert_eq!(app.pending_initial_prompt.as_deref(), Some("第一句"));
        assert_eq!(app.pending_creates.len(), 1);
    }

    #[tokio::test]
    async fn pending_initial_prompt_is_transferred_when_create_lands() {
        let (mut app, _rx) = App::new_for_test();
        app.pending_initial_prompt = Some("hello".into());
        app.pending_creates
            .insert("cmd-create".into(), Instant::now());

        app.handle(AppMsg::Action(ActionResult::SessionList(Ok(vec![
            list_entry("new-seed", 200),
        ]))));

        assert_eq!(app.tabs, vec!["new-seed".to_string()]);
        assert!(app.pending_initial_prompt.is_none());
        assert_eq!(
            app.sessions["new-seed"].composer.value(),
            "hello",
            "首条草稿应原样带入真实会话 composer"
        );
    }

    #[test]
    fn resume_cwd_filter_keeps_current_directory_tree_only() {
        let (mut app, _rx) = App::new_for_test();
        app.session_list_cache = vec![
            list_entry_with_cwd("a", Some("/work/project")),
            list_entry_with_cwd("b", Some("/work/project/sub")),
            list_entry_with_cwd("c", Some("/work/other")),
            list_entry_with_cwd("d", None),
        ];
        app.session_cwd_filter = Some("/work/project".into());

        assert_eq!(app.filtered_sessions(false), vec![0, 1]);
    }

    /// 新建会话的**兜底发现**回归锁。
    ///
    /// 为什么不能只锁 `Created` 事件那条路：实测（真 daemon，锚点 b77c251）daemon
    /// 侧收据 `state=succeeded`、journal 里 `session_state_changed/created` 带**正确**
    /// 的 `causation_id == command_id`，但 TUI 侧既不开 tab 也不 toast。默认 Agent
    /// View 首屏又没有列表面，于是表现为「按 Ctrl+N 什么都没发生、再按一次又多建
    /// 一个会话」。所以「新会话要真的开出来」必须由列表兜底，这条锁的就是它。
    // 必须跑在 Tokio runtime 里：`open_session_tab` → `attach_and_bootstrap`
    // → `spawn_api` 走 `tokio::spawn`。
    #[tokio::test]
    async fn pending_create_opens_newest_session_from_list() {
        let (mut app, _rx) = App::new_for_test();
        app.pending_creates
            .insert("cmd-create-3".to_string(), Instant::now());

        app.handle(AppMsg::Action(ActionResult::SessionList(Ok(vec![
            list_entry("old-seed", 100),
            list_entry("new-seed", 200),
        ]))));

        assert_eq!(
            app.tabs,
            vec!["new-seed".to_string()],
            "有在途 create 时必须打开**最新建**的那个会话（且不得顺手开旧会话）"
        );
        assert!(
            app.pending_creates.is_empty(),
            "打开后要消费掉在途 create，否则每次列表刷新都会反复开"
        );
    }

    /// 反向闸（同一条分支）：**没有**在途 create 时，列表刷新不得擅自打开任何
    /// 会话——否则每次首页自动刷新都会把用户弹进某个旧会话。
    #[test]
    fn list_refresh_without_pending_create_opens_nothing() {
        let (mut app, _rx) = App::new_for_test();

        app.handle(AppMsg::Action(ActionResult::SessionList(Ok(vec![
            list_entry("old-seed", 100),
        ]))));

        assert!(
            app.tabs.is_empty(),
            "无在途 create 时不得自动开会话，实测 tabs={:?}",
            app.tabs
        );
    }

    fn create_ack(command_id: &str, status: RingingCommandAckStatus) -> RingingCommandAck {
        RingingCommandAck {
            command_id: command_id.to_string(),
            status,
            code: Some("rate_limited".into()),
            message: Some("too many sessions".into()),
            retry_after_ms: Some(500),
        }
    }

    /// CNB issue #4 缺陷 1 的**全链路**回归锁（PR #18 二轮复审阻断项）。
    ///
    /// 此前只测了 `session_ops::apply_rejected_ack` 本身，把本文件 handler 里那行
    /// 调用换回旧行为（只 toast、不撤销）时**全绿**——本次主修复唯一生效的那层
    /// 没有网。本测试打穿 `App::handle` → `handle_action`，锁的是**状态栏
    /// `· creating…` 的消失**（`ui/status_bar.rs:40` 读的就是 `pending_creates`），
    /// 不是函数返回值。
    ///
    /// 变异验证（实测）：把 `handle_action` 里的 `apply_rejected_ack` 调用换回
    /// 旧代码 → 本测试红，实测残留 `["cmd-create-1"]`。
    #[test]
    fn rejected_create_ack_from_handler_clears_pending_create() {
        let (mut app, _rx) = App::new_for_test();
        app.pending_creates
            .insert("cmd-create-1".to_string(), Instant::now());

        app.handle(AppMsg::Action(ActionResult::CommandAck {
            seed: None,
            label: "新会话",
            result: Ok(create_ack(
                "cmd-create-1",
                RingingCommandAckStatus::Rejected,
            )),
        }));

        assert!(
            app.pending_creates.is_empty(),
            "handler 必须真的撤销 pending create（否则状态栏 creating… 挂满 15s），实测残留 {:?}",
            app.pending_creates.keys().collect::<Vec<_>>()
        );
        let toast = app.toasts.back().expect("被拒必须有失败提示");
        assert!(
            toast.text.contains("未创建"),
            "提示要说清会话没建成，实测：{}",
            toast.text
        );
    }

    /// 反向闸（同一条 handler）：`Accepted` 不是失败，不得撤销 pending create——
    /// 命令已进入 actor，`Created` 事件还会经 `causation_id` 回来，提前撤销会让
    /// 新建的标签页永不自动打开。
    #[test]
    fn accepted_create_ack_from_handler_keeps_pending_create() {
        let (mut app, _rx) = App::new_for_test();
        app.pending_creates
            .insert("cmd-create-2".to_string(), Instant::now());

        app.handle(AppMsg::Action(ActionResult::CommandAck {
            seed: None,
            label: "新会话",
            result: Ok(create_ack(
                "cmd-create-2",
                RingingCommandAckStatus::Accepted,
            )),
        }));

        assert_eq!(
            app.pending_creates.len(),
            1,
            "Accepted 不得撤销 pending create（标签页还要靠它自动打开）"
        );
    }

    /// 反向闸（同一条 handler）：别的命令被拒不得误伤新建会话的 pending。
    #[test]
    fn rejected_ack_for_other_command_from_handler_keeps_pending_create() {
        let (mut app, _rx) = App::new_for_test();
        app.pending_creates
            .insert("cmd-create-3".to_string(), Instant::now());

        app.handle(AppMsg::Action(ActionResult::CommandAck {
            seed: Some("seed-x".into()),
            label: "撤销回合",
            result: Ok(create_ack("cmd-other-9", RingingCommandAckStatus::Rejected)),
        }));

        assert_eq!(
            app.pending_creates.len(),
            1,
            "无关命令的拒绝不得撤销 pending create"
        );
    }

    fn channel(c: qaqh_client::Channel) -> StreamKey {
        // v1 三频道已并入每 seed 一条的 v2 单流；测试里仍用频道名当不同的流身份。
        StreamKey::V2(c.as_str().to_string())
    }

    fn timeline(seed: &str) -> StreamKey {
        StreamKey::Timeline(seed.into())
    }

    /// 构造一条最小可用的 canonical v2 投影事件（ephemeral，无 cursor）。
    fn v2_event(seed: &str, payload: qaqh_client::ClientV2Payload) -> RuntimeMsg {
        RuntimeMsg::V2Event {
            seed: seed.into(),
            event: Box::new(qaqh_client::ClientV2Event {
                schema: qaqh_client::RINGING_SCHEMA.into(),
                version: qaqh_client::RINGING_V2_VERSION,
                server_epoch: "e1".into(),
                seed: seed.into(),
                event_id: "ev-1".into(),
                stream_key: qaqh_client::ClientV2StreamKey::Channel(
                    qaqh_client::Channel::Conversation,
                ),
                delivery: qaqh_client::ClientV2Delivery::Ephemeral,
                cursor: None,
                log_id: None,
                fact_seq: None,
                projection_index: None,
                revision: None,
                causation_id: None,
                correlation_id: None,
                payload,
            }),
        }
    }

    /// 阻断项 1 的回归：告警必须**按流**记账。
    ///
    /// 场景：seed A 的 timeline 流断开（告警），随后另一条流（session 频道）重连
    /// 成功（恢复信号）——A 的告警必须还在、相位仍是 `ReadyWithIssue`，文案也不能
    /// 串成后者的。证伪方式：把账本退回「一个全局布尔 + 直接清 conn_error」（旧
    /// 行为），本测试会变成 `Ready` 且无文案。
    #[test]
    fn stream_alert_is_cleared_only_by_its_own_recovery() {
        let mut issues = StreamIssues::default();
        issues.raise(timeline("A"), "timeline[A] 断开，3000ms 后重连".into());
        let (phase, error) = reconcile_conn(&ConnPhase::Ready, &issues, None);
        assert_eq!(phase, ConnPhase::ReadyWithIssue);
        assert!(
            error.as_deref().unwrap_or_default().contains("timeline[A]"),
            "首次告警文案必须可见：{error:?}"
        );

        // 另一条流恢复：不能把 A 的告警吞掉。
        assert!(
            !issues.clear(&channel(qaqh_client::Channel::Conversation)),
            "这条流本来就没有告警，clear 应当报告「原本不在账本里」"
        );
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(
            phase,
            ConnPhase::ReadyWithIssue,
            "别的流重连成功不得清掉 A 的告警"
        );
        assert!(
            error.as_deref().unwrap_or_default().contains("timeline[A]"),
            "文案不得串成别人的：{error:?}"
        );

        // A 自己恢复：这才是清空的时机。
        assert!(issues.clear(&timeline("A")), "A 此前在账本里");
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::Ready);
        assert!(error.is_none());
    }

    /// 多条流同时告警：全部清完才回到 `Ready`，文案取最近一条。
    #[test]
    fn ready_is_restored_only_when_every_stream_recovers() {
        let mut issues = StreamIssues::default();
        issues.raise(
            channel(qaqh_client::Channel::Control),
            "control 断开".into(),
        );
        issues.raise(timeline("A"), "timeline[A] 断开".into());
        let (phase, error) = reconcile_conn(&ConnPhase::Ready, &issues, None);
        assert_eq!(phase, ConnPhase::ReadyWithIssue);
        assert_eq!(error.as_deref(), Some("timeline[A] 断开"), "取最近一条");

        issues.clear(&channel(qaqh_client::Channel::Control));
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::ReadyWithIssue, "还剩 timeline[A] 没恢复");
        assert_eq!(
            error.as_deref(),
            Some("timeline[A] 断开"),
            "不能残留已经恢复那条流的文案"
        );

        issues.clear(&timeline("A"));
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::Ready);
        assert!(error.is_none());
    }

    /// 同一 `StreamKey` 重复告警只覆盖文案，不堆积条目。
    #[test]
    fn repeated_issue_for_same_stream_overwrites() {
        let mut issues = StreamIssues::default();
        let key = timeline("A");
        assert!(
            issues.raise(key.clone(), "第一次".into()),
            "首次算新出现的流"
        );
        assert!(
            !issues.raise(key.clone(), "第二次".into()),
            "同一条流重复告警不算新增"
        );
        assert_eq!(issues.messages.len(), 1, "账本条目不堆积");
        assert_eq!(issues.order.len(), 1, "顺序表也不堆积");
        assert_eq!(issues.latest(), Some("第二次"), "覆盖为最新文案");
    }

    /// 建议项 4：`Lost` 期间收到流事件，相位仍是 `Lost`，失联原因不被覆盖/清掉。
    #[test]
    fn lost_phase_keeps_its_reason_across_stream_events() {
        let mut issues = StreamIssues::default();
        issues.raise(timeline("A"), "timeline[A] 断开".into());
        let reason = "与 daemon 失联（20s 内无任何频道连接）——按 R 重连".to_string();

        let (phase, error) = reconcile_conn(&ConnPhase::Lost, &issues, Some(reason.clone()));
        assert_eq!(phase, ConnPhase::Lost);
        assert_eq!(
            error.as_deref(),
            Some(reason.as_str()),
            "流告警不得覆盖失联原因"
        );

        // 某条流恢复：失联相位与原因都不动。
        issues.clear(&timeline("A"));
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::Lost);
        assert!(
            error.is_some(),
            "Lost 分支靠 conn_error 给用户原因，不许被流恢复信号清掉"
        );
    }

    /// `Opening`：流告警不改相位（「连接中」已经表达了状态）。
    #[test]
    fn opening_phase_ignores_stream_alerts() {
        let mut issues = StreamIssues::default();
        issues.raise(channel(qaqh_client::Channel::Tool), "tool 断开".into());
        let (phase, _) = reconcile_conn(&ConnPhase::Opening, &issues, None);
        assert_eq!(phase, ConnPhase::Opening);
    }

    // ───────── App 层接线（端到端：经 App::handle 投递真实事件） ─────────

    /// M4（T15）：Ctrl+T 浮层按 `e` → 置位 pending_pager（main.rs 帧间消费）；
    /// 浮层保持打开（$PAGER 是附加浏览，不消耗浮层）。
    #[test]
    fn thinking_overlay_e_requests_pager() {
        let (mut app, _rx) = App::new_for_test();
        app.overlays.push(Overlay::Thinking {
            seed: "s".into(),
            scroll: 0,
            body: "第一段\n第二段".into(),
        });
        assert!(app.overlay_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
        assert_eq!(app.pending_pager.as_deref(), Some("第一段\n第二段"));
        assert!(matches!(
            app.overlays.last(),
            Some(Overlay::Thinking { .. })
        ));
    }

    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn conn(ev: ConnEvent) -> AppMsg {
        AppMsg::Runtime(RuntimeMsg::Conn(ev))
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> AppMsg {
        AppMsg::Key(KeyEvent::new(code, modifiers))
    }

    fn app_with_tabs(
        seeds: &[&str],
        active: usize,
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
        let (mut app, rx) = App::new_for_test();
        for seed in seeds {
            app.tabs.push((*seed).to_string());
            app.sessions
                .insert((*seed).to_string(), SessionState::new((*seed).to_string()));
        }
        app.active = active;
        (app, rx)
    }

    /// **App 层接线**（复审第一轮「建议 4」的原话）：`Lost` 期间收到流事件，
    /// 相位仍是 `Lost`、失联原因既不被清掉也不被覆盖。
    ///
    /// 证伪方式：把 `Lost` 分支改成「先清 conn_error」（旧行为）、或让
    /// `reconcile_conn_view` 无条件取账本文案（去掉 `Lost` 保护）——两条断言变红。
    #[test]
    fn handle_conn_keeps_lost_reason_across_stream_events() {
        let (mut app, _rx) = App::new_for_test();
        let reason = "与 daemon 失联（20s 内无任何频道连接）——按 R 重连";

        app.handle(conn(ConnEvent::Lost(reason.into())));
        assert_eq!(app.conn_phase, ConnPhase::Lost);
        assert_eq!(app.conn_error.as_deref(), Some(reason));

        // 某条流重连成功：失联相位与原因都不许动。
        app.handle(conn(ConnEvent::StreamRecovered {
            stream: channel(qaqh_client::Channel::Control),
        }));
        assert_eq!(
            app.conn_phase,
            ConnPhase::Lost,
            "流恢复信号不得把失联相位抹成正常"
        );
        assert!(
            app.conn_error.is_some(),
            "Lost 分支靠 conn_error 给用户原因，不许被清掉"
        );

        // 新到的流告警同样不许覆盖失联原因。
        app.handle(conn(ConnEvent::StreamIssue {
            stream: timeline("A"),
            error: "timeline[A] 断开，3000ms 后重连".into(),
        }));
        assert_eq!(app.conn_phase, ConnPhase::Lost);
        assert_eq!(
            app.conn_error.as_deref(),
            Some(reason),
            "流告警不得覆盖失联原因"
        );
    }

    /// **App 层接线**：新 session（`Ready`）清空流告警账本。
    ///
    /// 不这么做的话，旧 client 遗留的 timeline 告警（那条流再也不会发 `Open` 来撤
    /// 自己）会把相位永久钉在 `ReadyWithIssue`。此前这一行只有「接线靠编译」的保证。
    ///
    /// 证伪方式：删掉 `Ready` 分支里的 `self.stream_issues = StreamIssues::default();`
    /// ——`stream_issues.is_empty()` 断言立刻变红（相位也回不到 `Ready`）。
    #[test]
    fn handle_conn_ready_clears_the_stream_issue_ledger() {
        let (mut app, _rx) = App::new_for_test();

        // 连接就绪 → 某条流告警 → 相位进入 ReadyWithIssue。
        app.handle(conn(ConnEvent::Ready {
            epoch: "e1".into(),
            epoch_changed: false,
        }));
        assert_eq!(app.conn_phase, ConnPhase::Ready);
        app.handle(conn(ConnEvent::StreamIssue {
            stream: timeline("A"),
            error: "timeline[A] 断开".into(),
        }));
        assert_eq!(app.conn_phase, ConnPhase::ReadyWithIssue);
        assert!(!app.stream_issues.is_empty());

        // 新 session（epoch 变化）：账本整体作废，相位回 Ready。
        app.handle(conn(ConnEvent::Ready {
            epoch: "e2".into(),
            epoch_changed: true,
        }));
        assert!(
            app.stream_issues.is_empty(),
            "新 session 后旧代遗留的流告警必须清空"
        );
        assert_eq!(app.conn_phase, ConnPhase::Ready);
        assert!(app.conn_error.is_none());
    }

    /// **缺陷 2 的端到端版**：按 Alt+数字切标签时，绑旧 seed 的 `Confirm` 必须真的
    /// 从栈里消失，全局层留下；随后按 `y` 不会作用到旧 seed。
    ///
    /// 证伪方式：去掉 Alt+tab 分支里的 `prune_overlays_for_active_seed()`——确认框
    /// 留在栈里，`y` 会把 `CloseTab("A")` 落到 A 上（A 从 tabs 消失）。
    ///
    /// 断言分工（避免高估本用例）：前两条（栈里没有确认框、全局层留下）是**不变量**，
    /// 同一个变异下先红；最后那条按 `y` 是**后果**断言，说明这个不变量坏了会怎样伤害
    /// 用户，它不会独立于前两条失败。
    #[test]
    fn alt_tab_prunes_the_previous_seeds_confirm_overlay() {
        let (mut app, _rx) = app_with_tabs(&["A", "B"], 0);
        // Ctrl+W 的确认框（绑 A）+ 一个全局层（帮助）——全局层不许被误伤。
        app.overlays.push(Overlay::Confirm {
            action: ConfirmAction::CloseTab("A".into()),
        });
        app.overlays.push(Overlay::Help);

        // Alt+2 → 切到标签 B
        app.handle(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.active, 1, "Alt+2 应切到标签 B");
        assert!(
            !app.overlays
                .iter()
                .any(|o| matches!(o, Overlay::Confirm { .. })),
            "旧 seed 的确认框必须被剪掉：{:?}",
            app.overlays
        );
        assert!(
            matches!(app.overlays.as_slice(), [Overlay::Help]),
            "全局层必须留下：{:?}",
            app.overlays
        );

        // 再按 y：确认框已不在栈里，不该关掉 A。
        app.handle(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(
            app.tabs.iter().any(|t| t == "A"),
            "切标签后按 y 不得作用到旧 seed 的确认动作（A 被关了）"
        );
    }

    use crate::app::subagent::{SubagentEntry, SubagentState};
    use crate::runtime::TimelineLostReason;

    /// 父标签 + 一个正在跟踪（Running）的子代理。
    fn app_with_running_subagent() -> (App, String) {
        let (mut app, _rx) = App::new_for_test();
        let sub = "seed-sub".to_string();
        app.sessions
            .insert(sub.clone(), SessionState::new(sub.clone()));
        app.subagent_seeds.insert(sub.clone());
        let mut parent = SessionState::new("parent".into());
        parent.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: Some(sub.clone()),
            name: "explore".into(),
            state: SubagentState::Running,
        });
        app.sessions.insert("parent".into(), parent);
        (app, sub)
    }

    fn subagent_state(app: &App, seed: &str) -> SubagentState {
        app.sessions["parent"]
            .subagents
            .iter()
            .find(|e| e.seed.as_deref() == Some(seed))
            .expect("子代理条目存在")
            .state
    }

    /// issue #2 缺陷 1：非 404 的失败**不得**把仍在运行的子代理误标 `Closed`，
    /// 且必须留下可见提示（旧实现只按「是不是子代理」判定 → 静默收口）。
    #[test]
    fn timeline_lost_keeps_subagent_on_non_404_and_warns() {
        let (mut app, sub) = app_with_running_subagent();
        app.handle_runtime(RuntimeMsg::TimelineLost {
            seed: sub.clone(),
            reason: TimelineLostReason::Other(
                "HTTP 401: /ringing/v1/sessions/seed-sub/timeline".into(),
            ),
        });
        assert_eq!(
            subagent_state(&app, &sub),
            SubagentState::Running,
            "401/超时/网络错只说明这条流断了，会话可能还活着"
        );
        assert!(
            app.subagent_seeds.contains(&sub),
            "非 404 不得停止该 seed 的 timeline 跟踪"
        );
        assert!(
            app.toasts
                .iter()
                .any(|t| t.level == NoticeLevel::Error && t.text.contains(&sub)),
            "非 404 必须有可见提示（不能像旧实现那样 return 掉）"
        );
    }

    /// 404 = 会话确实不存在：这才收口为 `Closed` 并停止跟踪。
    #[test]
    fn timeline_lost_closes_subagent_on_404() {
        let (mut app, sub) = app_with_running_subagent();
        app.handle_runtime(RuntimeMsg::TimelineLost {
            seed: sub.clone(),
            reason: TimelineLostReason::SessionMissing,
        });
        assert_eq!(subagent_state(&app, &sub), SubagentState::Closed);
        assert!(!app.subagent_seeds.contains(&sub));
    }

    /// 判据矩阵：只有「子代理 + 404」收口；非子代理 seed 一律只提示。
    #[test]
    fn timeline_lost_effect_requires_404_and_subagent() {
        assert_eq!(
            timeline_lost_effect(true, &TimelineLostReason::SessionMissing),
            TimelineLostEffect::CloseSubagent
        );
        assert_eq!(
            timeline_lost_effect(true, &TimelineLostReason::Other("boom".into())),
            TimelineLostEffect::Notice
        );
        assert_eq!(
            timeline_lost_effect(false, &TimelineLostReason::SessionMissing),
            TimelineLostEffect::Notice
        );
    }

    /// issue #2 缺陷 3 的端到端路径（「后端主动关父」）：daemon 关掉一个
    /// **不是本地标签**的父会话时，`handle_control` 也必须走一遍回收——旧实现
    /// 被 `if self.tabs.contains(&seed)` 挡在门外，子代理永远留在跟踪集里。
    ///
    /// 这里刻意让父**本身也是子代理**（嵌套）：它挂在 `root` 名下，不在 tabs。
    #[test]
    fn closed_event_for_non_tab_parent_reclaims_subagents() {
        let (mut app, _rx) = App::new_for_test();
        let mut parent = SessionState::new("parent".into());
        parent.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: Some("sub".into()),
            name: "explore".into(),
            state: SubagentState::Running,
        });
        app.sessions.insert("parent".into(), parent);
        app.sessions
            .insert("sub".into(), SessionState::new("sub".into()));
        for s in ["parent", "sub"] {
            app.subagent_seeds.insert(s.into());
        }
        let mut root = SessionState::new("root".into());
        root.subagents.push(SubagentEntry {
            tool_call_id: "c0".into(),
            seed: Some("parent".into()),
            name: "nested".into(),
            state: SubagentState::Running,
        });
        app.sessions.insert("root".into(), root);
        assert!(
            !app.tabs.contains(&"parent".to_string()),
            "前提：父不是本地标签"
        );

        app.handle(AppMsg::Runtime(v2_event(
            "parent",
            qaqh_client::ClientV2Payload::MetaDelta(qaqh_client::ClientV2MetaDelta::Deleted {
                revision: 1,
                tombstone_at_ms: 0,
                reason: qaqh_client::ClientV2DeleteReason::User,
                purge_after_ms: None,
            }),
        )));

        assert!(
            !app.subagent_seeds.contains("sub") && !app.subagent_seeds.contains("parent"),
            "daemon 主动关父时必须回收它自己与它的子代理"
        );
        assert!(!app.sessions.contains_key("sub"));
        assert_eq!(
            app.sessions["root"].subagents[0].state,
            SubagentState::Closed,
            "会话已消失 → 挂在 root 名下的父条目收口为 Closed"
        );
    }

    #[tokio::test]
    async fn opening_todo_workspace_refreshes_dashboard() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));
        app.show_workspace = false;

        app.handle(AppMsg::Key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::F(4),
            ratatui::crossterm::event::KeyModifiers::NONE,
        )));

        assert!(app.show_workspace);
        assert!(app.dashboard_fetching.contains("seed"));
    }

    #[tokio::test]
    async fn todo_tool_update_refreshes_dashboard() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));
        let tool: qaqh_client::TimelineTool = serde_json::from_value(serde_json::json!({
            "tool_call_id": "call-todo-1",
            "name": "todo_write",
            "state": "running"
        }))
        .expect("tool");
        let entry = qaqh_client::TimelineEntry {
            timeline_seq: 1,
            turn_id: "turn-1".into(),
            round_num: Some(0),
            event: qaqh_client::TimelineEvent::ToolUpdated {
                block_id: "todo-1".into(),
                tool,
            },
        };

        app.handle(AppMsg::Runtime(RuntimeMsg::Timeline {
            seed: "seed".into(),
            entry: Box::new(entry),
        }));

        assert!(app.dashboard_fetching.contains("seed"));
    }

    /// **渲染缓存纪律回归**（机主实测 ⟂ 峰值 466 的根因）：
    /// dashboard（todo / 最近改动）只被 workspace 侧栏消费
    /// （`ui/sidebar.rs` 直读 `sess.dashboard`），与 transcript 渲染缓存无关；
    /// 而它在工具调用期高频更新——清缓存 = 每次工具调用整缓存重建
    /// （实测 ⟂ 峰值 466 ≈ 全量重渲）。
    ///
    /// 证伪方式：把任一处 `sess.block_cache = None` 加回 dashboard 路径 → 本测试红。
    #[test]
    fn dashboard_updates_must_not_clear_transcript_cache() {
        let (mut app, _rx) = app_with_tabs(&["seed"], 0);
        app.sessions.get_mut("seed").unwrap().block_cache =
            Some(crate::app::render::TranscriptCache::new(80));

        let snapshot = || qaqh_client::DomainDashboardSnapshot {
            seed: "seed".into(),
            documents: Vec::new(),
            recent_edits: vec!["src/lib.rs".into()],
            tasks: Vec::new(),
            current_todo_id: None,
        };

        // ① v2 投影没有 dashboard 增量（v1 的 control DashboardSnapshot 已随
        //    三频道流删除）——workspace 面板只走下面的 service 拉取路径。

        // ② service 拉取兜底路径（DashboardUpdated → fetch_dashboard 的结果）。
        app.handle(AppMsg::Action(ActionResult::Dashboard {
            seed: "seed".into(),
            result: Ok(snapshot()),
        }));
        assert!(
            app.sessions["seed"].block_cache.is_some(),
            "Dashboard 结果不得清空 transcript 渲染缓存"
        );
    }
}
