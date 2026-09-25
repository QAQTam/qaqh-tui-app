//! 子代理（spawn_subagent）发现与状态模型 —— 只读观测。
//!
//! 数据来源（均已在协议内，无需后端新增事件）：
//! 1. 父会话 timeline 的 `spawn_subagent` 工具卡：args_json 带 `agent_name`，
//!    output（成功态）带 `{seed, name, process_id, ...}` JSON —— 子代理身份。
//! 2. `ControlEvent::SubagentStatus { seed(父), name, state }`：终态标签
//!    （COMPLETED / ERROR / TIMEOUT / CANCELLED），由结果注入父会话时发射。
//! 3. 子代理自身的 timeline 流：TurnStarted / 工具卡 / RoundCompleted 实时投影。
//!
//! 子代理完成后 daemon 会自动 SessionClose（ephemeral 会话目录删除），因此
//! 终态后调用方应停止跟踪其 timeline 流，但保留本地 SessionState 快照供查看。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::timeline_model::Turn;
use qaqh_client::{TimelineTool, TimelineToolBody, TimelineTurnState};

/// spawn_subagent 工具名（与后端 `qaqh-subagent` 注册的 key 一致）。
pub const SPAWN_TOOL: &str = "spawn_subagent";

/// 子代理生命周期状态。Running 只表示"已 spawn 且未收到终态"；
/// 实时活动性以子代理 SessionState.streaming 为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    /// 工具卡已出现（Prepared/Running），seed 尚未可知。
    Starting,
    /// 已拿到 seed，attach + timeline 订阅中。
    Running,
    Completed,
    Error,
    Timeout,
    Cancelled,
    /// 会话已被 daemon 关闭（终态后自动卸载）。
    Closed,
}

impl SubagentState {
    pub fn label(self) -> &'static str {
        match self {
            SubagentState::Starting => "starting",
            SubagentState::Running => "running",
            SubagentState::Completed => "completed",
            SubagentState::Error => "error",
            SubagentState::Timeout => "timeout",
            SubagentState::Cancelled => "cancelled",
            SubagentState::Closed => "closed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SubagentState::Completed
                | SubagentState::Error
                | SubagentState::Timeout
                | SubagentState::Cancelled
                | SubagentState::Closed
        )
    }
}

/// 一个子代理实例的观测条目（挂在父会话 SessionState 上，按 spawn 顺序）。
#[derive(Debug, Clone)]
pub struct SubagentEntry {
    /// 父会话 timeline 工具卡 id（身份锚点，rebaseline 后仍稳定）。
    pub tool_call_id: String,
    /// 子代理会话 seed；Starting 阶段尚无（工具输出未返回）。
    pub seed: Option<String>,
    pub name: String,
    pub state: SubagentState,
}

impl SubagentEntry {
    fn new(tool_call_id: String, name: String) -> Self {
        Self {
            tool_call_id,
            seed: None,
            name,
            state: SubagentState::Starting,
        }
    }
}

/// 从 `spawn_subagent` 工具卡增量 upsert 条目。
///
/// 返回 `Some(seed)` 表示**首次**发现该子代理 seed（调用方应 attach + 订阅
/// timeline）；已知 seed 的重复工具卡更新返回 None（幂等）。
pub fn upsert_from_tool(sess: &mut SessionState, tool: &TimelineTool) -> Option<String> {
    if tool.name != SPAWN_TOOL {
        return None;
    }
    // Phase C：display.body = Subagent { name, seed }（跨仓展示契约 §3.3）。
    // 投影期 name 取自 args（Prepared 即有），seed 仅在产出后非空。
    // display 缺失（旧 daemon）→ 无法发现子代理（观测特性要求新 daemon）。
    let display_pair = tool
        .display
        .as_ref()
        .and_then(|d| d.body.as_ref())
        .and_then(|b| match b {
            TimelineToolBody::Subagent { name, seed } => Some((seed.as_str(), name.as_str())),
            _ => None,
        });
    let output_seed = display_pair
        .filter(|(seed, _)| !seed.is_empty())
        .map(|(seed, name)| {
            (
                seed.to_string(),
                if name.trim().is_empty() {
                    "subagent".to_string()
                } else {
                    name.to_string()
                },
            )
        });
    let args_name = display_pair.and_then(|(seed, name)| {
        (seed.is_empty() && !name.trim().is_empty()).then(|| name.trim().to_string())
    });

    // 身份锚点：tool_call_id 优先；输出阶段可按 seed 并轨（正常不会分叉，
    // 但 rebaseline 重建后 tool_call_id 可能变化，seed 是稳定身份）。
    let idx = match output_seed {
        Some((ref seed, _)) => sess
            .subagents
            .iter()
            .position(|e| e.tool_call_id == tool.tool_call_id || e.seed.as_deref() == Some(seed)),
        None => sess
            .subagents
            .iter()
            .position(|e| e.tool_call_id == tool.tool_call_id),
    };
    let idx = match idx {
        Some(i) => i,
        None => {
            let name = args_name.clone().unwrap_or_else(|| "subagent".into());
            sess.subagents
                .push(SubagentEntry::new(tool.tool_call_id.clone(), name));
            sess.subagents.len() - 1
        }
    };
    let entry = &mut sess.subagents[idx];
    if let Some(name) = args_name
        && (entry.name == "subagent" || entry.seed.is_none())
    {
        entry.name = name;
    }
    match output_seed {
        Some((seed, name)) => {
            let newly = entry.seed.as_deref() != Some(seed.as_str());
            if entry.seed.is_none() || !name.is_empty() {
                entry.name = name;
            }
            entry.seed = Some(seed);
            // 失败的 spawn（SPAWN_ERROR 等）不会带 seed，走到这里即已派生成功。
            if entry.state == SubagentState::Starting {
                entry.state = SubagentState::Running;
            }
            newly.then_some(entry.seed.clone()).flatten()
        }
        None => {
            // Prepared/Running 阶段：保持 Starting。
            None
        }
    }
}

/// rebaseline 全量重扫：从重建后的 timeline 恢复条目。
/// 返回首次发现的 seed 集合（调用方逐个 attach）。
pub fn rescan(sess: &mut SessionState) -> Vec<String> {
    let mut discovered = Vec::new();
    // timeline_model 的 Block.tool 是展示镜像 ToolCard（字段与 wire TimelineTool 同名）。
    let cards: Vec<(String, Option<String>, Option<String>)> = sess
        .timeline
        .turns
        .iter()
        .flat_map(|t| t.rounds.iter())
        .flat_map(|r| r.blocks.iter())
        .filter_map(|b| b.tool.as_ref())
        .filter(|t| t.name == SPAWN_TOOL)
        .map(|t| {
            (
                t.tool_call_id.clone(),
                t.args_json.clone(),
                t.output.clone(),
            )
        })
        .collect();
    for (tool_call_id, args_json, output) in cards {
        let card = TimelineTool {
            display: None,
            progress_bytes_total: 0,
            progress_stream: None,
            tool_call_id,
            name: SPAWN_TOOL.to_string(),
            state: qaqh_client::TimelineToolState::Succeeded,
            summary: None,
            args_json,
            output,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        };
        if let Some(seed) = upsert_from_tool(sess, &card)
            && !discovered.contains(&seed)
        {
            discovered.push(seed);
        }
    }
    discovered
}

/// v2 `SubagentTerminalStatus` → 本仓子代理终态。
pub fn state_from_v2(status: qaqh_client::ClientV2SubagentTerminalStatus) -> SubagentState {
    use qaqh_client::ClientV2SubagentTerminalStatus as S;
    match status {
        S::Completed => SubagentState::Completed,
        S::Failed => SubagentState::Error,
        S::Cancelled => SubagentState::Cancelled,
        S::TimedOut => SubagentState::Timeout,
    }
}

/// v2 `SubagentSpawned` → 把 `child_session_id` 绑到 `parent_call_id` 对应的条目。
pub fn bind_seed(sess: &mut SessionState, parent_call_id: &str, child_session_id: &str) {
    if let Some(entry) = sess
        .subagents
        .iter_mut()
        .find(|e| e.tool_call_id == parent_call_id)
    {
        entry.seed = Some(child_session_id.to_string());
    }
}

/// v2 `SubagentFinished` → 按 `child_session_id` 落终态。
/// 返回该条目的 seed（供调用方停止 timeline 跟踪）。
pub fn apply_terminal(
    sess: &mut SessionState,
    child_session_id: &str,
    status: qaqh_client::ClientV2SubagentTerminalStatus,
) -> Option<String> {
    let state = state_from_v2(status);
    let entry = sess
        .subagents
        .iter_mut()
        .find(|e| e.seed.as_deref() == Some(child_session_id) && !e.state.is_terminal())?;
    entry.state = state;
    entry.seed.clone()
}

/// 从子代理自身的 timeline 推导终态（SubagentStatus 缺失时的兜底，
/// 覆盖 TUI 中途接入 / 事件丢失的场景）。子代理只有一个任务回合：
/// 全部回合已 **sealed** 且非 Running → 终态。
///
/// `sealed` 是比 `state != Running` 更保守的闸门：timeline 快照可能滞后于
/// daemon 的真实封口/reopen 时序，窗口期内非 sealed 的回合不应被推导成终态，
/// 否则会提前 `untrack` 掉仍在更新的 timeline 流（见 issue #2 的 U-29）。
pub fn derive_terminal(sub: &SessionState) -> Option<SubagentState> {
    if sub.timeline.turns.is_empty() || sub.timeline.is_streaming() {
        return None;
    }
    let finished = sub
        .timeline
        .turns
        .iter()
        .all(|t: &Turn| t.sealed && t.state != TimelineTurnState::Running);
    if !finished {
        return None;
    }
    let failed =
        sub.timeline.turns.iter().any(|t| {
            t.state == TimelineTurnState::Failed || t.state == TimelineTurnState::Cancelled
        });
    Some(if failed {
        SubagentState::Error
    } else {
        SubagentState::Completed
    })
}

// ───────────────────────── App 集成 ─────────────────────────

/// 子代理观测的生命周期：发现 → attach → 订阅 →（只读）查看 → 终态卸载。
impl App {
    // ── 视图栈 ──

    /// 当前正在查看的会话 seed：inspect 优先，否则活动标签。
    pub fn view_seed(&self) -> Option<String> {
        self.inspect.clone().or_else(|| self.active_seed())
    }

    pub fn view_session(&self) -> Option<&SessionState> {
        let seed = self.view_seed()?;
        self.sessions.get(&seed)
    }

    pub fn inspecting(&self) -> bool {
        self.inspect.is_some()
    }

    pub(super) fn exit_inspect(&mut self) {
        self.inspect = None;
        // 返回父会话：重置焦点触发 touch_focus / 渲染缓存重建。
        self.last_focused = None;
        // 这里**不**剪 overlay：`inspect` 不改变活动标签，而所有会改变它的路径
        // （Alt+数字/方向、点击标签、open_session_tab、close_tab_by_seed）自己
        // 已经剪过；唯一由 Esc 进入的这条路径还要先过 `overlay_key`，那时 overlay
        // 栈必然为空。原先这里那次调用在任何可达状态下都删不掉东西（no-op）。
    }

    // ── 发现与跟踪 ──

    /// timeline 增量钩子：spawn_subagent 工具卡 → upsert 条目；首次拿到
    /// seed 时建立跟踪。
    pub(super) fn discover_spawn_tool(&mut self, parent: &str, tool: &TimelineTool) {
        let discovered = {
            let Some(sess) = self.sessions.get_mut(parent) else {
                return;
            };
            subagent::upsert_from_tool(sess, tool)
        };
        if let Some(seed) = discovered {
            self.ensure_subagent_tracked(parent, &seed);
        }
    }

    /// 确保 子代理 SessionState 存在 + seed 进入 timeline 跟踪 + attach。
    pub(super) fn ensure_subagent_tracked(&mut self, parent: &str, seed: &str) {
        if seed.is_empty() || parent == seed {
            return;
        }
        if !self.sessions.contains_key(seed) {
            self.sessions
                .insert(seed.to_owned(), SessionState::new(seed.to_owned()));
            self.subagent_seeds.insert(seed.to_owned());
            self.sync_tracked();
            self.attach_subagent_seed(seed.to_owned());
        }
    }

    /// SessionAttach（无 actor 副作用）→ bootstrap。timeline 流由 runtime 在
    /// seed 进入跟踪集后自动建立（attach 未落地前短退避重试）。
    fn attach_subagent_seed(&mut self, seed: String) {
        self.spawn_api(move |api, tx| async move {
            let attach = api
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(ControlCommand::SessionAttach { seed: seed.clone() }),
                    Default::default(),
                )
                .await;
            if let Err(e) = attach {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    seed: Some(seed.clone()),
                    label: "attach",
                    result: Err(e),
                }));
                return;
            }
            let result = api.bootstrap(&seed).await;
            let client_session_id = api.v2_client_session_id().await;
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap {
                seed,
                result,
                client_session_id,
            }));
        });
    }

    /// 终态/会话消失：停止 timeline 跟踪（保留本地 SessionState 快照供查看）。
    pub(super) fn untrack_subagent(&mut self, seed: &str) {
        if self.subagent_seeds.remove(seed) {
            self.sync_tracked();
        }
    }

    /// 把 seed 对应的（各父会话中的）子代理条目标记为 Closed。
    pub(super) fn mark_subagent_closed(&mut self, seed: &str) {
        for sess in self.sessions.values_mut() {
            for entry in sess.subagents.iter_mut() {
                if entry.seed.as_deref() == Some(seed) && !entry.state.is_terminal() {
                    entry.state = SubagentState::Closed;
                }
            }
        }
        self.untrack_subagent(seed);
    }

    /// rebaseline 钩子：从重建的 timeline 恢复条目 + 发现中途接管的子代理；
    /// 对子代理自身的 rebaseline 做终态兑底推导。
    pub(super) fn handle_subagent_rebaseline(&mut self, seed: &str) {
        let discovered = {
            let Some(sess) = self.sessions.get_mut(seed) else {
                return;
            };
            subagent::rescan(sess)
        };
        for s in discovered {
            self.ensure_subagent_tracked(seed, &s);
        }
        let derived = if self.subagent_seeds.contains(seed) {
            self.sessions.get(seed).and_then(subagent::derive_terminal)
        } else {
            None
        };
        if let Some(state) = derived {
            for sess in self.sessions.values_mut() {
                for entry in sess.subagents.iter_mut() {
                    if entry.seed.as_deref() == Some(seed)
                        && (!entry.state.is_terminal() || entry.state == SubagentState::Closed)
                    {
                        entry.state = state;
                    }
                }
            }
            // 派生终态与 `ControlEvent::SubagentStatus` 同等权威：同样停止该
            // seed 的 timeline 跟踪（daemon 随后 SessionClose）。漏掉这一步会
            // 让终态子代理的流一直挂着，直到连接重建才被动收口。
            self.untrack_subagent(seed);
        }
    }

    // ── 按键导航 ──

    /// 子代理观测导航：Ctrl+↑ 深入/在同级间循环（支持嵌套逐层深入）、
    /// Ctrl+↓ 逐层返回、Esc 直达标签视图。均以弹窗/覆盖层优先为前提。
    /// 返回 true 表示按键已消费。
    pub(super) fn subagent_nav_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if ctrl => {
                // 基准 = 当前视图（观测中 = 子代理本身，支持嵌套逐层深入）。
                let has_subagents = self.view_session().is_some_and(|s| !s.subagents.is_empty());
                if !has_subagents {
                    // 无子代理：不消费（交回 composer，如多行输入移动光标）。
                    return false;
                }
                self.cycle_subagent();
                true
            }
            KeyCode::Down if ctrl => {
                // 逐层返回：子代理 → 其父；已是标签直属 → 回到标签视图。
                if let Some(cur) = self.inspect.clone() {
                    self.inspect = self.subagent_parent(&cur);
                    self.last_focused = None;
                    true
                } else {
                    false
                }
            }
            KeyCode::Esc if self.inspecting() => {
                self.exit_inspect();
                true
            }
            _ => false,
        }
    }

    /// 在活动会话的子代理间循环切换：未观测时进入“最近优先”的子代理
    /// （最后一个非终态，否则最后一个）；已观测时顺序前进并回绕。
    fn cycle_subagent(&mut self) {
        let Some(base) = self.view_seed() else {
            return;
        };
        let viewable: Vec<String> = self
            .sessions
            .get(&base)
            .map(|sess| {
                sess.subagents
                    .iter()
                    .filter_map(|e| e.seed.clone())
                    .collect()
            })
            .unwrap_or_default();
        if viewable.is_empty() {
            return;
        }
        let next = match &self.inspect {
            Some(cur) => {
                let idx = viewable
                    .iter()
                    .position(|s| s == cur)
                    .map(|i| (i + 1) % viewable.len())
                    .unwrap_or(0);
                viewable[idx].clone()
            }
            None => {
                // 最近优先：最后一个 Running，否则最后一个。
                let running = self.sessions.get(&base).and_then(|s| {
                    s.subagents
                        .iter()
                        .filter(|e| e.state == SubagentState::Running)
                        .filter_map(|e| e.seed.clone())
                        .next_back()
                });
                running.or_else(|| viewable.last().cloned()).unwrap()
            }
        };
        self.inspect = Some(next);
        self.last_focused = None; // 触发 touch_focus（保护被观察 seed 的缓存）
    }

    /// 查找某子代理 seed 的直属父会话 seed（Ctrl+↓ 逐层上溯用）。
    /// 直属父是标签会话 → None（回到标签视图）；直属父也是子代理 → 返回
    /// 该父 seed（继续观测上一层）。
    pub(crate) fn subagent_parent(&self, sub_seed: &str) -> Option<String> {
        for (seed, sess) in &self.sessions {
            if sess
                .subagents
                .iter()
                .any(|e| e.seed.as_deref() == Some(sub_seed))
            {
                return (!self.tabs.contains(seed)).then(|| seed.clone());
            }
        }
        None
    }

    /// 观测态按键：仅滚动/翻页作用于子代理视图（由 handle_key 在吞掉其余
    /// 按键前调用）。
    pub(super) fn inspect_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.scroll_down(20),
            KeyCode::Home if ctrl => self.scroll_top(),
            KeyCode::End if ctrl => self.scroll_bottom(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::TimelineToolState;

    fn tool(
        name: &str,
        state: TimelineToolState,
        args: Option<&str>,
        output: Option<&str>,
    ) -> TimelineTool {
        TimelineTool {
            display: None,
            progress_bytes_total: 0,
            progress_stream: None,
            tool_call_id: "c1".into(),
            name: name.into(),
            state,
            summary: None,
            args_json: args.map(str::to_owned),
            output: output.map(str::to_owned),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            failure: None,
            permission: None,
        }
    }

    /// Phase C fixture：spawn 工具卡挂上 Subagent 投影（name 常显、seed 产出后非空）。
    fn with_subagent_display(t: TimelineTool, name: &str, seed: &str) -> TimelineTool {
        TimelineTool {
            display: Some(qaqh_client::TimelineToolDisplay {
                summary: None,
                diff: None,
                header: Some(qaqh_client::TimelineToolHeader::Other {
                    label: "subagent".into(),
                }),
                body: Some(qaqh_client::TimelineToolBody::Subagent {
                    name: name.into(),
                    seed: seed.into(),
                }),
                metrics: None,
                outcome: None,
            }),
            ..t
        }
    }

    #[test]
    fn prepared_then_output_discovers_seed_once() {
        let mut sess = SessionState::new("parent".into());
        let prepared = with_subagent_display(
            tool(SPAWN_TOOL, TimelineToolState::Prepared, None, None),
            "explore",
            "",
        );
        assert_eq!(upsert_from_tool(&mut sess, &prepared), None);
        assert_eq!(sess.subagents.len(), 1);
        assert_eq!(sess.subagents[0].name, "explore");
        assert_eq!(sess.subagents[0].state, SubagentState::Starting);

        let done = with_subagent_display(
            tool(SPAWN_TOOL, TimelineToolState::Succeeded, None, None),
            "explore",
            "abc123",
        );
        assert_eq!(
            upsert_from_tool(&mut sess, &done).as_deref(),
            Some("abc123")
        );
        // 同一卡片重放（timeline 幂等）不再报新发现。
        assert_eq!(upsert_from_tool(&mut sess, &done), None);
        assert_eq!(sess.subagents[0].state, SubagentState::Running);
        assert_eq!(sess.subagents[0].seed.as_deref(), Some("abc123"));
    }

    #[test]
    fn non_spawn_tool_ignored() {
        let mut sess = SessionState::new("parent".into());
        let t = tool(
            "bash",
            TimelineToolState::Succeeded,
            None,
            Some(r#"{"seed":"x"}"#),
        );
        assert_eq!(upsert_from_tool(&mut sess, &t), None);
        assert!(sess.subagents.is_empty());
    }

    #[test]
    fn failed_spawn_keeps_starting_without_seed() {
        let mut sess = SessionState::new("parent".into());
        let t = with_subagent_display(
            tool(SPAWN_TOOL, TimelineToolState::Failed, None, None),
            "x",
            "",
        );
        assert_eq!(upsert_from_tool(&mut sess, &t), None);
        assert_eq!(sess.subagents[0].state, SubagentState::Starting);
        assert!(sess.subagents[0].seed.is_none());
    }

    #[test]
    fn apply_terminal_matches_by_seed_and_skips_terminal() {
        use qaqh_client::ClientV2SubagentTerminalStatus as S;
        let mut sess = SessionState::new("parent".into());
        sess.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: Some("s1".into()),
            name: "explore".into(),
            state: SubagentState::Running,
        });
        assert_eq!(
            apply_terminal(&mut sess, "s1", S::Failed).as_deref(),
            Some("s1")
        );
        assert_eq!(sess.subagents[0].state, SubagentState::Error);
        // 已终态：不再匹配（找不到非终态条目）。
        assert_eq!(apply_terminal(&mut sess, "s1", S::Completed), None);
    }

    #[test]
    fn bind_seed_sets_child_session_on_parent_call() {
        let mut sess = SessionState::new("parent".into());
        sess.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: None,
            name: "explore".into(),
            state: SubagentState::Starting,
        });
        bind_seed(&mut sess, "c1", "s1");
        assert_eq!(sess.subagents[0].seed.as_deref(), Some("s1"));
    }

    #[test]
    fn state_from_v2_variants() {
        use qaqh_client::ClientV2SubagentTerminalStatus as S;
        assert_eq!(state_from_v2(S::Completed), SubagentState::Completed);
        assert_eq!(state_from_v2(S::Failed), SubagentState::Error);
        assert_eq!(state_from_v2(S::TimedOut), SubagentState::Timeout);
        assert_eq!(state_from_v2(S::Cancelled), SubagentState::Cancelled);
    }

    #[test]
    fn derive_terminal_requires_sealed_turns() {
        use crate::app::timeline_model::Turn;

        fn turn(state: TimelineTurnState, sealed: bool) -> Turn {
            Turn {
                thinking: Default::default(),
                turn_id: "t1".into(),
                turn_index: Some(1),
                user_text: String::new(),
                state,
                failure: None,
                sealed,
                offloaded: false,
                rounds: Vec::new(),
            }
        }

        // 空 timeline：未开始，不推导。
        let mut sub = SessionState::new("sub".into());
        assert_eq!(derive_terminal(&sub), None);

        // 新闸门：Completed 但尚未 sealed 的回合不得推导终态——快照可能滞后，
        // 提前 untrack 会停掉仍在更新的 timeline 流。
        sub.timeline
            .turns
            .push(turn(TimelineTurnState::Completed, false));
        assert_eq!(
            derive_terminal(&sub),
            None,
            "unsealed Completed turn must not derive terminal"
        );

        // 同一回合封口后才允许推导。
        sub.timeline.turns[0].sealed = true;
        assert_eq!(derive_terminal(&sub), Some(SubagentState::Completed));

        // 任一 Running 回合都阻断终态（sealed 也不例外）。
        sub.timeline
            .turns
            .push(turn(TimelineTurnState::Running, true));
        assert_eq!(derive_terminal(&sub), None);

        // Failed / Cancelled 封口后推导为 Error。
        sub.timeline.turns.pop();
        sub.timeline.turns[0].state = TimelineTurnState::Failed;
        assert_eq!(derive_terminal(&sub), Some(SubagentState::Error));
        sub.timeline.turns[0].state = TimelineTurnState::Cancelled;
        assert_eq!(derive_terminal(&sub), Some(SubagentState::Error));
    }

    /// issue #2 缺陷 2：派生终态（`SubagentStatus` 缺失时的兜底）必须与
    /// `ControlEvent::SubagentStatus` 同等收口——标记状态**并停止 timeline 跟踪**。
    #[test]
    fn derived_terminal_also_stops_tracking() {
        use crate::app::timeline_model::Turn;
        let (mut app, _rx) = App::new_for_test();
        let sub = "seed-sub";

        // 子代理自身 timeline：唯一回合已封口且非 Running → derive_terminal = Completed。
        let mut st = SessionState::new(sub.into());
        st.timeline.turns.push(Turn {
            thinking: Default::default(),
            turn_id: "t1".into(),
            turn_index: Some(1),
            user_text: String::new(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            rounds: Vec::new(),
        });
        app.sessions.insert(sub.into(), st);
        app.subagent_seeds.insert(sub.into());

        let mut parent = SessionState::new("parent".into());
        parent.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: Some(sub.into()),
            name: "explore".into(),
            state: SubagentState::Running,
        });
        app.sessions.insert("parent".into(), parent);

        app.handle_subagent_rebaseline(sub);

        assert_eq!(
            app.sessions["parent"].subagents[0].state,
            SubagentState::Completed,
            "派生终态仍要落到父会话条目上"
        );
        assert!(
            !app.subagent_seeds.contains(sub),
            "派生终态必须停止该 seed 的 timeline 跟踪（否则流一直挂着）"
        );
    }
}
