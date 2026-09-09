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
use crate::protocol::timeline::{TimelineTool, TimelineTurnState};

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
    let output_seed = tool.output.as_deref().and_then(parse_spawn_output);
    let args_name = parse_agent_name(tool.args_json.as_deref());

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
            tool_call_id,
            name: SPAWN_TOOL.to_string(),
            state: crate::protocol::timeline::TimelineToolState::Succeeded,
            summary: None,
            args_json,
            output,
            diff: None,
            progress: String::new(),
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

/// `SubagentStatus.state` 标签 → 状态（容忍大小写与尾部附加信息，
/// 如 "ERROR exit=1" / "TIMEOUT after 120s"）。
pub fn state_from_tag(tag: &str) -> Option<SubagentState> {
    let upper = tag.to_ascii_uppercase();
    if upper.starts_with("COMPLETED") {
        Some(SubagentState::Completed)
    } else if upper.starts_with("ERROR") {
        Some(SubagentState::Error)
    } else if upper.starts_with("TIMEOUT") {
        Some(SubagentState::Timeout)
    } else if upper.starts_with("CANCELLED") {
        Some(SubagentState::Cancelled)
    } else {
        None
    }
}

/// 应用终态标签到（父会话中）同名且未终态的首个条目。
/// Closed 是 TUI 侧猜测（会话消失），权威终态标签可覆盖它。
/// 返回该条目的 seed（供调用方停止 timeline 跟踪）。
pub fn apply_status(sess: &mut SessionState, name: &str, tag: &str) -> Option<String> {
    let state = state_from_tag(tag)?;
    let entry = sess
        .subagents
        .iter_mut()
        .find(|e| e.name == name && (!e.state.is_terminal() || e.state == SubagentState::Closed))?;
    entry.state = state;
    entry.seed.clone()
}

/// 从子代理自身的 timeline 推导终态（SubagentStatus 缺失时的兜底，
/// 覆盖 TUI 中途接入 / 事件丢失的场景）。子代理只有一个任务回合：
/// 全部回合已 sealed 且非流式 → 终态。
pub fn derive_terminal(sub: &SessionState) -> Option<SubagentState> {
    if sub.timeline.turns.is_empty() || sub.timeline.is_streaming() {
        return None;
    }
    let finished = sub
        .timeline
        .turns
        .iter()
        .all(|t: &Turn| t.state != TimelineTurnState::Running);
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

/// 从工具输出 JSON（`json_ok` 形态：`{status:"ok", seed, name, ...}`）解析
/// (seed, name)。输出可能被截断或为纯文本 → 解析失败返回 None（不阻断）。
fn parse_spawn_output(output: &str) -> Option<(String, String)> {
    let trimmed = output.trim();
    // output 可能是嵌套 JSON 或带前缀文本；取首个 '{' 起尝试解析。
    let json_start = trimmed.find('{')?;
    let value: serde_json::Value = serde_json::from_str(&trimmed[json_start..]).ok()?;
    let seed = value.get("seed")?.as_str()?.to_owned();
    if seed.is_empty() {
        return None;
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("subagent")
        .to_owned();
    Some((seed, name))
}

/// 从工具参数 JSON 解析 `agent_name`（Prepared/Running 阶段尚无输出）。
fn parse_agent_name(args_json: Option<&str>) -> Option<String> {
    let args = args_json?;
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let name = value.get("agent_name")?.as_str()?.trim().to_owned();
    (!name.is_empty()).then_some(name)
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
        self.spawn_api(move |client, tx| async move {
            let cmd = build_envelope(
                &client,
                RingingCommand::Control(ControlCommand::SessionAttach { seed: seed.clone() }),
            )
            .with_seed(seed.clone());
            if let Err(e) = client.command(&cmd).await {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    seed: Some(seed.clone()),
                    label: "attach",
                    result: Err(e.to_string()),
                }));
                return;
            }
            let result = client.bootstrap(&seed).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
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
        if self.subagent_seeds.contains(seed)
            && let Some(sub) = self.sessions.get(seed)
            && let Some(state) = subagent::derive_terminal(sub)
        {
            for sess in self.sessions.values_mut() {
                for entry in sess.subagents.iter_mut() {
                    if entry.seed.as_deref() == Some(seed)
                        && (!entry.state.is_terminal() || entry.state == SubagentState::Closed)
                    {
                        entry.state = state;
                    }
                }
            }
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
    use crate::protocol::timeline::TimelineToolState;

    fn tool(
        name: &str,
        state: TimelineToolState,
        args: Option<&str>,
        output: Option<&str>,
    ) -> TimelineTool {
        TimelineTool {
            tool_call_id: "c1".into(),
            name: name.into(),
            state,
            summary: None,
            args_json: args.map(str::to_owned),
            output: output.map(str::to_owned),
            diff: None,
            progress: String::new(),
            failure: None,
            permission: None,
        }
    }

    #[test]
    fn prepared_then_output_discovers_seed_once() {
        let mut sess = SessionState::new("parent".into());
        let prepared = tool(
            SPAWN_TOOL,
            TimelineToolState::Prepared,
            Some(r#"{"agent_name":"explore"}"#),
            None,
        );
        assert_eq!(upsert_from_tool(&mut sess, &prepared), None);
        assert_eq!(sess.subagents.len(), 1);
        assert_eq!(sess.subagents[0].name, "explore");
        assert_eq!(sess.subagents[0].state, SubagentState::Starting);

        let done = tool(
            SPAWN_TOOL,
            TimelineToolState::Succeeded,
            Some(r#"{"agent_name":"explore"}"#),
            Some(r#"{"status":"ok","process_id":7,"seed":"abc123","name":"explore"}"#),
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
        let t = tool(
            SPAWN_TOOL,
            TimelineToolState::Failed,
            Some(r#"{"agent_name":"x"}"#),
            Some(r#"{"status":"error","code":"SPAWN_ERROR"}"#),
        );
        assert_eq!(upsert_from_tool(&mut sess, &t), None);
        assert_eq!(sess.subagents[0].state, SubagentState::Starting);
        assert!(sess.subagents[0].seed.is_none());
    }

    #[test]
    fn apply_status_matches_by_name_and_skips_terminal() {
        let mut sess = SessionState::new("parent".into());
        sess.subagents.push(SubagentEntry {
            tool_call_id: "c1".into(),
            seed: Some("s1".into()),
            name: "explore".into(),
            state: SubagentState::Running,
        });
        assert_eq!(
            apply_status(&mut sess, "explore", "ERROR exit=1").as_deref(),
            Some("s1")
        );
        assert_eq!(sess.subagents[0].state, SubagentState::Error);
        // 已终态：不再匹配（找不到非终态同名条目）。
        assert_eq!(apply_status(&mut sess, "explore", "COMPLETED"), None);
    }

    #[test]
    fn state_from_tag_variants() {
        assert_eq!(state_from_tag("COMPLETED"), Some(SubagentState::Completed));
        assert_eq!(state_from_tag("ERROR exit=1"), Some(SubagentState::Error));
        assert_eq!(
            state_from_tag("TIMEOUT after 120s"),
            Some(SubagentState::Timeout)
        );
        assert_eq!(state_from_tag("CANCELLED"), Some(SubagentState::Cancelled));
        assert_eq!(state_from_tag("RUNNING"), None);
    }

    #[test]
    fn derive_terminal_requires_sealed_turns() {
        // 空 timeline：未开始，不推导。
        let sub = SessionState::new("sub".into());
        assert_eq!(derive_terminal(&sub), None);
        // 带 turn 的情形由 timeline_model 集成行为覆盖，这里只锁空表语义。
    }
}
