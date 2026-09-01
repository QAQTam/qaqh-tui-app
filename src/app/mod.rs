//! App 状态机：输入路由、wire 事件消费、命令发送、覆盖层。
//!
//! 事件驱动（无轮询泵）：所有后端状态变化经 runtime 消息到达后立即生效，
//! UI 在同一帧内重绘。

pub mod markdown;
pub mod render_line;
pub mod render_transcript;
pub mod session;
pub mod settings;
pub mod slash;
pub mod timeline_model;

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
}

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
            config: None,
            settings_saving: false,
            show_reasoning: true,
            show_workspace: true,
            tracked_seeds: HashSet::new(),
            focus_order: Vec::new(),
            last_focused: None,
            show_todo_detail: true,
            last_tick: Instant::now(),
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
        if let Some(armed) = self.quit_armed {
            if armed.elapsed() > Duration::from_secs(3) {
                self.quit_armed = None;
            }
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
            MouseEventKind::Down(kind) if kind == ratatui::crossterm::event::MouseButton::Left => {
                if m.row == 0 {
                    self.click_tab(m.column);
                }
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
        if let Some(Overlay::Settings(st)) = self.overlays.last_mut() {
            if let Some(buf) = st.editing.as_mut() {
                for ch in text.chars() {
                    if ch != '\n' && ch != '\r' {
                        buf.buf.insert(buf.cursor.min(buf.buf.len()), ch);
                        buf.cursor += 1;
                    }
                }
                return;
            }
        }
        let Some(sess) = self.active_session_mut() else { return };
        if let Some(panel) = sess.pending_ask.as_mut() {
            if panel.editing_custom.is_some() {
                panel.input.push_str(&text);
                return;
            }
        }
        if let Some(panel) = sess.pending_plan.as_mut() {
            if panel.entering_message {
                panel.message.push_str(&text);
                return;
            }
        }
        sess.composer.insert_str(&text);
    }

    fn handle_runtime(&mut self, msg: RuntimeMsg) {
        match msg {
            RuntimeMsg::Conn(ev) => self.handle_conn(ev),
            RuntimeMsg::Ringing { channel, env } => self.handle_envelope(channel, *env),
            RuntimeMsg::ResetRequired { seed, .. } => {
                // 频道级 reset → 重新 bootstrap 该会话（timeline 流自会 re-baseline）。
                let tx = self.msg_tx.clone();
                let client = self.client.clone();
                let seed2 = seed.clone();
                tokio::spawn(async move {
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
                    let tx = self.msg_tx.clone();
                    let client = self.client.clone();
                    tokio::spawn(async move {
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
                        if let Some(cid) = causation_id {
                            if self.pending_creates.remove(&cid).is_some() {
                                self.open_session_tab(&seed);
                                self.toast(NoticeLevel::Info, format!("新会话已创建 {seed}"));
                            }
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
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    if let Some(t) = title.clone() {
                        sess.title = Some(t);
                    }
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
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    if sess.pending_ask.as_ref().is_some_and(|p| p.interaction_id == interaction_id) {
                        sess.pending_ask = None;
                        let _ = resolution;
                    }
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
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    if sess.pending_plan.as_ref().is_some_and(|p| p.interaction_id == interaction_id) {
                        sess.pending_plan = None;
                    }
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
                } else if self.sessions.contains_key(&snapshot.seed) {
                    if let Some(sess) = self.sessions.get_mut(&snapshot.seed) {
                        sess.dashboard = Some(snapshot);
                    }
                }
                // 若对应会话在后台 tabs 中，dashboard 仍更新以便切回即现
            }
            ControlEvent::DashboardUpdated { session_seed, .. } => {
                // 轻量心跳：若已跟踪但 dashboard 仍为空，且 envelope seed 指向该会话，
                // 不主动拉取（避免轮询风暴），仅标记；具体兜底由 bootstrap 完成后覆盖。
                let _ = session_seed;
            }
            ControlEvent::SubagentStatus { name, state, .. } => {
                self.toast(NoticeLevel::Info, format!("子代理 {name}: {state}"));
            }
            ControlEvent::OperationFailed { scope, error, .. } => {
                self.toast(NoticeLevel::Error, format!("失败[{:?}] {}: {}", scope, error.code, error.message));
                // 鬼影清理（winui 教训）：ask 被拒/交互不存在 → 清挂起面板。
                if matches!(error.code.as_str(), "ask_rejected" | "interaction_not_found") {
                    if let Some(sess) = self.sessions.get_mut(&seed) {
                        sess.pending_ask = None;
                        sess.pending_plan = None;
                    }
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
                if let Some(u) = usage {
                    if let Some(conv) = sess.conversation.as_mut() {
                        conv.usage = Some(u);
                    }
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
                sess.compact_status = Some(format!("压缩中 {turns_keeping}/{turns_total}"));
            }
            ConversationEvent::CompactProgress { .. } => {}
            ConversationEvent::CompactFinished { status, turns_compacted, .. } => {
                sess.compact_status = Some(format!("{status:?}"));
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
                    if let Some(sess) = self.sessions.get_mut(&seed) {
                        let conv = crate::protocol::snapshot::ConversationStateView::parse(&b.conversation.state);
                        sess.usage = conv.usage.clone();
                        sess.usage_totals = conv.usage_totals.clone();
                        sess.context_limit = conv.context_limit;
                        let model = conv.model.clone();
                        sess.conversation = Some(conv);
                        let ctl = crate::protocol::snapshot::ChannelStateView::parse_control(&b.control.state);
                        sess.activity = ctl.activity.or(sess.activity);
                        if sess.mode == crate::protocol::command::ConversationMode::Code {
                            if let Some(meta) = &sess.meta {
                                sess.mode = meta.conversation_mode();
                            }
                        }
                        if ctl.dashboard.is_some() {
                            sess.dashboard = ctl.dashboard;
                            sess.rendered = None;
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
                        if let Some(seed) = seed {
                            if let Some(sess) = self.sessions.get_mut(&seed) {
                                sess.composer.input = msg.chars().collect(); // 不丢内容
                                sess.composer.cursor = sess.composer.input.len();
                            }
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
                        {
                            if let Ok(state) = serde_json::from_value::<ActivityState>(state.clone()) {
                                self.activity_cache.insert(seed.to_owned(), state);
                            }
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
                        self.toast(NoticeLevel::Info, format!("附件已上传"));
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
        }
    }

    // ───────────────────────── 标签页 / 会话 ─────────────────────────

    fn sync_tracked(&mut self) {
        let seeds: Vec<String> = self.tabs.clone();
        self.tracked_seeds = seeds.iter().cloned().collect();
        self.runtime.set_tracked_seeds(seeds);
    }

    pub fn open_session_tab(&mut self, seed: &str) {
        if self.tabs.iter().any(|s| s == seed) {
            self.active = self.tabs.iter().position(|s| s == seed).unwrap_or(0);
            return;
        }
        self.tabs.push(seed.to_owned());
        self.sessions.insert(seed.to_owned(), SessionState::new(seed.to_owned()));
        self.active = self.tabs.len() - 1;
        self.sync_tracked();
        // attach + bootstrap（timeline 流由 runtime 自动建立）。
        self.attach_and_bootstrap(seed.to_owned());
    }

    fn attach_and_bootstrap(&mut self, seed: String) {
        let tx = self.msg_tx.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Control(ControlCommand::SessionResume { seed: seed.clone() }),
            )
            .with_seed(seed.clone());
            let ack = client.command(&cmd).await;
            if let Err(e) = ack {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    seed: Some(seed.clone()),
                    label: "resume",
                    result: Err(e.to_string()),
                }));
                return;
            }
            let result = client.bootstrap(&seed).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
        });
    }

    pub fn new_session(&mut self) {
        self.new_session_with_cwd(None);
    }

    /// 三档回退：显式 > 环境变量 > 启动目录 > 当前会话 > None（让后端迁移）
    pub fn effective_cwd(&self, explicit: Option<String>) -> Option<String> {
        if let Some(p) = explicit {
            let t = p.trim().to_string();
            if !t.is_empty() {
                let expanded = crate::app::slash::expand_tilde(&t);
                return Some(expanded);
            }
        }
        if let Ok(env) = std::env::var("QAQH_DEFAULT_CWD") {
            let env = crate::app::slash::expand_tilde(env.trim());
            if !env.trim().is_empty() && crate::app::slash::is_absolute_path(&env) {
                return Some(env.trim().to_string());
            }
        }
        if let Some(cur) = self.initial_cwd.as_deref().filter(|s| !s.trim().is_empty()) {
            return Some(cur.to_string());
        }
        self.active_session().and_then(|s| s.meta.as_ref().and_then(|m| m.cwd.clone()))
    }

    pub fn new_session_with_cwd(&mut self, cwd: Option<String>) {
        let cwd = self.effective_cwd(cwd);
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        let cmd = build_envelope(
            &client,
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd,
                tool_mode: None,
                custom_tools: vec![],
            }),
        );
        let command_id = cmd.command_id.clone();
        self.pending_creates.insert(command_id.clone(), Instant::now());
        tokio::spawn(async move {
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: None,
                label: "新会话",
                result,
            }));
        });
    }

    fn close_tab_by_seed(&mut self, seed: &str) {
        if let Some(pos) = self.tabs.iter().position(|s| s == seed) {
            self.tabs.remove(pos);
            self.sessions.remove(seed);
            self.tracked_seeds.remove(seed);
            self.focus_order.retain(|s| s != seed);
            if self.last_focused.as_deref() == Some(seed) {
                self.last_focused = None;
            }
            if self.active >= self.tabs.len() && self.active > 0 {
                self.active = self.tabs.len() - 1;
            }
            self.sync_tracked();
        }
    }

    pub fn active_seed(&self) -> Option<String> {
        self.tabs.get(self.active).cloned()
    }

    pub fn active_session(&self) -> Option<&SessionState> {
        self.tabs.get(self.active).and_then(|s| self.sessions.get(s))
    }

    fn active_session_mut(&mut self) -> Option<&mut SessionState> {
        let seed = self.tabs.get(self.active)?.clone();
        self.sessions.get_mut(&seed)
    }

    // ───────────────────────── 命令发送 ─────────────────────────

    pub fn send_message(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        if sess.composer.is_empty() {
            return;
        }
        let (text, attachments) = sess.composer.take();
        let content_refs: Vec<ContentRef> = attachments.into_iter().map(|a| a.content).collect();
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text,
                    images: vec![],
                    attachments: (!content_refs.is_empty()).then_some(content_refs),
                    as_system: false,
                }),
            )
            .with_seed(seed.clone());
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "发送",
                result,
            }));
        });
    }

    pub fn cancel_turn(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let streaming = self.sessions.get(&seed).is_some_and(|s| s.streaming.is_some());
        if !streaming {
            return;
        }
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Conversation(ConversationCommand::ConversationCancel { turn_id: None }),
            )
            .with_seed(seed.clone());
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "中止",
                result,
            }));
        });
    }

    pub fn toggle_mode(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let next = match self.sessions.get(&seed).map(|s| s.mode) {
            Some(crate::protocol::command::ConversationMode::Plan) => {
                crate::protocol::command::ConversationMode::Code
            }
            _ => crate::protocol::command::ConversationMode::Plan,
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.mode = next; // 乐观更新
            sess.rendered = None;
        }
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Conversation(ConversationCommand::ConversationSetMode { mode: next }),
            )
            .with_seed(seed.clone());
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "切换模式",
                result,
            }));
        });
    }

    pub fn compact(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Conversation(ConversationCommand::ConversationCompact { turn_id: None }),
            )
            .with_seed(seed.clone());
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "compact",
                result,
            }));
        });
    }

    pub fn undo_turn(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(turn_id) = self.sessions.get(&seed).and_then(|s| s.timeline.last_turn_id().map(str::to_owned))
        else {
            return;
        };
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Conversation(ConversationCommand::ConversationUndoTurn { turn_id }),
            )
            .with_seed(seed.clone());
            let command_id = cmd.command_id.clone();
            let result = client.command(&cmd).await;
            match result {
                Ok(_) => {
                    // ACK ≠ 完成：轮询 receipt 到终态（对齐 winui，但消费其结果）。
                    let mut state: Option<RingingCommandStatus> = None;
                    for _ in 0..30 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if let Ok(status) = client.command_status(&command_id).await {
                            if status.state.is_terminal() {
                                state = Some(status);
                                break;
                            }
                        }
                    }
                    let _ = tx.send(AppMsg::Action(ActionResult::Receipt {
                        label: "撤销回合",
                        seed: Some(seed),
                        result: Ok(state.unwrap_or(RingingCommandStatus {
                            command_id: String::new(),
                            state: CommandState::Running,
                            payload_fingerprint: String::new(),
                            terminal_event_id: None,
                            error_code: None,
                        })),
                    }));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                        seed: Some(seed),
                        label: "撤销回合",
                        result: Err(e.to_string()),
                    }));
                }
            }
        });
    }

    fn request_rebaseline(&mut self, seed: &str) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        let seed = seed.to_owned();
        tokio::spawn(async move {
            let result = client
                .timeline_page(&seed, None, crate::runtime::TIMELINE_PAGE_LIMIT)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::Rebaseline { seed, result }));
        });
    }

    pub fn load_older(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let loading = self.sessions.get(&seed).is_some_and(|s| s.loading_older || !s.timeline.has_more);
        if loading {
            return;
        }
        let first_turn = self
            .sessions
            .get(&seed)
            .and_then(|s| s.timeline.turns.first().map(|t| t.turn_id.clone()));
        let Some(before) = first_turn else { return };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.loading_older = true;
        }
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .timeline_page(&seed, Some(&before), crate::runtime::TIMELINE_PAGE_LIMIT)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::LoadOlder { seed, result }));
        });
    }

    // ───────────────────────── 交互响应命令 ─────────────────────────

    pub fn submit_ask(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(panel) = self.sessions.get_mut(&seed).and_then(|s| s.pending_ask.as_ref()) else {
            return;
        };
        let answers = match panel.collect_answers() {
            Ok(a) => a,
            Err(e) => {
                if let Some(sess) = self.sessions.get_mut(&seed) {
                    if let Some(p) = sess.pending_ask.as_mut() {
                        p.error = Some(e);
                    }
                }
                return;
            }
        };
        let interaction_id = self
            .sessions
            .get(&seed)
            .and_then(|s| s.pending_ask.as_ref())
            .map(|p| p.interaction_id.clone())
            .expect("panel");
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.pending_ask = None;
        }
        let answers = answers
            .into_iter()
            .map(|(question_id, answer)| crate::protocol::command::AskAnswer { question_id, answer })
            .collect();
        self.send_control_command(
            seed,
            ControlCommand::InteractionAskRespond { interaction_id, answers },
            "提交回答",
        );
    }

    pub fn dismiss_ask(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(interaction_id) = self
            .sessions
            .get(&seed)
            .and_then(|s| s.pending_ask.as_ref())
            .map(|p| p.interaction_id.clone())
        else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.pending_ask = None;
        }
        self.send_control_command(seed, ControlCommand::InteractionAskDismiss { interaction_id }, "跳过 ask");
    }

    pub fn respond_plan(&mut self, approved: bool, autonomous: bool) {
        let Some(seed) = self.active_seed() else { return };
        let panel = self.sessions.get(&seed).and_then(|s| s.pending_plan.as_ref());
        let Some(panel) = panel else { return };
        let interaction_id = panel.interaction_id.clone();
        let message = if approved { None } else {
            let m = panel.message.trim().to_owned();
            (!m.is_empty()).then_some(m)
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.pending_plan = None;
        }
        self.send_control_command(
            seed,
            ControlCommand::PlanReviewRespond { interaction_id, approved, message, autonomous },
            "plan review",
        );
    }

    pub fn respond_permission(&mut self, approved: bool) {
        let Some(seed) = self.active_seed() else { return };
        let Some(panel) = self
            .sessions
            .get(&seed)
            .and_then(|s| s.active_permission().cloned())
        else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.pending_permissions.retain(|p| p.tool_call_id != panel.tool_call_id);
        }
        let cmd = RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
            tool_call_id: panel.tool_call_id,
            approved,
            trust_folder: panel.trust_folder,
        });
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let env = build_envelope(&client, cmd).with_seed(seed.clone());
            let result = client.command(&env).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "权限",
                result,
            }));
        });
    }

    fn send_control_command(&mut self, seed: String, command: ControlCommand, label: &'static str) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let env = build_envelope(&client, RingingCommand::Control(command)).with_seed(seed.clone());
            let result = client.command(&env).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label,
                result,
            }));
        });
    }

    // ───────────────────────── 服务面 ─────────────────────────

    pub fn fetch_session_list(&mut self) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let list = client.session_list().await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::SessionList(list)));
            let activity = client
                .service(methods::SESSION_ACTIVITY, &serde_json::json!({}))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::SessionActivity(activity)));
        });
    }

    pub fn fetch_config(&mut self) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::CONFIG_LOAD, &serde_json::json!({}))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigLoaded(result)));
        });
    }

    /// `config.save`：把设置页草稿作为 Merge Patch 发送（只发脏字段）。
    fn save_settings(&mut self, st: &mut SettingsState) {
        if self.settings_saving {
            return;
        }
        if st.draft.is_empty() {
            self.toast(NoticeLevel::Info, "设置无改动");
            return;
        }
        if let Err(e) = st.draft.validate() {
            self.toast(NoticeLevel::Error, format!("校验失败：{e}"));
            return;
        }
        self.settings_saving = true;
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        let payload = st.draft.to_json();
        tokio::spawn(async move {
            let result = client
                .service(methods::CONFIG_SAVE, &payload)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite { label: "设置", result }));
        });
    }

    /// 端口字段回车：即时走各自的单写口（不进草稿）。
    fn settings_port_activate(&mut self, st: &mut SettingsState) {
        match st.row().id {
            settings::FieldId::PermissionLevel => {
                self.toast(NoticeLevel::Info, "聚焦权限级别后按 1-4 即时生效");
            }
            settings::FieldId::ActiveProfile => {
                let name = st
                    .profile_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.active_profile.clone()));
                if let Some(name) = name {
                    self.apply_profile(name);
                }
            }
            settings::FieldId::WorkspaceMode => {
                let mode = st
                    .ws_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.workspace.mode.clone()));
                if let Some(mode) = mode {
                    self.set_workspace_mode(mode);
                }
            }
            _ => {}
        }
    }

    /// 端口字段 ←→：切换候选（回车才真正应用）。
    fn settings_port_cycle(&mut self, st: &mut SettingsState, delta: i32) {
        match st.row().id {
            settings::FieldId::ActiveProfile => {
                let Some(cfg) = self.config.as_ref() else { return };
                if cfg.profiles.is_empty() {
                    return;
                }
                let cur = st
                    .profile_sel
                    .clone()
                    .unwrap_or_else(|| cfg.active_profile.clone());
                let idx = cfg.profiles.iter().position(|n| *n == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(cfg.profiles.len() as i32) as usize;
                st.profile_sel = Some(cfg.profiles[next].clone());
            }
            settings::FieldId::WorkspaceMode => {
                // local（全平台）/ wsl（仅 Windows）——与后端 workspace.set_mode 校验一致。
                let modes: &[&str] = if cfg!(windows) { &["local", "wsl"] } else { &["local"] };
                let cur = st
                    .ws_sel
                    .clone()
                    .or_else(|| self.config.as_ref().map(|c| c.workspace.mode.clone()))
                    .unwrap_or_else(|| "local".into());
                let idx = modes.iter().position(|m| *m == cur).unwrap_or(0);
                let next = (idx as i32 + delta).rem_euclid(modes.len() as i32) as usize;
                st.ws_sel = Some(modes[next].to_string());
            }
            _ => {}
        }
    }

    /// `profile.apply`：切换活跃 profile（服务端单写口，写后广播 reload）。
    pub fn apply_profile(&mut self, name: String) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::PROFILE_APPLY, &serde_json::json!({ "name": name }))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "应用 Profile",
                result,
            }));
        });
    }

    /// `workspace.set_mode`：local / wsl（仅 Windows）（服务端单写口）。
    pub fn set_workspace_mode(&mut self, mode: String) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(methods::WORKSPACE_SET_MODE, &serde_json::json!({ "mode": mode }))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "workspace 模式",
                result,
            }));
        });
    }

    pub fn set_permission_level(&mut self, level: u8) {
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = client
                .service(
                    methods::CONFIG_SET_PERMISSION_LEVEL,
                    &serde_json::json!({ "level": level }),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::ConfigWrite {
                label: "权限级别",
                result,
            }));
        });
    }

    pub fn archive_session(&mut self, seed: String) {
        self.send_control_command(seed.clone(), ControlCommand::SessionArchive { seed }, "归档");
    }

    pub fn unarchive_session(&mut self, seed: String) {
        self.send_control_command(seed.clone(), ControlCommand::SessionUnarchive { seed }, "取消归档");
    }

    pub fn delete_session(&mut self, seed: String) {
        self.send_control_command(seed.clone(), ControlCommand::SessionDelete { seed }, "删除");
    }

    pub fn upload_attachment(&mut self, path: String) {
        let Some(seed) = self.active_seed() else { return };
        let client = self.client.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let read_path = path.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
                let bytes = std::fs::read(&read_path).map_err(|e| e.to_string())?;
                let media = guess_media_type(&read_path);
                Ok((bytes, media))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
            match result {
                Ok((bytes, media)) => {
                    let uploaded = client.upload_content(&seed, &media, bytes).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded { seed, path, result: uploaded }));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded {
                        seed,
                        path,
                        result: Err(e),
                    }));
                }
            }
        });
    }

    // ───────────────────────── toast / 滚动 ─────────────────────────

    pub fn toast(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.toasts.push_back(Toast { level, text: text.into(), at: Instant::now() });
        while self.toasts.len() > 8 {
            self.toasts.pop_front();
        }
    }

    // ───────────────────────── 滚动 ─────────────────────────

    pub fn scroll_up(&mut self, lines: usize) {
        let Some(seed) = self.active_seed() else { return };
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        sess.scroll.follow = false;
        sess.scroll.offset = sess.scroll.offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let Some(seed) = self.active_seed() else { return };
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        if sess.scroll.offset <= lines {
            sess.scroll.offset = 0;
            sess.scroll.follow = true;
        } else {
            sess.scroll.offset -= lines;
        }
    }

    pub fn scroll_top(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.scroll.follow = false;
            sess.scroll.offset = usize::MAX / 2; // 渲染时 clamp
        }
    }

    pub fn scroll_bottom(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.scroll.follow = true;
            sess.scroll.offset = 0;
        }
    }

    // ───────────────────────── 按键路由 ─────────────────────────

    fn handle_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};

        // 退出（Ctrl+C 二次确认 / Ctrl+Q）。
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.quit_armed.is_some() {
                self.quit = true;
                return;
            }
            self.quit_armed = Some(Instant::now());
            self.toast(NoticeLevel::Info, "再按一次 Ctrl+C 退出");
            return;
        }
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }

        // 全局键。
        match key.code {
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_session();
                return;
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(seed) = self.active_seed() {
                    self.overlays.push(Overlay::Confirm { action: ConfirmAction::CloseTab(seed) });
                }
                return;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_session_list();
                return;
            }
            KeyCode::Char(',') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_settings();
                return;
            }
            KeyCode::F(10) => {
                self.toggle_settings();
                return;
            }
            KeyCode::F(1) => {
                self.toggle_overlay(Overlay::Help);
                return;
            }
            KeyCode::F(3) => {
                self.show_reasoning = !self.show_reasoning;
                for s in self.sessions.values_mut() {
                    s.rendered = None;
                }
                return;
            }
            KeyCode::F(4) => {
                self.show_workspace = !self.show_workspace;
                return;
            }
            KeyCode::F(6) => {
                self.show_todo_detail = !self.show_todo_detail;
                return;
            }
            KeyCode::F(7) => {
                self.toggle_tool_expand();
                return;
            }
            _ => {}
        }

        // Alt+数字 / Alt+方向：标签切换。
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as u8 - b'1') as usize;
                    if idx < self.tabs.len() {
                        self.active = idx;
                    }
                    return;
                }
                KeyCode::Left => {
                    if self.active > 0 {
                        self.active -= 1;
                    } else if !self.tabs.is_empty() {
                        self.active = self.tabs.len() - 1;
                    }
                    return;
                }
                KeyCode::Right => {
                    if self.active + 1 < self.tabs.len() {
                        self.active += 1;
                    } else {
                        self.active = 0;
                    }
                    return;
                }
                _ => {}
            }
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
        if self.tabs.is_empty() {
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

    pub fn open_session_list(&mut self) {
        if self.overlays.last().is_some_and(|o| matches!(o, Overlay::SessionList { .. })) {
            self.overlays.pop();
            return;
        }
        let stale = self
            .session_list_at
            .map(|t| t.elapsed() > Duration::from_secs(3))
            .unwrap_or(true);
        if stale {
            self.fetch_session_list();
        }
        self.overlays.push(Overlay::SessionList { selected: 0, show_archived: false });
    }

    pub fn toggle_settings(&mut self) {
        let open = self.overlays.last().is_some_and(|o| matches!(o, Overlay::Settings(_)));
        if open {
            self.overlays.pop();
        } else {
            if self.config.is_none() {
                self.fetch_config();
            }
            self.overlays.push(Overlay::Settings(SettingsState::default()));
        }
    }

    fn toggle_tool_expand(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        // 收集所有可折叠工具（有输出或 diff），按时间逆序
        let mut candidates: Vec<String> = Vec::new();
        for turn in sess.timeline.turns.iter().rev() {
            for round in turn.rounds.iter().rev() {
                for block in round.blocks.iter().rev() {
                    if let Some(tool) = &block.tool {
                        let has_content = tool.output.as_deref().is_some_and(|s| !s.trim().is_empty())
                            || !tool.progress.trim().is_empty()
                            || tool.diff.as_deref().is_some_and(|d| !d.trim().is_empty());
                        if has_content {
                            candidates.push(tool.tool_call_id.clone());
                        }
                    }
                }
            }
        }
        if candidates.is_empty() { return; }
        // 策略：优先展开最近的收起态；若全部已展开，则收起最近的展开态（循环）
        let mut target: Option<String> = None;
        for id in &candidates {
            if !sess.expanded_tools.contains(id) {
                target = Some(id.clone());
                break;
            }
        }
        if target.is_none() {
            // 全部已展开 → 收起最近一个
            target = candidates.first().cloned();
        }
        if let Some(id) = target {
            if sess.expanded_tools.contains(&id) {
                sess.expanded_tools.remove(&id);
            } else {
                sess.expanded_tools.insert(id);
            }
            sess.rendered = None;
        }
    }

    /// 会话列表的过滤谓词（与渲染一致）。
    fn filtered_sessions(&self, show_archived: bool) -> Vec<usize> {
        self.session_list_cache
            .iter()
            .enumerate()
            .filter(|(_, m)| (show_archived || !m.archived) && !m.ephemeral)
            .map(|(i, _)| i)
            .collect()
    }

    /// 首页（无 tab 时）按键：复用会话列表的导航，视觉更直接
    fn home_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        // 允许 Ctrl 组合已被全局键处理，这里只处理首页专属
        let items = self.filtered_sessions(self.home_show_archived);
        let count = items.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    if self.home_selected == 0 {
                        self.home_selected = count - 1;
                    } else {
                        self.home_selected -= 1;
                    }
                }
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.home_selected = (self.home_selected + 1) % count;
                }
                return true;
            }
            KeyCode::Enter => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].seed.clone();
                    self.open_session_tab(&seed);
                }
                return true;
            }
            KeyCode::Char('n') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_session();
                return true;
            }
            KeyCode::Char('r') => {
                self.fetch_session_list();
                return true;
            }
            KeyCode::Char('a') => {
                self.home_show_archived = !self.home_show_archived;
                self.home_selected = 0;
                return true;
            }
            KeyCode::Char('x') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].seed.clone();
                    self.overlays.push(Overlay::Confirm { action: ConfirmAction::ArchiveSession(seed) });
                }
                return true;
            }
            KeyCode::Char('u') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].seed.clone();
                    self.unarchive_session(seed);
                }
                return true;
            }
            KeyCode::Char('D') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].seed.clone();
                    self.overlays.push(Overlay::Confirm { action: ConfirmAction::DeleteSession(seed) });
                }
                return true;
            }
            _ => {}
        }
        // 首页下也允许 j/k 翻页等，防止落入 composer
        matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('r') | KeyCode::Char('a') | KeyCode::Char('x') | KeyCode::Char('u') | KeyCode::Char('D'))
    }

    /// 交互弹窗按键。返回 true = 已消费。优先级 permission > ask > plan。
    fn modal_key(&mut self, key: KeyEvent) -> bool {
        let Some(seed) = self.active_seed() else { return false };
        let which = {
            let Some(sess) = self.sessions.get(&seed) else { return false };
            if sess.active_permission().is_some() {
                1
            } else if sess.pending_ask.is_some() {
                2
            } else if sess.pending_plan.is_some() {
                3
            } else {
                0
            }
        };
        match which {
            1 => self.permission_key(&seed, key),
            2 => self.ask_key(&seed, key),
            3 => self.plan_key(&seed, key),
            _ => false,
        }
    }

    fn permission_key(&mut self, seed: &str, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyCode;
        enum D {
            Approve,
            Deny,
            ToggleTrust,
            None,
        }
        let decision = {
            let Some(sess) = self.sessions.get(seed) else { return true };
            let Some(perm) = sess.active_permission() else { return true };
            match key.code {
                KeyCode::Char('a') => D::Approve,
                KeyCode::Char('d') | KeyCode::Esc => D::Deny,
                KeyCode::Char('t')
                    if perm.risk == PermissionRisk::High && !perm.paths.is_empty() =>
                {
                    D::ToggleTrust
                }
                _ => D::None,
            }
        };
        match decision {
            D::Approve => self.respond_permission(true),
            D::Deny => self.respond_permission(false),
            D::ToggleTrust => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_permissions.first_mut() {
                        p.trust_folder = !p.trust_folder;
                    }
                }
            }
            D::None => {}
        }
        true
    }

    fn ask_key(&mut self, seed: &str, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyCode;
        enum D {
            FocusUp,
            FocusDown,
            Select { focus: usize, option: usize },
            StartEdit { focus: usize },
            EditChar(char),
            EditBackspace,
            EditCommit,
            EditCancel,
            Submit,
            Dismiss,
            None,
        }
        let decision = {
            let Some(sess) = self.sessions.get(seed) else { return true };
            let Some(ask) = sess.pending_ask.as_ref() else { return true };
            let focus = ask.focus.min(ask.questions.len().saturating_sub(1));
            if ask.editing_custom.is_some() {
                match key.code {
                    KeyCode::Enter => D::EditCommit,
                    KeyCode::Esc => D::EditCancel,
                    KeyCode::Backspace => D::EditBackspace,
                    KeyCode::Char(c) => D::EditChar(c),
                    _ => D::None,
                }
            } else {
                match key.code {
                    KeyCode::Up => D::FocusUp,
                    KeyCode::Down | KeyCode::Tab => D::FocusDown,
                    KeyCode::Char(c @ '1'..='9') => {
                        D::Select { focus, option: (c as u8 - b'1') as usize }
                    }
                    KeyCode::Char('e')
                        if ask.questions.get(focus).map(|q| q.allow_custom).unwrap_or(false) =>
                    {
                        D::StartEdit { focus }
                    }
                    KeyCode::Enter => D::Submit,
                    KeyCode::Esc => D::Dismiss,
                    _ => D::None,
                }
            }
        };
        match decision {
            D::FocusUp => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        p.focus = p.focus.saturating_sub(1);
                    }
                }
            }
            D::FocusDown => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        if p.focus + 1 < p.questions.len() {
                            p.focus += 1;
                        }
                    }
                }
            }
            D::Select { focus, option } => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        if p.questions
                            .get(focus)
                            .map(|q| option < q.options.len())
                            .unwrap_or(false)
                        {
                            p.selections[focus] = Some(option);
                            p.error = None;
                        }
                    }
                }
            }
            D::StartEdit { focus } => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        p.editing_custom = Some(focus);
                        p.input = p.customs[focus].clone();
                    }
                }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        p.input.push(c);
                    }
                }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        p.input.pop();
                    }
                }
            }
            D::EditCommit => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        let qi = p.editing_custom.take().unwrap_or(0);
                        if p.input.trim().is_empty() {
                            p.customs[qi].clear();
                        } else {
                            p.customs[qi] = p.input.trim().to_owned();
                        }
                        p.input.clear();
                        p.error = None;
                    }
                }
            }
            D::EditCancel => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_ask.as_mut() {
                        p.editing_custom = None;
                        p.input.clear();
                    }
                }
            }
            D::Submit => self.submit_ask(),
            D::Dismiss => self.dismiss_ask(),
            D::None => {}
        }
        true
    }

    fn plan_key(&mut self, seed: &str, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        enum D {
            Approve,
            ApproveAuto,
            StartReject,
            Scroll(i32),
            EditChar(char),
            EditBackspace,
            SubmitReject,
            CancelEdit,
            None,
        }
        let decision = {
            let Some(sess) = self.sessions.get(seed) else { return true };
            let entering = sess
                .pending_plan
                .as_ref()
                .map(|p| p.entering_message)
                .unwrap_or(false);
            if entering {
                match key.code {
                    KeyCode::Enter => D::SubmitReject,
                    KeyCode::Esc => D::CancelEdit,
                    KeyCode::Backspace => D::EditBackspace,
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        D::EditChar(c)
                    }
                    _ => D::None,
                }
            } else {
                match key.code {
                    KeyCode::Char('a') => D::Approve,
                    KeyCode::Char('g') => D::ApproveAuto,
                    KeyCode::Char('r') => D::StartReject,
                    KeyCode::Up => D::Scroll(-3),
                    KeyCode::Down => D::Scroll(3),
                    KeyCode::PageUp => D::Scroll(-20),
                    KeyCode::PageDown => D::Scroll(20),
                    _ => D::None,
                }
            }
        };
        match decision {
            D::Approve => self.respond_plan(true, false),
            D::ApproveAuto => self.respond_plan(true, true),
            D::StartReject => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_plan.as_mut() {
                        p.entering_message = true;
                    }
                }
            }
            D::Scroll(delta) => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_plan.as_mut() {
                        if delta > 0 {
                            p.scroll = p.scroll.saturating_add(delta as usize);
                        } else {
                            p.scroll = p.scroll.saturating_sub((-delta) as usize);
                        }
                    }
                }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_plan.as_mut() {
                        p.message.push(c);
                    }
                }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_plan.as_mut() {
                        p.message.pop();
                    }
                }
            }
            D::SubmitReject => self.respond_plan(false, false),
            D::CancelEdit => {
                if let Some(s) = self.sessions.get_mut(seed) {
                    if let Some(p) = s.pending_plan.as_mut() {
                        p.entering_message = false;
                        p.message.clear();
                    }
                }
            }
            D::None => {}
        }
        true
    }


    fn overlay_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let Some(top) = self.overlays.last().cloned() else { return false };

        match top {
            Overlay::Help => {
                self.overlays.pop();
                return true;
            }
            Overlay::Settings(mut st) => {
                use ratatui::crossterm::event::KeyCode;
                // ── 编辑态：按键全部进缓冲 ──
                if st.editing.is_some() {
                    let mut buf = st.editing.take().expect("checked");
                    match key.code {
                        KeyCode::Esc => {} // 取消：editing 保持 None
                        KeyCode::Enter => {
                            if let Err(e) = st.commit_edit(self.config.as_ref(), buf) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                        KeyCode::Backspace => {
                            if buf.cursor > 0 {
                                buf.cursor -= 1;
                                buf.buf.remove(buf.cursor);
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Delete => {
                            if buf.cursor < buf.buf.len() {
                                buf.buf.remove(buf.cursor);
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Left => {
                            buf.cursor = buf.cursor.saturating_sub(1);
                            st.editing = Some(buf);
                        }
                        KeyCode::Right => {
                            if buf.cursor < buf.buf.len() {
                                buf.cursor += 1;
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Home => {
                            buf.cursor = 0;
                            st.editing = Some(buf);
                        }
                        KeyCode::End => {
                            buf.cursor = buf.buf.len();
                            st.editing = Some(buf);
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            buf.buf.insert(buf.cursor.min(buf.buf.len()), c);
                            buf.cursor += 1;
                            st.editing = Some(buf);
                        }
                        _ => st.editing = Some(buf),
                    }
                    self.replace_overlay(Overlay::Settings(st));
                    return true;
                }

                // ── 浏览态 ──
                let row = st.row();
                let id = row.id;
                let kind = row.kind;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlays.pop();
                        return true; // 关闭即丢弃草稿（标题有 ● 未保存提示）
                    }
                    KeyCode::Up | KeyCode::Char('k') => st.move_focus(-1),
                    KeyCode::Down | KeyCode::Char('j') => st.move_focus(1),
                    KeyCode::PageUp => st.move_focus(-8),
                    KeyCode::PageDown => st.move_focus(8),
                    KeyCode::Enter => match kind {
                        FieldKind::Text | FieldKind::Secret | FieldKind::Number | FieldKind::Float => {
                            st.editing = st.start_edit(self.config.as_ref());
                        }
                        FieldKind::Enum => {
                            if let Err(e) = st.cycle(self.config.as_ref(), 1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                        FieldKind::Toggle => {
                            let _ = st.cycle(self.config.as_ref(), 1);
                        }
                        FieldKind::Port => self.settings_port_activate(&mut st),
                    },
                    KeyCode::Left => match kind {
                        FieldKind::Port => self.settings_port_cycle(&mut st, -1),
                        _ => {
                            if let Err(e) = st.cycle(self.config.as_ref(), -1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                    },
                    KeyCode::Right => match kind {
                        FieldKind::Port => self.settings_port_cycle(&mut st, 1),
                        _ => {
                            if let Err(e) = st.cycle(self.config.as_ref(), 1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                    },
                    KeyCode::Char('s') | KeyCode::Char('S') => self.save_settings(&mut st),
                    KeyCode::Char('r') => self.fetch_config(),
                    // 权限级别：聚焦该行时数字键即时生效（沿用旧面板行为）。
                    KeyCode::Char(c @ '1'..='4') if id == settings::FieldId::PermissionLevel => {
                        self.set_permission_level(c as u8 - b'0');
                    }
                    _ => {}
                }
                self.replace_overlay(Overlay::Settings(st));
                return true;
            }
            Overlay::AttachPath { mut input, mut cursor, seed } => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlays.pop();
                    }
                    KeyCode::Enter => {
                        let path: String = input.iter().collect();
                        self.overlays.pop();
                        let path = path.trim().to_owned();
                        if !path.is_empty() {
                            self.upload_attachment(path);
                        }
                        let _ = seed;
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            input.remove(cursor - 1);
                            cursor -= 1;
                        }
                        self.replace_overlay(Overlay::AttachPath { input, cursor, seed });
                    }
                    KeyCode::Delete => {
                        if cursor < input.len() {
                            input.remove(cursor);
                        }
                        self.replace_overlay(Overlay::AttachPath { input, cursor, seed });
                    }
                    KeyCode::Left => {
                        cursor = cursor.saturating_sub(1);
                        self.replace_overlay(Overlay::AttachPath { input, cursor, seed });
                    }
                    KeyCode::Right => {
                        if cursor < input.len() {
                            cursor += 1;
                        }
                        self.replace_overlay(Overlay::AttachPath { input, cursor, seed });
                    }
                    KeyCode::Home => {
                        self.replace_overlay(Overlay::AttachPath { input, cursor: 0, seed });
                    }
                    KeyCode::End => {
                        let n = input.len();
                        self.replace_overlay(Overlay::AttachPath { input, cursor: n, seed });
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.insert(cursor.min(input.len()), c);
                        cursor += 1;
                        self.replace_overlay(Overlay::AttachPath { input, cursor, seed });
                    }
                    _ => {}
                }
                return true;
            }
            Overlay::Confirm { action } => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => match action {
                        ConfirmAction::DeleteSession(seed) => self.delete_session(seed.clone()),
                        ConfirmAction::ArchiveSession(seed) => self.archive_session(seed.clone()),
                        ConfirmAction::CloseTab(seed) => {
                            let seed = seed.clone();
                            self.close_tab_by_seed(&seed);
                        }
                    },
                    _ => {}
                }
                self.overlays.pop();
                return true;
            }
            Overlay::SessionList { selected, show_archived } => {
                let items = self.filtered_sessions(show_archived);
                let count = items.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlays.pop();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let next = selected.saturating_sub(1);
                        self.replace_overlay(Overlay::SessionList { selected: next, show_archived });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let next = (selected + 1).min(count.saturating_sub(1));
                        self.replace_overlay(Overlay::SessionList { selected: next, show_archived });
                    }
                    KeyCode::Char('a') => {
                        self.replace_overlay(Overlay::SessionList { selected, show_archived: !show_archived });
                    }
                    KeyCode::Char('r') => self.fetch_session_list(),
                    KeyCode::Char('n') => {
                        self.overlays.pop();
                        self.new_session();
                    }
                    KeyCode::Enter => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].seed.clone();
                            self.overlays.pop();
                            self.open_session_tab(&seed);
                        }
                    }
                    KeyCode::Char('x') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].seed.clone();
                            self.overlays.push(Overlay::Confirm { action: ConfirmAction::ArchiveSession(seed) });
                        }
                    }
                    KeyCode::Char('u') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].seed.clone();
                            self.unarchive_session(seed);
                        }
                    }
                    KeyCode::Char('D') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].seed.clone();
                            self.overlays.push(Overlay::Confirm { action: ConfirmAction::DeleteSession(seed) });
                        }
                    }
                    _ => {}
                }
                return true;
            }
            Overlay::CwdInput { mut input, mut cursor } => {
                match key.code {
                    KeyCode::Esc => { self.overlays.pop(); }
                    KeyCode::Enter => {
                        let raw: String = input.iter().collect();
                        self.overlays.pop();
                        self.confirm_cwd_input(raw);
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 { input.remove(cursor-1); cursor-=1; }
                        self.replace_overlay(Overlay::CwdInput{ input, cursor });
                    }
                    KeyCode::Delete => {
                        if cursor < input.len() { input.remove(cursor); }
                        self.replace_overlay(Overlay::CwdInput{ input, cursor });
                    }
                    KeyCode::Left => { cursor = cursor.saturating_sub(1); self.replace_overlay(Overlay::CwdInput{ input, cursor }); }
                    KeyCode::Right => { if cursor < input.len() { cursor+=1; } self.replace_overlay(Overlay::CwdInput{ input, cursor }); }
                    KeyCode::Home => { self.replace_overlay(Overlay::CwdInput{ input, cursor: 0 }); }
                    KeyCode::End => { let n = input.len(); self.replace_overlay(Overlay::CwdInput{ input, cursor: n }); }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.insert(cursor.min(input.len()), c); cursor+=1;
                        self.replace_overlay(Overlay::CwdInput{ input, cursor });
                    }
                    _ => {}
                }
                return true;
            }
        }
    }

    fn replace_overlay(&mut self, overlay: Overlay) {
        if !self.overlays.is_empty() {
            let n = self.overlays.len();
            self.overlays[n - 1] = overlay;
        }
    }

    // ── slash 二级菜单辅助 ──
    pub fn slash_visible(&self) -> bool {
        if !self.overlays.is_empty() {
            return false;
        }
        if let Some(sess) = self.active_session() {
            let val = sess.composer.value();
            !crate::app::slash::completions_for(&val).is_empty()
        } else {
            false
        }
    }

    pub fn slash_candidates(&self) -> Vec<crate::app::slash::SlashDef> {
        if let Some(sess) = self.active_session() {
            crate::app::slash::completions_for(&sess.composer.value())
                .into_iter()
                .cloned()
                .collect()
        } else {
            vec![]
        }
    }

    fn clamp_slash_selected(&mut self) {
        let n = self.slash_candidates().len();
        if n == 0 {
            self.slash_selected = 0;
        } else if self.slash_selected >= n {
            self.slash_selected = n - 1;
        }
    }

    fn autocomplete_slash(&mut self) {
        let candidates = self.slash_candidates();
        if candidates.is_empty() {
            return;
        }
        let idx = self.slash_selected.min(candidates.len() - 1);
        let name = candidates[idx].name;
        if let Some(sess) = self.active_session_mut() {
            let new = format!("/{name} ");
            sess.composer.input = new.chars().collect();
            sess.composer.cursor = sess.composer.input.len();
        }
        self.slash_selected = 0;
    }

    fn execute_slash_text(&mut self, raw: &str) -> bool {
        let trimmed = raw.trim();
        if trimmed.is_empty() || !trimmed.starts_with('/') {
            return false;
        }
        // 裸 "/" 留给菜单，不算命令
        if trimmed == "/" {
            return false;
        }
        let Some(cmd) = crate::app::slash::parse(trimmed) else {
            return false;
        };
        match cmd {
            SlashCmd::New { cwd } => {
                // 静默创建：按 effective_cwd 回退链；二级编辑仅按 Tab 按需触发
                match cwd {
                    Some(p) => {
                        let raw = p.trim().to_string();
                        if raw.is_empty() {
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(None);
                        } else if raw == "?" || raw.eq_ignore_ascii_case("edit") {
                            let initial = self.effective_cwd(None).unwrap_or_default();
                            self.overlays.push(Overlay::CwdInput { input: initial.chars().collect(), cursor: initial.len() });
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                        } else {
                            let expanded = crate::app::slash::expand_tilde(&raw);
                            if !crate::app::slash::is_absolute_path(&expanded) {
                                self.toast(NoticeLevel::Error, format!("cwd 需为绝对路径：{raw}"));
                                return true;
                            }
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(Some(expanded));
                        }
                    }
                    None => {
                        if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                        self.slash_selected = 0;
                        self.new_session_with_cwd(None);
                    }
                }
                true
            }
            SlashCmd::Help => {
                if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                self.slash_selected = 0;
                self.toggle_overlay(Overlay::Help);
                true
            }
            SlashCmd::Clear => {
                if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                self.slash_selected = 0;
                true
            }
            SlashCmd::Unknown(s) => {
                if s.is_empty() {
                    false
                } else {
                    self.toast(NoticeLevel::Error, format!("未知命令：/{s}"));
                    if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                    true
                }
            }
        }
    }

    /// CwdInput 二级弹窗确认
    fn confirm_cwd_input(&mut self, raw: String) {
        let cwd = raw.trim().to_owned();
        if cwd.is_empty() {
            self.new_session_with_cwd(None);
            return;
        }
        let cwd = crate::app::slash::expand_tilde(&cwd);
        if !crate::app::slash::is_absolute_path(&cwd) {
            self.toast(NoticeLevel::Error, format!("cwd 需为绝对路径：{cwd}"));
            return;
        }
        self.new_session_with_cwd(Some(cwd));
    }

    /// Composer 按键。
    fn composer_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 斜杠菜单优先：Up/Down/Tab/Esc/Enter 劫持（需在借用前计算）
        let slash_vis = self.slash_visible();
        match key.code {
            KeyCode::Esc if slash_vis => {
                self.slash_selected = 0;
                return;
            }
            KeyCode::Tab if slash_vis => {
                self.autocomplete_slash();
                return;
            }
            KeyCode::Tab => {
                // composer 为 /new 或 /n 且无参时，Tab 打开二级编辑（显式 CwdInput）
                let val = self.active_session().map(|s| s.composer.value()).unwrap_or_default();
                let trimmed = val.trim().to_string();
                if trimmed == "/new" || trimmed == "/n" {
                    let initial = self.effective_cwd(None).unwrap_or_default();
                    self.overlays.push(Overlay::CwdInput { input: initial.chars().collect(), cursor: initial.len() });
                    if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                    self.slash_selected = 0;
                    return;
                }
            }
            KeyCode::Enter if slash_vis || self.active_session().is_some_and(|s| s.composer.value().trim_start().starts_with('/')) => {
                // 若为 slash 输入，优先走 slash 执行或补全
                let val = self.active_session().map(|s| s.composer.value()).unwrap_or_default();
                let trimmed = val.trim().to_string();
                if trimmed.starts_with('/') {
                    if trimmed == "/" {
                        self.autocomplete_slash();
                        return;
                    }
                    // 若菜单可见且输入仍是前缀（无空格），Tab/Enter 应补全而非直接执行部分命令
                    let has_space = trimmed.contains(char::is_whitespace);
                    if slash_vis && !has_space {
                        // 若输入已是完整命令（如 "/new"），直接执行；否则补全
                        let without = trimmed[1..].to_ascii_lowercase();
                        let exact = crate::app::slash::SLASH_COMMANDS.iter().any(|d| d.name == without || (d.name == "new" && without == "n"));
                        if exact {
                            if self.execute_slash_text(&trimmed) { return; }
                        } else {
                            self.autocomplete_slash();
                            return;
                        }
                    } else if self.execute_slash_text(&trimmed) {
                        return;
                    } else if slash_vis {
                        self.autocomplete_slash();
                        return;
                    }
                }
                self.send_message();
                return;
            }
            KeyCode::Enter => { self.send_message(); return; },
            KeyCode::Esc => self.cancel_turn(),
            KeyCode::Backspace => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_left();
                        let cur = s.composer.cursor;
                        while s.composer.input.len() > cur {
                            s.composer.input.pop();
                        }
                    } else {
                        s.composer.backspace();
                    }
                    true
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
            }
            KeyCode::Delete => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    s.composer.delete();
                    true
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
            }
            KeyCode::Left => {
                if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_left();
                    } else {
                        s.composer.left();
                    }
                }
            }
            KeyCode::Right => {
                if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_right();
                    } else {
                        s.composer.right();
                    }
                }
            }
            KeyCode::Home => {
                if ctrl {
                    self.scroll_top();
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.home();
                }
            }
            KeyCode::End => {
                if ctrl {
                    self.scroll_bottom();
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.end();
                }
            }
            KeyCode::Up => {
                if slash_vis {
                    let n = self.slash_candidates().len();
                    if n > 0 {
                        if self.slash_selected == 0 { self.slash_selected = n - 1; } else { self.slash_selected -= 1; }
                    }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_up();
                }
            }
            KeyCode::Down => {
                if slash_vis {
                    let n = self.slash_candidates().len();
                    if n > 0 { self.slash_selected = (self.slash_selected + 1) % n; }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_down();
                }
            }
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.scroll_down(20),
            KeyCode::Char('a') if ctrl => {
                if let Some(seed) = self.active_seed() {
                    self.overlays.push(Overlay::AttachPath {
                        input: Vec::new(),
                        cursor: 0,
                        seed,
                    });
                }
            }
            KeyCode::Char('p') if ctrl => self.toggle_mode(),
            KeyCode::Char('y') if ctrl => self.undo_turn(),
            KeyCode::Char('e') if ctrl => self.compact(),
            KeyCode::Char('u') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    s.composer.clear();
                }
            }
            KeyCode::Char('k') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    let cur = s.composer.cursor;
                    s.composer.input.truncate(cur);
                }
            }
            KeyCode::Char('w') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    s.composer.word_left();
                    let cur = s.composer.cursor;
                    while s.composer.input.len() > cur {
                        s.composer.input.pop();
                    }
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    s.composer.insert(c);
                    true
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
            }
            _ => {}
        }
    }

    /// PageUp：滚动；到顶且还有更早回合 → 触发分页加载。
    fn page_up(&mut self) {
        let (total, at_limit, has_more, loading) = {
            let Some(sess) = self.active_session() else { return };
            let total = sess.rendered.as_ref().map(|r| r.lines.len()).unwrap_or(0);
            (total, sess.scroll.offset >= total.saturating_sub(1), sess.timeline.has_more, sess.loading_older)
        };
        self.scroll_up(20);
        if at_limit && has_more && !loading {
            self.load_older();
        }
        let _ = total;
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

    /// 焦点切换的内存回收（会话隔离的最后一环）：
    /// 1) 非 active 标签全部丢弃渲染缓存（聚焦时按需重建）；
    /// 2) 超出 LRU 窗口的标签丢弃 timeline 模型（轻状态/挂起交互/用量保留），
    ///    标记 needs_rebaseline；
    /// 3) 回到被逐出的标签时自动 re-baseline（服务端是权威历史）。
    fn touch_focus(&mut self, active: &str) {
        self.focus_order.retain(|s| s != active);
        self.focus_order.insert(0, active.to_owned());

        for (seed, s) in self.sessions.iter_mut() {
            if seed != active {
                s.rendered = None;
            }
        }

        let keep: HashSet<String> =
            self.focus_order.iter().take(ACTIVE_MODELS).cloned().collect();
        for (seed, s) in self.sessions.iter_mut() {
            if !keep.contains(seed) && s.ready && !s.needs_rebaseline {
                s.timeline = timeline_model::TimelineModel::default();
                s.rendered = None;
                s.ready = false;
                s.needs_rebaseline = true;
                s.scroll.follow = true;
                s.scroll.offset = 0;
            }
        }

        if self.sessions.get(active).is_some_and(|s| s.needs_rebaseline && !s.loading_older) {
            self.request_rebaseline(active);
        }
    }

    fn click_tab(&mut self, column: u16) {
        // 与 ui::tab_bar 的布局约定一致：品牌段 10 列，其后每 tab 占
        // " [n] title " 的宽度；仅处理前 9 个。
        let mut col: u16 = 10;
        for (idx, seed) in self.tabs.iter().enumerate().take(9) {
            let title = self.sessions.get(seed).map(|s| s.title()).unwrap_or_default();
            let label_w =
                format!(" {} {} ", idx + 1, truncate_str(&title, 18)).chars().count() as u16;
            if column >= col && column < col + label_w {
                self.active = idx;
                return;
            }
            col += label_w;
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
