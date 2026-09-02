//! App 状态机：输入路由、wire 事件消费、命令发送、覆盖层。
//!
//! 事件驱动（无轮询泵）：所有后端状态变化经 runtime 消息到达后立即生效，
//! UI 在同一帧内重绘。

pub(crate) mod keymap;
pub(crate) mod anim;
mod composer_ops;
mod paste_guard;
mod interaction;
mod overlay_ops;
mod session_ops;
mod settings_ops;
mod transcript_ops;
pub mod markdown;
pub mod render_line;
pub mod render_transcript;
pub mod session;
pub mod settings;
pub mod slash;
pub mod timeline_model;

use self::keymap::{GlobalKey, ModalRoute};
use self::paste_guard::PasteGuard;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::protocol::command::{
    ConversationCommand, ControlCommand, RingingCommand, ToolCommand,
};
use crate::app::slash::SlashCmd;
use crate::protocol::config::ConfigDto;
use crate::protocol::envelope::{CommandState, RingingCommandStatus};
use crate::protocol::event::{
    ActivityState, AskResolution, ContentRef, ConversationEvent, ControlEvent, NoticeLevel,
    PermissionCategory, PermissionRisk, SessionState as SessionStateEvent, ToolEvent,
};
use crate::protocol::methods::{self, SessionMetaView};
use crate::protocol::timeline::TimelinePage;
use crate::runtime::{ConnEvent, Runtime, RuntimeMsg};
use crate::transport::http::{build_envelope, HttpClient};
use session::{
    streaming_done, sync_streaming_from_timeline, AskPanel, PermissionPanel, PlanPanel,
    SessionState, StreamPhase,
};

/// 保留 timeline 模型的最近焦点标签数（LRU；超出者仅存轻状态，
/// 重新聚焦时 re-baseline 重建 transcript）。对照 opencode sync 的
/// "进入会话全量重取 + 滑动窗口" 策略。
const ACTIVE_MODELS: usize = 4;
/// 单会话内存中的回合窗口上限（timeline 是服务端权威，内存只是视窗）。
const TURNS_CAP: usize = 400;

/// app 后台任务回传的结果。
// 大变体承载完整协议响应；Box 化属性能优化，推迟到独立任务（不影响正确性）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ActionResult {
    Bootstrap { seed: String, result: Result<crate::protocol::snapshot::RingingSessionBootstrap, String> },
    CommandAck { seed: Option<String>, label: &'static str, result: Result<crate::protocol::envelope::RingingCommandAck, String> },
    SessionList(Result<Vec<SessionMetaView>, String>),
    SessionActivity(Result<serde_json::Value, String>),
    ConfigLoaded(Result<serde_json::Value, String>),
    ConfigWrite { label: &'static str, result: Result<serde_json::Value, String> },
    Uploaded { seed: String, path: String, result: Result<ContentRef, String> },
    Rebaseline { seed: String, result: Result<TimelinePage, String> },
    LoadOlder { seed: String, result: Result<TimelinePage, String> },
    Receipt { label: &'static str, seed: Option<String>, result: Result<RingingCommandStatus, String> },
    Dashboard { seed: String, result: Result<crate::protocol::event::DashboardSnapshot, String> },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnPhase {
    Opening,
    Ready,
    Lost,
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
    SessionList { selected: usize, show_archived: bool },
    Settings(settings::SettingsState),
    Help,
    AttachPath { input: Vec<char>, cursor: usize, seed: String },
    Confirm { action: ConfirmAction },
    /// 二级：/new 的 cwd 输入（/ 本身的一级菜单为 inline 浮层，非 overlay）
    CwdInput { input: Vec<char>, cursor: usize },
}

use self::settings::{FieldKind, SettingsState};

pub struct App {
    pub quit: bool,
    pub client: Arc<HttpClient>,
    pub runtime: Arc<Runtime>,
    pub msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,

    pub tabs: Vec<String>,
    pub sessions: HashMap<String, SessionState>,
    pub active: usize,

    pub overlays: Vec<Overlay>,
    pub conn_phase: ConnPhase,
    pub epoch: String,
    pub conn_error: Option<String>,

    pub toasts: VecDeque<Toast>,
    /// 新建会话的 command_id → 发起时间（等 causation_id 关联）。
    pub pending_creates: HashMap<String, Instant>,
    pub session_list_cache: Vec<SessionMetaView>,
    pub session_list_at: Option<Instant>,
    pub activity_cache: HashMap<String, ActivityState>,
    dashboard_fetching: HashSet<String>,
    /// config.load 的 typed 快照（ConfigDto 镜像；ConfigChanged 到达时重拉）。
    pub config: Option<ConfigDto>,
    /// settings 保存请求在途标记（防 config.save 双发——事故 R4）。
    pub settings_saving: bool,
    pub show_reasoning: bool,
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
}

impl App {
    pub fn new(
        client: Arc<HttpClient>,
        runtime: Arc<Runtime>,
        msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
    ) -> Self {
        Self::new_with_cwd(client, runtime, msg_tx, std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
    }

    pub fn new_with_cwd(
        client: Arc<HttpClient>,
        runtime: Arc<Runtime>,
        msg_tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
        initial_cwd: Option<String>,
    ) -> Self {
        Self {
            quit: false,
            client,
            runtime,
            msg_tx,
            tabs: Vec::new(),
            sessions: HashMap::new(),
            active: 0,
            overlays: Vec::new(),
            conn_phase: ConnPhase::Opening,
            epoch: String::new(),
            conn_error: None,
            toasts: VecDeque::new(),
            pending_creates: HashMap::new(),
            session_list_cache: Vec::new(),
            session_list_at: None,
            activity_cache: HashMap::new(),
            dashboard_fetching: HashSet::new(),
            config: None,
            settings_saving: false,
            show_reasoning: true,
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
                    s.rendered = None;
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
        self.pending_creates.retain(|_, at| at.elapsed() < Duration::from_secs(15));
        if let Some(armed) = self.quit_armed
            && armed.elapsed() > Duration::from_secs(3) {
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
            MouseEventKind::Down(kind) if kind == ratatui::crossterm::event::MouseButton::Left
                && m.row == 0 => {
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
            && let Some(buf) = st.editing.as_mut() {
                for ch in text.chars() {
                    if ch != '\n' && ch != '\r' {
                        buf.buf.insert(buf.cursor.min(buf.buf.len()), ch);
                        buf.cursor += 1;
                    }
                }
                return;
            }
        let Some(sess) = self.active_session_mut() else { return };
        if let Some(panel) = sess.pending_ask.as_mut()
            && panel.editing_custom.is_some() {
                panel.input.push_str(&text);
                return;
            }
        if let Some(panel) = sess.pending_plan.as_mut()
            && panel.entering_message {
                panel.message.push_str(&text);
                return;
            }
        sess.composer.insert_str(&text);
    }

    fn handle_runtime(&mut self, msg: RuntimeMsg) {
        match msg {
            RuntimeMsg::Conn(ev) => self.handle_conn(ev),
            RuntimeMsg::Ringing { channel, env } => self.handle_envelope(channel, *env),
            RuntimeMsg::ResetRequired { seed, .. } => {
                // 频道级 reset → 重新 bootstrap 该会话（timeline 流自会 re-baseline）。
                let seed2 = seed.clone();
                self.spawn_api(move |client, tx| async move {
                    let result = client.bootstrap(&seed2).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed: seed2, result }));
                });
            }
            RuntimeMsg::Timeline { seed, entry } => {
                let Some(sess) = self.sessions.get_mut(&seed) else { return };
                sess.timeline.apply(&entry);
                sess.timeline.cap_turns(TURNS_CAP);
                sync_streaming_from_timeline(sess);
                if !sess.scroll.follow {
                    // 非跟随模式：内容增长等价于视口上移。
                    sess.scroll.offset = sess.scroll.offset.saturating_add(0);
                }
            }
            RuntimeMsg::TimelineRebaseline { seed, page } => {
                let Some(sess) = self.sessions.get_mut(&seed) else { return };
                // 重基线 = 权威时间线已就绪：压缩动画兜底清除。
                sess.compact_anim = None;
                let was_follow = sess.scroll.follow;
                let first_load = !sess.ready;
                sess.needs_rebaseline = false;
                sess.timeline.replace_from_page(&page);
                sess.timeline.cap_turns(TURNS_CAP);
                sess.ready = true;
                sync_streaming_from_timeline(sess);
                if first_load || was_follow {
                    sess.scroll.follow = true;
                    sess.scroll.offset = 0;
                }
                sess.rendered = None;
            }
            RuntimeMsg::TimelineLost { seed, error } => {
                self.toast(NoticeLevel::Error, format!("timeline 断开[{seed}]: {error}"));
            }
        }
    }

    fn handle_conn(&mut self, ev: ConnEvent) {
        match ev {
            ConnEvent::Opening => {
                self.conn_phase = ConnPhase::Opening;
            }
            ConnEvent::Ready { epoch, epoch_changed } => {
                self.conn_phase = ConnPhase::Ready;
                if self.epoch != epoch {
                    self.epoch = epoch.clone();
                }
                self.conn_error = None;
                // 重 open（租约重建 / daemon 重启）：重新 attach 全部 open seeds
                // 并 re-baseline；epoch 变化时 timeline 流自行重放。
                let seeds = self.tabs.clone();
                if !seeds.is_empty() {
                    self.spawn_api(move |client, tx| async move {
                        for seed in seeds {
                            let cmd = build_envelope(
                                &client,
                                RingingCommand::Control(ControlCommand::SessionResume { seed: seed.clone() }),
                            )
                            .with_seed(seed.clone());
                            if let Err(e) = client.command(&cmd).await {
                                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                                    seed: Some(seed.clone()),
                                    label: "resume",
                                    result: Err(e.to_string()),
                                }));
                                continue;
                            }
                            let result = client.bootstrap(&seed).await.map_err(|e| e.to_string());
                            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
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
            ConnEvent::StreamIssue { error, .. } => {
                self.conn_error = Some(error);
            }
        }
    }

    fn handle_envelope(&mut self, _channel: crate::protocol::Channel, env: crate::protocol::envelope::RingingEventEnvelope) {
        let seed = env.seed.clone();
        let causation_id = env.causation_id.clone();
        match env.event {
            crate::protocol::event::RingingEvent::Control(ev) => self.handle_control(seed, causation_id, ev),
            crate::protocol::event::RingingEvent::Conversation(ev) => self.handle_conversation(seed, ev),
            crate::protocol::event::RingingEvent::Tool(ev) => self.handle_tool(seed, ev),
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
                            && self.pending_creates.remove(&cid).is_some() {
                                self.open_session_tab(&seed);
                                self.toast(NoticeLevel::Info, format!("新会话已创建 {seed}"));
                            }
                    }
                    SessionStateEvent::Resumed => {}
                    SessionStateEvent::Closed | SessionStateEvent::Archived | SessionStateEvent::Deleted => {
                        if self.tabs.contains(&seed) {
                            let verb = match state {
                                SessionStateEvent::Archived => "已归档",
                                SessionStateEvent::Deleted => "已删除",
                                _ => "已关闭",
                            };
                            self.close_tab_by_seed(&seed);
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
                    && let Some(t) = title.clone() {
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
                    st.ws_sel = None;
                }
                if self.overlays.iter().any(|o| matches!(o, Overlay::Settings(_))) {
                    self.fetch_config();
                }
            }
            ControlEvent::InteractionRequested { interaction_id, turn_id, mode, questions } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.pending_ask = Some(AskPanel::new(interaction_id, turn_id, mode, questions));
                    sess.scroll.follow = true;
                }
            }
            ControlEvent::InteractionResolved { resolution, interaction_id } => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && sess.pending_ask.as_ref().is_some_and(|p| p.interaction_id == interaction_id) {
                        sess.pending_ask = None;
                        let _ = resolution;
                    }
                if resolution == AskResolution::Dismissed {
                    self.toast(NoticeLevel::Warn, format!("ask 已跳过 [{seed}]"));
                }
            }
            ControlEvent::PlanReviewRequested { interaction_id, turn_id, plan_content, review_type, todo_items } => {
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
            ControlEvent::PlanReviewResolved { interaction_id, approved } => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && sess.pending_plan.as_ref().is_some_and(|p| p.interaction_id == interaction_id) {
                        sess.pending_plan = None;
                    }
                self.toast(
                    if approved { NoticeLevel::Info } else { NoticeLevel::Warn },
                    format!("plan review {}", if approved { "已批准" } else { "已拒绝" }),
                );
            }
            ControlEvent::SkillsUpdated { available, active, runtime, .. } => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.skills = Some(crate::protocol::event::SkillsStatus {
                        available,
                        active,
                        runtime,
                        ..Default::default()
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
                if let Some(sess) = self.sessions.get_mut(&target) {
                    sess.dashboard = Some(snapshot);
                    sess.rendered = None;
                } else if self.sessions.contains_key(&snapshot.seed)
                    && let Some(sess) = self.sessions.get_mut(&snapshot.seed) {
                        sess.dashboard = Some(snapshot);
                        sess.rendered = None;
                    }
                // replaceable 空快照（tasks=[]）时：老 daemon/丢帧后仍为空，主动回退 service 拉取。
                let needs_fallback = self
                    .sessions
                    .get(&target)
                    .and_then(|s| s.dashboard.as_ref())
                    .is_some_and(|d| d.tasks.is_empty() && d.recent_edits.is_empty() && d.documents.is_empty());
                if needs_fallback {
                    self.fetch_dashboard(target.clone());
                }
            }
            ControlEvent::DashboardUpdated { session_seed, .. } => {
                let target = if session_seed.is_empty() { seed.clone() } else { session_seed.clone() };
                let needs_fetch = if self.sessions.contains_key(&target) {
                    let sess = &self.sessions[&target];
                    sess.dashboard.is_none()
                        || sess.dashboard.as_ref().is_some_and(|d| d.tasks.is_empty() && d.documents.is_empty())
                } else {
                    false
                };
                if needs_fetch {
                    self.fetch_dashboard(target);
                }
            }
            ControlEvent::SubagentStatus { name, state, .. } => {
                self.toast(NoticeLevel::Info, format!("子代理 {name}: {state}"));
            }
            ControlEvent::OperationFailed { scope, error, .. } => {
                self.toast(NoticeLevel::Error, format!("失败[{:?}] {}: {}", scope, error.code, error.message));
                // 鬼影清理（winui 教训）：ask 被拒/交互不存在 → 清挂起面板。
                if matches!(error.code.as_str(), "ask_rejected" | "interaction_not_found")
                    && let Some(sess) = self.sessions.get_mut(&seed) {
                        sess.pending_ask = None;
                        sess.pending_plan = None;
                    }
            }
            ControlEvent::OperationCompleted { .. } => {}
        }
    }

    // ───────────────────────── 对话频道事件 ─────────────────────────

    fn handle_conversation(&mut self, seed: String, ev: ConversationEvent) {
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        match ev {
            ConversationEvent::TurnStarted { turn_id, .. } => {
                sess.streaming = Some(session::StreamingState {
                    turn_id,
                    phase: StreamPhase::Thinking,
                    round_num: 0,
                    tool_name: None,
                });
                sess.last_error = None;
                sess.scroll.follow = true;
                sess.scroll.offset = 0;
            }
            ConversationEvent::TurnCompleted { usage, turn_id, .. } => {
                streaming_done(sess, Some(&turn_id));
                if let Some(u) = usage
                    && let Some(conv) = sess.conversation.as_mut() {
                        conv.usage = Some(u);
                    }
            }
            ConversationEvent::TurnFailed { turn_id, error } => {
                streaming_done(sess, Some(&turn_id));
                sess.last_error = Some(error.clone());
                self.toast(NoticeLevel::Error, format!("回合失败: {}: {}", error.code, error.message));
            }
            ConversationEvent::RoundDelta { round_num, kind, .. } => {
                if let Some(s) = sess.streaming.as_mut() {
                    s.round_num = round_num;
                    s.phase = match kind {
                        crate::protocol::event::RoundDeltaKind::Thinking => StreamPhase::Thinking,
                        crate::protocol::event::RoundDeltaKind::ToolCalling => StreamPhase::ToolCalling,
                        crate::protocol::event::RoundDeltaKind::Answering => StreamPhase::Answering,
                    };
                }
            }
            ConversationEvent::BlockCheckpoint { .. } => {}
            ConversationEvent::RoundCompleted { .. } => {}
            ConversationEvent::ProviderRetrying { attempt, max_retries, error_message, .. } => {
                self.toast(
                    NoticeLevel::Warn,
                    format!("provider 重试 {attempt}/{max_retries}: {}", truncate_str(&error_message, 60)),
                );
            }
            ConversationEvent::ProviderToolStatus { state, .. } => {
                if let Some(s) = sess.streaming.as_mut() {
                    s.phase = match state {
                        crate::protocol::event::ProviderToolState::Completed => StreamPhase::Answering,
                        _ => StreamPhase::ToolCalling,
                    };
                }
            }
            ConversationEvent::UsageUpdated { usage, context_limit, model, .. } => {
                sess.apply_usage(usage, context_limit, model);
            }
            ConversationEvent::CompactStarted { turns_total, turns_keeping, .. } => {
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
            ConversationEvent::CompactFinished { status, turns_compacted, .. } => {
                sess.compact_anim = None;
                self.toast(
                    match status {
                        crate::protocol::event::CompactStatus::Completed => NoticeLevel::Info,
                        _ => NoticeLevel::Warn,
                    },
                    format!("compact {}: {:?}", 
                        if status == crate::protocol::event::CompactStatus::Completed { "完成" } else { "未完成" },
                        turns_compacted),
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
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        match ev {
            ToolEvent::ToolPermissionRequested {
                tool_call_id, tool_name, reason, paths, category, level, risk, consequence, ..
            } => {
                // 去重：同一 tool_call 只保留一个面板。
                sess.pending_permissions.retain(|p| p.tool_call_id != tool_call_id);
                sess.pending_permissions.push(PermissionPanel {
                    tool_call_id,
                    tool_name,
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
            ToolEvent::ToolStarted { tool_call_id, name, .. } => {
                sess.pending_permissions.retain(|p| p.tool_call_id != tool_call_id);
                if let Some(s) = sess.streaming.as_mut() {
                    s.phase = StreamPhase::ToolCalling;
                    s.tool_name = Some(name);
                }
            }
            ToolEvent::ToolFinished { tool_call_id, .. } => {
                sess.pending_permissions.retain(|p| p.tool_call_id != tool_call_id);
            }
            ToolEvent::ToolNotice { level, message, .. } => {
                self.toast(level, format!("[tool] {message}"));
            }
            ToolEvent::CodeChanged { lines_added, lines_removed, .. } => {
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
                        let conv = crate::protocol::snapshot::ConversationStateView::parse(&b.conversation.state);
                        sess.usage = conv.usage.clone();
                        sess.usage_totals = conv.usage_totals.clone();
                        sess.context_limit = conv.context_limit;
                        let model = conv.model.clone();
                        sess.conversation = Some(conv);
                        let ctl = crate::protocol::snapshot::ChannelStateView::parse_control(&b.control.state);
                        sess.activity = ctl.activity.or(sess.activity);
                        if sess.mode == crate::protocol::command::ConversationMode::Code
                            && let Some(meta) = &sess.meta {
                                sess.mode = meta.conversation_mode();
                            }
                        match ctl.dashboard {
                            Some(dash) => {
                                let is_empty = dash.tasks.is_empty() && dash.documents.is_empty() && dash.recent_edits.is_empty();
                                sess.dashboard = Some(dash);
                                sess.rendered = None;
                                needs_fetch = is_empty;
                            }
                            None => {
                                needs_fetch = true;
                            }
                        }
                        let tool = crate::protocol::snapshot::ChannelStateView::parse_tool(&b.tool.state);
                        if let Some(perm) = tool.pending_permission {
                            // bootstrap 恢复挂起权限（详情等 tool 事件补全）。
                            sess.pending_permissions.push(PermissionPanel {
                                tool_call_id: perm.tool_call_id,
                                tool_name: "（恢复中）".into(),
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
                        sess.rendered = None;
                    }
                    if needs_fetch {
                        self.fetch_dashboard(bootstrap_seed);
                    }
                }
                Err(e) => self.toast(NoticeLevel::Error, format!("bootstrap 失败[{seed}]: {e}")),
            },
            ActionResult::CommandAck { seed, label, result } => match result {
                Ok(ack) => {
                    if ack.status == crate::protocol::envelope::AckStatus::Rejected {
                        let msg = format!(
                            "{} 被拒绝: {} {}",
                            label,
                            ack.code.unwrap_or_default(),
                            ack.message.unwrap_or_default()
                        );
                        self.toast(NoticeLevel::Error, msg.clone());
                        if let Some(seed) = seed
                            && let Some(sess) = self.sessions.get_mut(&seed) {
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
            ActionResult::SessionActivity(Ok(v)) => {
                if let Some(arr) = v.as_array() {
                    for item in arr {
                        if let (Some(seed), Some(state)) =
                            (item.get("seed").and_then(|s| s.as_str()), item.get("state"))
                            && let Ok(state) = serde_json::from_value::<ActivityState>(state.clone()) {
                                self.activity_cache.insert(seed.to_owned(), state);
                            }
                    }
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
                    sess.rendered = None;
                }
            }
            ActionResult::LoadOlder { seed, result } => {
                if let (Some(sess), Ok(page)) = (self.sessions.get_mut(&seed), result) {
                    sess.loading_older = false;
                    // prepend 而非 replace：已加载的窗口内容保留；
                    // offset（距底行数）不变，视口内容相对稳定。
                    sess.timeline.prepend_older(&page);
                    sess.timeline.cap_turns(TURNS_CAP);
                    sess.rendered = None;
                } else if let Some(sess) = self.sessions.get_mut(&seed) {
                    sess.loading_older = false;
                }
            }
            ActionResult::Receipt { label, seed, result, .. } => match result {
                Ok(status) if status.state == CommandState::Succeeded => {
                    self.toast(NoticeLevel::Info, format!("{label} 完成"));
                    if let Some(seed) = seed {
                        self.request_rebaseline(&seed);
                    }
                }
                Ok(status) => {
                    self.toast(
                        NoticeLevel::Warn,
                        format!("{label}: {:?}", status.state),
                    );
                }
                Err(e) => self.toast(NoticeLevel::Error, format!("{label}: {e}")),
            },
            ActionResult::Dashboard { seed, result } => {
                self.dashboard_fetching.remove(&seed);
                match result {
                    Ok(dash) => {
                        if let Some(sess) = self.sessions.get_mut(&seed)
                            && (!dash.tasks.is_empty() || !dash.recent_edits.is_empty() || !dash.documents.is_empty()) {
                                sess.dashboard = Some(dash);
                                sess.rendered = None;
                            }
                    }
                    Err(_e) => {}
                }
            }
        }
    }

    // ───────────────────────── 标签页 / 会话 ─────────────────────────

    fn sync_tracked(&mut self) {
        let seeds: Vec<String> = self.tabs.clone();
        self.tracked_seeds = seeds.iter().cloned().collect();
        self.runtime.set_tracked_seeds(seeds);
    }

    pub fn toast(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.toasts.push_back(Toast { level, text: text.into(), at: Instant::now() });
        while self.toasts.len() > 8 {
            self.toasts.pop_front();
        }
    }

    // ───────────────────────── 滚动 ─────────────────────────

    /// app → daemon 异步出口的唯一入口：集中克隆 client/msg_tx 并 spawn。
    /// 今后如需统一超时/退避/取消/指标，只需叠加在此处。
    pub(super) fn spawn_api<F, Fut>(&self, task: F)
    where
        F: FnOnce(Arc<HttpClient>, tokio::sync::mpsc::UnboundedSender<AppMsg>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move { task(client, tx).await });
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
            Some(GlobalKey::CloseTab) => {
                if let Some(seed) = self.active_seed() {
                    self.overlays.push(Overlay::Confirm { action: ConfirmAction::CloseTab(seed) });
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
                self.show_reasoning = !self.show_reasoning;
                for s in self.sessions.values_mut() {
                    s.rendered = None;
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
            None => {}
        }

        // Alt+数字 / Alt+方向：标签切换（目标计算为纯函数，见 keymap）。
        if key.modifiers.contains(KeyModifiers::ALT)
            && let Some(next) = keymap::alt_tab_target(self.active, self.tabs.len(), key.code) {
                self.active = next;
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

        // 首页（无 tab 且无覆盖层时，会话列表即首页）
        if self.tabs.is_empty()
            && self.home_key(key) {
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

    /// 每帧前维护：焦点变化时执行 LRU 内存回收；只为 active 会话重建
    /// 渲染缓存（后台标签的缓存已被丢弃，聚焦时按需重建一次）。
    pub fn ensure_render_caches(&mut self, width: u16) {
        let Some(active) = self.active_seed() else { return };
        if self.last_focused.as_deref() != Some(active.as_str()) {
            self.touch_focus(&active);
            self.last_focused = Some(active.clone());
        }
        let Some(sess) = self.sessions.get_mut(&active) else { return };
        // 流式/运行中工具需动画：即使 version 未变也定期重绘（对齐 opencode Spinner 60fps，tui 侧 500ms Tick 驱动）
        let streaming = sess.timeline.is_streaming()
            || sess.timeline.turns.iter().any(|t| t.rounds.iter().any(|r| r.blocks.iter().any(|b| {
                b.tool.as_ref().is_some_and(|tl| tl.state == crate::protocol::timeline::TimelineToolState::Running)
            })));
        let need = match &sess.rendered {
            Some(cached) => cached.version != sess.timeline.version || cached.width != width || streaming,
            None => true,
        };
        if need {
            let lines = render_transcript::render_transcript_with_opts(sess, width, self.show_reasoning);
            sess.rendered = Some(session::RenderedTranscript {
                version: sess.timeline.version,
                width,
                lines,
            });
        }
    }

}

pub fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
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
