//! App 状态机：输入路由、wire 事件消费、命令发送、覆盖层。
//!
//! 事件驱动（无轮询泵）：所有后端状态变化经 runtime 消息到达后立即生效，
//! UI 在同一帧内重绘。

pub(crate) mod anim;
mod composer_ops;
mod export;
mod interaction;
pub(crate) mod keymap;
pub mod markdown;
mod overlay_ops;
pub(crate) mod pager;
mod paste_guard;
pub(crate) mod render;
pub mod render_line;
pub mod render_transcript;
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
use qaqh_client::{
    AskResolution, ContentRef, ControlEvent, ConversationEvent,
    DomainActivityState as ActivityState, DomainSessionState as SessionStateEvent, NoticeLevel,
    PermissionCategory, PermissionRisk, ToolEvent,
};
use qaqh_client::{ControlCommand, ConversationCommand, RingingCommand, ToolCommand};
use qaqh_client::{RingingCommandState as CommandState, RingingCommandStatus};
use qaqh_client::{SessionActivity, SessionListEntry};
use session::{
    AskPanel, PermissionPanel, PlanPanel, SessionState, StreamPhase, streaming_done,
    sync_streaming_from_timeline,
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
        result: Result<qaqh_client::RingingSessionBootstrap, String>,
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

    /// 会话 bootstrap（三频道快照原子恢复）。
    pub async fn bootstrap(
        &self,
        seed: &str,
    ) -> Result<qaqh_client::RingingSessionBootstrap, String> {
        self.client()?
            .bootstrap(seed)
            .await
            .map_err(|e| e.to_string())
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
            Overlay::SessionList { .. }
            | Overlay::Settings(_)
            | Overlay::Help
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

    pub toasts: VecDeque<Toast>,
    /// 新建会话的 command_id → 发起时间（等 causation_id 关联）。
    pub pending_creates: HashMap<String, Instant>,
    pub session_list_cache: Vec<SessionListEntry>,
    pub session_list_at: Option<Instant>,
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
            toasts: VecDeque::new(),
            pending_creates: HashMap::new(),
            session_list_cache: Vec::new(),
            session_list_at: None,
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
        // 首页自动刷新：无 tab 时保持列表新鲜（对齐 opencode Home 的常驻列表感）
        if self.tabs.is_empty() {
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
            RuntimeMsg::Ringing { env } => self.handle_envelope(*env),
            RuntimeMsg::ResetRequired { seed } => {
                // 频道级 reset → 重新 bootstrap 该会话（timeline 流自会 re-baseline）。
                let seed2 = seed.clone();
                self.spawn_api(move |api, tx| async move {
                    let result = api.bootstrap(&seed2).await;
                    let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap {
                        seed: seed2,
                        result,
                    }));
                });
            }
            RuntimeMsg::Timeline { seed, entry } => {
                // 子代理发现：spawn_subagent 工具卡（增量，先于 apply 检查）。
                if let qaqh_client::TimelineEvent::ToolUpdated { tool, .. } = &entry.event {
                    self.discover_spawn_tool(&seed, tool);
                }
                let Some(sess) = self.sessions.get_mut(&seed) else {
                    return;
                };
                // TurnSealed 是 timeline 上的权威终态：立即收口 streaming，不等
                // 对话频道 TurnCompleted（两条独立 SSE，可能乱序或丢失）。
                if let Some(terminal) = sess.timeline.apply(&entry) {
                    streaming_done(sess, Some(&terminal.turn_id));
                }
                sync_streaming_from_timeline(sess);
                if !sess.scroll.follow {
                    // 非跟随模式：内容增长等价于视口上移。
                    sess.scroll.offset = sess.scroll.offset.saturating_add(0);
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

    fn handle_envelope(&mut self, env: qaqh_client::RingingEventEnvelope) {
        let seed = env.seed.clone();
        let causation_id = env.causation_id.clone();
        match env.event {
            qaqh_client::RingingEvent::Control(ev) => self.handle_control(seed, causation_id, ev),
            qaqh_client::RingingEvent::Conversation(ev) => self.handle_conversation(seed, ev),
            qaqh_client::RingingEvent::Tool(ev) => self.handle_tool(seed, ev),
        }
    }

    // ───────────────────────── 控制频道事件 ─────────────────────────

    fn handle_control(&mut self, seed: String, causation_id: Option<String>, ev: ControlEvent) {
        match ev {
            ControlEvent::SessionStateChanged { state, .. } => {
                match state {
                    SessionStateEvent::Created => {
                        // 新会话经信封 causation_id == command_id 关联（不轮询列表）。
                        if let Some(cid) = causation_id
                            && self.pending_creates.remove(&cid).is_some()
                        {
                            self.open_session_tab(&seed);
                            self.toast(NoticeLevel::Info, format!("新会话已创建 {seed}"));
                        }
                    }
                    SessionStateEvent::Resumed => {}
                    SessionStateEvent::Closed
                    | SessionStateEvent::Archived
                    | SessionStateEvent::Deleted => {
                        // 无条件走一遍：daemon 主动关的**父**会话未必是本地标签
                        // （父本身是子代理时尤其如此），但它的子代理必须跟着回收。
                        let was_tab = self.tabs.contains(&seed);
                        self.close_tab_by_seed(&seed);
                        if was_tab {
                            let verb = match state {
                                SessionStateEvent::Archived => "已归档",
                                SessionStateEvent::Deleted => "已删除",
                                _ => "已关闭",
                            };
                            self.toast(NoticeLevel::Info, format!("会话 {seed} {verb}"));
                        }
                        self.session_list_at = None; // 触发会话列表刷新
                    }
                    _ => {}
                }
            }
            ControlEvent::SessionActivityChanged { state, .. } => {
                self.activity_cache.insert(seed.clone(), state);
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.activity = Some(state);
                }
                if state == ActivityState::WaitingUser && !self.tabs.contains(&seed) {
                    self.toast(NoticeLevel::Warn, format!("会话 {seed} 等待输入"));
                }
            }
            ControlEvent::SessionMetaChanged { title, .. } => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && let Some(t) = title.clone()
                {
                    sess.title = Some(t);
                }
                self.session_list_at = None;
            }
            ControlEvent::ConfigChanged { .. } => {
                // 任何 config.*/profile.* 写路径的广播（seed=""）：重拉 typed 快照，
                // 保留设置页草稿（脏字段展示优先于 loaded——B5 回声教训），
                // 并复位端口候选（应用后跟随服务端现值）。
                if let Some(Overlay::Settings(st)) = self.overlays.last_mut() {
                    st.profile_sel = None;
                }
                if self
                    .overlays
                    .iter()
                    .any(|o| matches!(o, Overlay::Settings(_)))
                {
                    self.fetch_config();
                }
            }
            ControlEvent::InteractionRequested {
                interaction_id,
                turn_id,
                mode,
                questions,
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.pending_ask =
                        Some(AskPanel::new(interaction_id, turn_id, mode, questions));
                    sess.scroll.follow = true;
                }
            }
            ControlEvent::InteractionResolved {
                resolution,
                interaction_id,
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && sess
                        .pending_ask
                        .as_ref()
                        .is_some_and(|p| p.interaction_id == interaction_id)
                {
                    sess.pending_ask = None;
                    let _ = resolution;
                }
                if resolution == AskResolution::Dismissed {
                    self.toast(NoticeLevel::Warn, format!("ask 已跳过 [{seed}]"));
                }
            }
            ControlEvent::PlanReviewRequested {
                interaction_id,
                turn_id,
                plan_content,
                review_type,
                todo_items,
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.pending_plan = Some(PlanPanel {
                        interaction_id,
                        turn_id,
                        plan_content,
                        review_type,
                        todo_items: todo_items.unwrap_or_default(),
                        message: String::new(),
                        entering_message: false,
                        scroll: 0,
                    });
                }
            }
            ControlEvent::PlanReviewResolved {
                interaction_id,
                approved,
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && sess
                        .pending_plan
                        .as_ref()
                        .is_some_and(|p| p.interaction_id == interaction_id)
                {
                    sess.pending_plan = None;
                }
                self.toast(
                    if approved {
                        NoticeLevel::Info
                    } else {
                        NoticeLevel::Warn
                    },
                    format!("plan review {}", if approved { "已批准" } else { "已拒绝" }),
                );
            }
            ControlEvent::SkillsUpdated {
                available,
                active,
                runtime,
                ..
            } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    // 权威 `SkillsStatus` **没有** `Default`：这里显式补齐服务端没
                    // 随事件下发的字段（而非用 `..Default::default()` 掩盖「我们其实
                    // 不知道」——零值在这里就是「未知」，写出来更诚实）。
                    sess.skills = Some(qaqh_client::SkillsStatus {
                        available,
                        active,
                        catalog_revision: String::new(),
                        context_epoch: 0,
                        operation_revision: 0,
                        token_budget: 0,
                        token_usage: 0,
                        runtime,
                        diagnostics: Vec::new(),
                    });
                }
            }
            ControlEvent::SystemNotice { level, message, .. } => {
                self.toast(level, format!("[system] {message}"));
            }
            ControlEvent::AgentLifecycleChanged { .. } => {}
            ControlEvent::DashboardSnapshot { snapshot } => {
                // 容错：envelope seed 可能与 snapshot.seed 不一致（旧 daemon/重连时序），
                // 优先 envelope seed，兜底 snapshot.seed。
                let target = if self.sessions.contains_key(&seed) {
                    seed.clone()
                } else if self.sessions.contains_key(&snapshot.seed) {
                    snapshot.seed.clone()
                } else {
                    seed.clone()
                };
                // 不得清 `block_cache`：dashboard 只被 workspace 侧栏消费
                // （`ui/sidebar.rs` 直接读 `sess.dashboard`），与 transcript 渲染
                // 缓存无关；而 dashboard 在工具调用期高频更新（todo/最近改动），
                // 清缓存会让每次工具调用触发整缓存重建（实测 ⟂ 峰值 466 ≈ 全量）。
                if let Some(sess) = self.sessions.get_mut(&target) {
                    sess.dashboard = Some(snapshot);
                } else if self.sessions.contains_key(&snapshot.seed)
                    && let Some(sess) = self.sessions.get_mut(&snapshot.seed)
                {
                    sess.dashboard = Some(snapshot);
                }
                // replaceable 空快照（tasks=[]）时：老 daemon/丢帧后仍为空，主动回退 service 拉取。
                let needs_fallback = self
                    .sessions
                    .get(&target)
                    .and_then(|s| s.dashboard.as_ref())
                    .is_some_and(|d| {
                        d.tasks.is_empty() && d.recent_edits.is_empty() && d.documents.is_empty()
                    });
                if needs_fallback {
                    self.fetch_dashboard(target.clone());
                }
            }
            ControlEvent::DashboardUpdated { session_seed, .. } => {
                let target = if session_seed.is_empty() {
                    seed.clone()
                } else {
                    session_seed.clone()
                };
                let needs_fetch = if self.sessions.contains_key(&target) {
                    let sess = &self.sessions[&target];
                    sess.dashboard.is_none()
                        || sess
                            .dashboard
                            .as_ref()
                            .is_some_and(|d| d.tasks.is_empty() && d.documents.is_empty())
                } else {
                    false
                };
                if needs_fetch {
                    self.fetch_dashboard(target);
                }
            }
            ControlEvent::SubagentStatus { name, state, .. } => {
                // 终态标签（COMPLETED/ERROR/TIMEOUT/CANCELLED）：同步条目并
                // 停止对应 seed 的 timeline 跟踪（daemon 随后 SessionClose）。
                let mut done_seed = None;
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && let Some(s) = subagent::apply_status(sess, &name, &state)
                {
                    done_seed = Some(s);
                }
                if let Some(s) = done_seed {
                    self.untrack_subagent(&s);
                }
                self.toast(NoticeLevel::Info, format!("子代理 {name}: {state}"));
            }
            ControlEvent::OperationFailed { scope, error, .. } => {
                self.toast(
                    NoticeLevel::Error,
                    format!("失败[{:?}] {}: {}", scope, error.code, error.message),
                );
                // 鬼影清理（winui 教训）：ask 被拒/交互不存在 → 清挂起面板。
                if matches!(
                    error.code.as_str(),
                    "ask_rejected" | "interaction_not_found"
                ) && let Some(sess) = self.sessions.get_mut(&seed)
                {
                    sess.pending_ask = None;
                    sess.pending_plan = None;
                }
            }
            ControlEvent::OperationCompleted { .. } => {}
        }
    }

    // ───────────────────────── 对话频道事件 ─────────────────────────

    fn handle_conversation(&mut self, seed: String, ev: ConversationEvent) {
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        match ev {
            ConversationEvent::TurnStarted { turn_id, .. } => {
                sess.streaming = Some(session::StreamingState {
                    turn_id,
                    phase: StreamPhase::Thinking,
                    round_num: 0,
                    tool_name: None,
                    armed_at: Instant::now(),
                });
                sess.last_error = None;
                sess.scroll.follow = true;
                sess.scroll.offset = 0;
            }
            ConversationEvent::TurnCompleted { usage, turn_id, .. } => {
                streaming_done(sess, Some(&turn_id));
                if let Some(u) = usage
                    && let Some(conv) = sess.conversation.as_mut()
                {
                    conv.usage = Some(u);
                }
            }
            ConversationEvent::TurnFailed { turn_id, error } => {
                streaming_done(sess, Some(&turn_id));
                sess.last_error = Some(error.clone());
                self.toast(
                    NoticeLevel::Error,
                    format!("回合失败: {}: {}", error.code, error.message),
                );
            }
            ConversationEvent::RoundDelta {
                round_num, kind, ..
            } => {
                if let Some(s) = sess.streaming.as_mut() {
                    s.round_num = round_num;
                    s.phase = match kind {
                        qaqh_client::RoundDeltaKind::Thinking => StreamPhase::Thinking,
                        qaqh_client::RoundDeltaKind::ToolCalling => StreamPhase::ToolCalling,
                        qaqh_client::RoundDeltaKind::Answering => StreamPhase::Answering,
                    };
                }
            }
            ConversationEvent::BlockCheckpoint { .. } => {}
            ConversationEvent::RoundCompleted { .. } => {}
            ConversationEvent::ProviderRetrying {
                attempt,
                max_retries,
                error_message,
                ..
            } => {
                self.toast(
                    NoticeLevel::Warn,
                    format!(
                        "provider 重试 {attempt}/{max_retries}: {}",
                        truncate_str(&error_message, 60)
                    ),
                );
            }
            ConversationEvent::ProviderToolStatus { state, .. } => {
                if let Some(s) = sess.streaming.as_mut() {
                    s.phase = match state {
                        qaqh_client::ProviderToolState::Completed => StreamPhase::Answering,
                        _ => StreamPhase::ToolCalling,
                    };
                }
            }
            ConversationEvent::UsageUpdated {
                usage,
                context_limit,
                model,
                ..
            } => {
                sess.apply_usage(usage, context_limit, model);
            }
            ConversationEvent::CompactStarted {
                turns_total,
                turns_keeping,
                ..
            } => {
                sess.compact_anim = Some(crate::app::session::CompactionAnim {
                    started_at: Instant::now(),
                    turns_total,
                    turns_keeping,
                    last_delta: None,
                });
            }
            ConversationEvent::CompactProgress { delta, .. } => {
                if let Some(anim) = &mut sess.compact_anim {
                    anim.last_delta = Some(delta);
                }
            }
            ConversationEvent::CompactFinished {
                status,
                turns_compacted,
                ..
            } => {
                sess.compact_anim = None;
                self.toast(
                    match status {
                        qaqh_client::CompactStatus::Completed => NoticeLevel::Info,
                        _ => NoticeLevel::Warn,
                    },
                    format!(
                        "compact {}: {:?}",
                        if status == qaqh_client::CompactStatus::Completed {
                            "完成"
                        } else {
                            "未完成"
                        },
                        turns_compacted
                    ),
                );
            }
            ConversationEvent::ConversationCancelled { turn_id } => {
                streaming_done(sess, turn_id.as_deref());
                self.toast(NoticeLevel::Info, "回合已取消");
            }
        }
    }

    // ───────────────────────── 工具频道事件 ─────────────────────────

    fn handle_tool(&mut self, seed: String, ev: ToolEvent) {
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        match ev {
            ToolEvent::ToolPermissionRequested {
                tool_call_id,
                tool_name,
                action_summary,
                reason,
                paths,
                category,
                level,
                risk,
                consequence,
                ..
            } => {
                // 去重：同一 tool_call 只保留一个面板。
                // 且已解决过的 tool_call 不再入队——补投（`ToolStarted` 之后才到的
                // 权限请求）不得复活幽灵面板，见 `SessionState::queue_permission`。
                sess.queue_permission(PermissionPanel {
                    tool_call_id,
                    tool_name,
                    action_summary,
                    reason,
                    paths,
                    category,
                    level,
                    risk,
                    consequence,
                    trust_folder: false,
                });
            }
            // daemon 无独立 permission-resolved 事件：以 Started/Finished 兜底清除。
            ToolEvent::ToolStarted {
                tool_call_id, name, ..
            } => {
                sess.resolve_permission(&tool_call_id);
                if let Some(s) = sess.streaming.as_mut() {
                    s.phase = StreamPhase::ToolCalling;
                    s.tool_name = Some(name);
                }
            }
            ToolEvent::ToolFinished { tool_call_id, .. } => {
                sess.resolve_permission(&tool_call_id);
            }
            ToolEvent::ToolNotice { level, message, .. } => {
                self.toast(level, format!("[tool] {message}"));
            }
            ToolEvent::CodeChanged {
                lines_added,
                lines_removed,
                ..
            } => {
                sess.code_added += lines_added;
                sess.code_removed += lines_removed;
            }
            ToolEvent::ToolCallPrepared { .. } | ToolEvent::AuditRecorded { .. } => {}
        }
    }

    // ───────────────────────── 后台结果 ─────────────────────────

    fn handle_action(&mut self, action: ActionResult) {
        match action {
            ActionResult::Bootstrap { seed, result } => match result {
                Ok(b) => {
                    let bootstrap_seed = seed.clone();
                    let mut needs_fetch = false;
                    if let Some(sess) = self.sessions.get_mut(&bootstrap_seed) {
                        // G1：用权威类型化视图，本仓不再手解三频道 state。
                        // `unwrap_or_default` 不隐藏问题——权威类型每个字段都带
                        // `#[serde(default)]`，形状漂移表现为「字段变缺省」，而漂移由
                        // 后端的产出方往返测试兜住（`typed_state_views_recover_every_producer_field`）；
                        // 真走到 `Err` 已是 state 根本不是对象的病态情形。
                        let conv = b.conversation_state().unwrap_or_default();
                        sess.usage = conv.usage.clone();
                        sess.usage_totals = conv.usage_totals.clone();
                        sess.context_limit =
                            conv.context_limit.map(|v| v.min(u32::MAX as u64) as u32);
                        let model = conv.model.clone();
                        sess.conversation = Some(conv);
                        let ctl = b.control_state().unwrap_or_default();
                        sess.activity = ctl.activity.or(sess.activity);
                        // bootstrap 是 control 域快照的权威刷新点；这里与 timeline
                        // 收敛一次，避免上一次连接遗留的 Working/Starting 与
                        // streaming 状态把 UI 钉在 working（timeline 空则不误判）。
                        sync_streaming_from_timeline(sess);
                        // 这里原有「从 SessionState::meta 的 mode 位同步会话模式」一段，
                        // 因 meta 恒为 None 从未生效，G2 随该字段一并删除。会话模式的实际
                        // 来源是 transcript_ops.rs 的乐观更新 + SessionMetaChanged 刷新。
                        match ctl.dashboard_snapshot {
                            Some(dash) => {
                                let is_empty = dash.tasks.is_empty()
                                    && dash.documents.is_empty()
                                    && dash.recent_edits.is_empty();
                                sess.dashboard = Some(dash);
                                needs_fetch = is_empty;
                            }
                            None => {
                                needs_fetch = true;
                            }
                        }
                        let tool = b.tool_state().unwrap_or_default();
                        if let Some(perm) = tool.pending_permission {
                            // bootstrap 恢复挂起权限（详情等 tool 事件补全）。只补不换：
                            // 快照可能落后于实时事件，不许用「（恢复中）」占位符覆盖
                            // 已有面板的详情，也不许复活已解决的 id。
                            sess.restore_permission_from_snapshot(PermissionPanel {
                                tool_call_id: perm,
                                tool_name: "（恢复中）".into(),
                                action_summary: None,
                                reason: String::new(),
                                paths: vec![],
                                category: PermissionCategory::Read,
                                level: 0,
                                risk: PermissionRisk::Medium,
                                consequence: String::new(),
                                trust_folder: false,
                            });
                        }
                        if let Some(m) = model {
                            let _ = m;
                        }
                        sess.block_cache = None;
                    }
                    if needs_fetch {
                        self.fetch_dashboard(bootstrap_seed);
                    }
                }
                Err(e) => self.toast(NoticeLevel::Error, format!("bootstrap 失败[{seed}]: {e}")),
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

        // 退出与全局键：先经 keymap 纯映射（可单测），再做状态副作用。
        match keymap::map_global_key(&key) {
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
                self.new_session();
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
                self.show_workspace = !self.show_workspace;
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

        // 首页（无 tab 且无覆盖层时，会话列表即首页）
        if self.tabs.is_empty() && self.home_key(key) {
            return;
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
    use qaqh_client::{RingingCommandAck, RingingCommandAckStatus};

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
        StreamKey::Channel(c)
    }

    fn timeline(seed: &str) -> StreamKey {
        StreamKey::Timeline(seed.into())
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

        app.handle_control(
            "parent".into(),
            None,
            ControlEvent::SessionStateChanged {
                seed: "parent".into(),
                state: SessionStateEvent::Closed,
            },
        );

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

        // ① control 频道推送（daemon 主动推，工具调用期高频）。
        let env = qaqh_client::RingingEventEnvelope::new(
            "seed",
            1,
            1,
            1,
            "ev-dash-1",
            qaqh_client::RingingEvent::Control(ControlEvent::DashboardSnapshot {
                snapshot: snapshot(),
            }),
        );
        app.handle(AppMsg::Runtime(RuntimeMsg::Ringing { env: Box::new(env) }));
        assert!(
            app.sessions["seed"].dashboard.is_some(),
            "dashboard 必须被应用"
        );
        assert!(
            app.sessions["seed"].block_cache.is_some(),
            "DashboardSnapshot 不得清空 transcript 渲染缓存"
        );

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
