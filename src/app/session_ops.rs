//! 会话/标签生命周期与 LRU 焦点管理（自 app/mod.rs 按域拆分，行为不变）。

use super::*;

/// 左侧会话栏的一行（渲染与点击共用同一份数据事实源）。
#[derive(Debug)]
pub struct SidebarRow {
    pub session_id: String,
    pub title: String,
    /// 领域活动状态；`None` = daemon 本生命周期内从未激活过（仅 running 兜底）。
    pub activity: Option<ActivityState>,
    /// daemon registry 实时查询：该会话当前是否有 worker 在跑。
    pub running: bool,
    /// 已在当前 tab 集里。
    pub is_open: bool,
    /// 当前 active tab。
    pub is_active: bool,
}

impl App {
    /// 周期刷新**只更新内容，不重排**。
    ///
    /// daemon 的 `session.list` 按 `updated_at` 降序（`SessionManager::list`），而
    /// `updated_at` **每写一条消息就刷新一次**（`save_one`）。TUI 每 3 秒（首页 /
    /// 历史的会话列表）或 8 秒（侧栏）整表拉一次，于是这段时间里写过东西的会话
    /// 就会往上跳——列表在光标底下换位，点错行是必然的。
    ///
    /// 策略：**老面孔留在原位**（只换内容），**新面孔按 daemon 的顺序插到最前**。
    /// 首次装载（缓存为空）直接采信 daemon 的顺序（"最近更新优先"仍然是初始口径）。
    pub(super) fn stable_session_order(
        &self,
        fresh: Vec<SessionListEntry>,
    ) -> Vec<SessionListEntry> {
        if self.session_list_cache.is_empty() {
            return fresh;
        }
        let mut slots: Vec<Option<SessionListEntry>> = fresh.into_iter().map(Some).collect();
        let mut kept: Vec<SessionListEntry> = Vec::with_capacity(slots.len());
        for old in &self.session_list_cache {
            let Some(pos) = slots.iter().position(|slot| {
                slot.as_ref()
                    .is_some_and(|entry| entry.meta.session_id == old.meta.session_id)
            }) else {
                // 已删除 / 已归档：自然从列表里消失。
                continue;
            };
            if let Some(entry) = slots[pos].take() {
                kept.push(entry);
            }
        }
        let mut ordered: Vec<SessionListEntry> = slots.into_iter().flatten().collect();
        ordered.extend(kept);
        ordered
    }

    /// 侧栏数据源：daemon 启动后**被激活过**的会话（activity tracker 有快照）
    /// 加上 registry 里仍在跑的会话，归档的永不出现。
    ///
    /// 顺序沿用 `session_list_cache`（daemon 侧已按 updated_at 排序），打开的
    /// tab 不重排——侧栏是"监控位"，不是 tab 栏镜像。
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        self.session_list_cache
            .iter()
            .filter(|entry| !entry.meta.archived)
            // 子代理**不是**顶层会话：`session.list` 不区分父子（daemon 侧
            // `list_sessions` 原样列出所有会话），所以过滤只能在前端做。子代理的
            // 存在感收敛到子代理预览条（见 `subagent_strip_line`）与 Ctrl+↑。
            .filter(|entry| !self.is_subagent_session(&entry.meta.session_id))
            .filter(|entry| {
                entry.running
                    || self.activity_cache.contains_key(&entry.meta.session_id)
                    || self.tabs.contains(&entry.meta.session_id)
            })
            .map(|entry| {
                let session_id = entry.meta.session_id.clone();
                SidebarRow {
                    is_open: self.tabs.contains(&session_id),
                    is_active: self
                        .tabs
                        .get(self.active)
                        .is_some_and(|active| active == &session_id),
                    title: entry.meta.display_title(),
                    activity: self.activity_cache.get(&session_id).copied(),
                    running: entry.running,
                    session_id,
                }
            })
            .collect()
    }

    /// 侧栏行点击：已打开的 tab 直接聚焦，未打开的走既有 open（attach+bootstrap）。
    /// 全部复用 [`App::open_session_tab`]，不另开语义。
    pub fn sidebar_open(&mut self, index: usize) {
        let Some(session_id) = self.sidebar_rows().get(index).map(|row| row.session_id.clone())
        else {
            return;
        };
        self.open_session_tab(&session_id);
    }

    pub fn open_session_tab(&mut self, session_id: &str) {
        if self.tabs.iter().any(|s| s == session_id) {
            self.active = self.tabs.iter().position(|s| s == session_id).unwrap_or(0);
            self.prune_overlays_for_active_session_id();
            self.fetch_team(session_id.to_owned());
            // 目标 tab 可能一直挂在后台（停流状态）：重新挂流，
            // 停流期间错过的 delta 由 activate 快照重基线补齐。
            self.sync_tracked();
            return;
        }
        self.tabs.push(session_id.to_owned());
        self.sessions.insert(
            session_id.to_owned(),
            SessionState::new(session_id.to_owned()),
        );
        self.active = self.tabs.len() - 1;
        self.prune_overlays_for_active_session_id();
        self.sync_tracked();
        // attach + bootstrap（timeline 流由 runtime 自动建立）。
        self.attach_and_bootstrap(session_id.to_owned());
    }

    pub(super) fn attach_and_bootstrap(&mut self, session_id: String) {
        self.spawn_api(move |api, tx| async move {
            let ack = api
                .send_command(
                    Some(&session_id),
                    RingingCommand::Control(ControlCommand::SessionResume {
                        session_id: session_id.clone(),
                    }),
                    Default::default(),
                )
                .await;
            if let Err(e) = ack {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    session_id: Some(session_id.clone()),
                    label: "resume",
                    result: Err(e),
                }));
                return;
            }
            // Snapshot first: TeamDelta is ephemeral and is not replayed.
            let team = api.team_v2(&session_id).await;
            let _ = tx.send(AppMsg::Action(ActionResult::Team {
                session_id: session_id.clone(),
                result: team,
            }));
            let result = api.bootstrap(&session_id).await;
            let client_session_id = api.v2_client_session_id().await;
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap {
                session_id,
                result,
                client_session_id,
            }));
        });
    }

    pub fn new_session(&mut self) {
        self.new_session_with_cwd(None);
    }

    /// 品牌首屏提交：先把输入暂存，等新会话 session_id 落成后带进真实 composer。
    ///
    /// 空输入仍创建一个空会话，行为对齐原来的 Ctrl+N；有输入时不在前端预造
    /// timeline，避免出现“本地临时消息 + 后端回放”双份正文。首条消息不自动发送，
    /// 用户能在会话 composer 里继续编辑后再按 Enter。
    pub(super) fn start_draft_conversation(&mut self) {
        let text = self.draft_composer.value();
        self.draft_composer.clear();
        if text.trim().is_empty() {
            self.new_session();
            return;
        }
        self.pending_initial_prompt = Some(text);
        self.new_session_with_cwd(None);
    }

    /// `SessionCreate` 已确认后调用：把首条草稿带进真实会话 composer。
    pub(super) fn transfer_pending_initial_prompt(&mut self) {
        let Some(text) = self.pending_initial_prompt.take() else {
            return;
        };
        let Some(session_id) = self.active_session_id() else {
            self.pending_initial_prompt = Some(text);
            return;
        };
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.composer.input = text.chars().collect();
            session.composer.cursor = session.composer.input.len();
        } else {
            self.pending_initial_prompt = Some(text);
        }
    }

    /// 创建命令没有进入 daemon，或等待 `SessionCreated` 超时：撤销 creating 状态，
    /// 并把开屏首条草稿放回可编辑输入框，避免用户输入丢失或状态栏永久卡住。
    pub(super) fn abort_pending_create(&mut self, message: impl Into<String>) {
        let had_pending = !self.pending_creates.is_empty();
        self.pending_creates.clear();
        if let Some(text) = self.pending_initial_prompt.take() {
            if self.tabs.is_empty() {
                self.draft_composer.insert_str(&text);
            } else if let Some(session_id) = self.active_session_id()
                && let Some(session) = self.sessions.get_mut(&session_id)
            {
                session.composer.input = text.chars().collect();
                session.composer.cursor = session.composer.input.len();
            } else {
                self.pending_initial_prompt = Some(text);
            }
        }
        if had_pending {
            self.toast(NoticeLevel::Error, message);
        }
    }

    /// 三档回退：显式 > 环境变量 > 启动目录 > None（让后端迁移）
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
        // 末级回退曾是 `active_session().meta.cwd`——`SessionState::meta` 恒为 None
        // （见 `SessionState::title` 的注），故这一级从未生效。G2 时删除。
        None
    }

    pub fn new_session_with_cwd(&mut self, cwd: Option<String>) {
        let cwd = self.effective_cwd(cwd);
        // command_id 由本侧生成并透传：新会话要靠 `causation_id == command_id`
        // 关联（`pending_creates`），不能让客户端自己造一个我们不知道的 id。
        let command_id = uuid::Uuid::new_v4().to_string();
        self.pending_creates
            .insert(command_id.clone(), Instant::now());
        // 立刻作废列表缓存：新建会话的落地结果由**列表**兜底发现（见
        // `App::handle_action` 的 `ActionResult::SessionList`）。见下方
        // `new_session_with_cwd` 的长注释——`Created` 事件在实践中不保证到达，
        // 不能把「新会话开出来」只押在那一条路径上。
        self.session_list_at = None;
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    None,
                    RingingCommand::Control(ControlCommand::SessionCreate {
                        close_current: false,
                        cwd,
                        tool_mode: None,
                        custom_tools: vec![],
                    }),
                    qaqh_client::CommandOptions {
                        command_id: Some(command_id),
                        ..Default::default()
                    },
                )
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: None,
                label: "新会话",
                result,
            }));
        });
    }

    pub(super) fn close_tab_by_session_id(&mut self, session_id: &str) {
        // 子代理回收**不依赖** session_id 是否在本地 `tabs`：daemon 主动关父会话、
        // 或父本身就是子代理时，父 session_id 从来不在 tabs 里——旧实现把整段回收罩在
        // `tabs` 命中内，这些子代理的 timeline 流与本地快照就永远留在跟踪集里。
        self.reclaim_subagents(session_id);
        if let Some(pos) = self.tabs.iter().position(|s| s == session_id) {
            self.tabs.remove(pos);
            self.sessions.remove(session_id);
            self.teams.remove(session_id);
            self.tracked_session_ids.remove(session_id);
            if self.active >= self.tabs.len() && self.active > 0 {
                self.active = self.tabs.len() - 1;
            }
            // 活动标签可能已经换成别的 session_id：旧 session_id 的确认/附件 overlay 作废。
            self.prune_overlays_for_active_session_id();
            self.sync_tracked();
            return;
        }
        // 不在 tabs：没有标签栈可调。会话确实已消失 → 停止 live timeline；
        // roster 条目与本地快照保留，终态仍可查看。
        self.untrack_subagent(session_id);
        self.tracked_session_ids.remove(session_id);
        self.sync_tracked();
    }

    /// 按父子关系回收 `session_id` 名下的全部子代理（含多层嵌套）：停止 timeline
    /// 跟踪并移除本地快照（子代理视图无宿主；daemon 侧 ephemeral 会话自会回收）。
    ///
    /// 先收集整个后代集合再统一删除——边删边找会把孙代一起弄丢（父条目随
    /// 快照删除后，父子关系就无从查起）。
    fn reclaim_subagents(&mut self, session_id: &str) {
        let mut descendants: Vec<String> = Vec::new();
        let mut frontier = vec![session_id.to_owned()];
        while let Some(parent) = frontier.pop() {
            let children: Vec<String> = self.child_agent_ids(&parent);
            for child in children {
                if child == parent || descendants.contains(&child) {
                    continue;
                }
                descendants.push(child.clone());
                frontier.push(child);
            }
        }
        for sub in descendants {
            self.untrack_subagent(&sub);
            self.sessions.remove(&sub);
            if self.inspect.as_deref() == Some(sub.as_str()) {
                self.inspect = None;
            }
        }
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.tabs.get(self.active).cloned()
    }

    pub fn active_session(&self) -> Option<&SessionState> {
        self.tabs
            .get(self.active)
            .and_then(|s| self.sessions.get(s))
    }

    pub(crate) fn active_session_mut(&mut self) -> Option<&mut SessionState> {
        let session_id = self.tabs.get(self.active)?.clone();
        self.sessions.get_mut(&session_id)
    }

    // ───────────────────────── 命令发送 ─────────────────────────

    pub fn fetch_session_list(&mut self) {
        self.spawn_api(move |api, tx| async move {
            // session.list 回数组，逐项解析为**权威类型**（G2）。
            // 仍逐项宽松：单条形状不符只跳过该条，不让整个列表失败——
            // 与 G1 同款「解析失败一律降级而不是崩」的契约。
            let list = api.query(QueryRequest::SessionList).await.and_then(|v| {
                v.as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                serde_json::from_value::<SessionListEntry>(item.clone()).ok()
                            })
                            .collect()
                    })
                    .ok_or_else(|| "session.list 应返回数组".to_string())
            });
            let _ = tx.send(AppMsg::Action(ActionResult::SessionList(list)));
            // 权威类型：逐项宽松解析（单条形状不符只跳过该条，与 session.list 同款）。
            let activity = api
                .query(QueryRequest::SessionActivity)
                .await
                .and_then(|v| {
                    v.as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| {
                                    serde_json::from_value::<SessionActivity>(item.clone()).ok()
                                })
                                .collect()
                        })
                        .ok_or_else(|| "session.activity 应返回数组".to_string())
                });
            let _ = tx.send(AppMsg::Action(ActionResult::SessionActivity(activity)));
        });
    }

    pub fn archive_session(&mut self, session_id: String) {
        self.send_control_command(
            session_id.clone(),
            ControlCommand::SessionArchive { session_id },
            "归档",
        );
    }

    pub fn unarchive_session(&mut self, session_id: String) {
        self.send_control_command(
            session_id.clone(),
            ControlCommand::SessionUnarchive { session_id },
            "取消归档",
        );
    }

    pub fn delete_session(&mut self, session_id: String) {
        self.send_control_command(
            session_id.clone(),
            ControlCommand::SessionDelete { session_id },
            "删除",
        );
    }

    pub(super) fn fetch_dashboard(&mut self, session_id: String) {
        if session_id.is_empty() || self.dashboard_fetching.contains(&session_id) {
            return;
        }
        self.dashboard_fetching.insert(session_id.clone());
        self.spawn_api(move |api, tx| async move {
            let value = api
                .query(QueryRequest::SessionDashboard {
                    session_id: session_id.clone(),
                })
                .await;
            let parsed: Result<qaqh_client::DomainDashboardSnapshot, String> = match value {
                Ok(v) => {
                    // session.dashboard 返回 {tasks: [{id,subject,status…}], recent_edits: […]}；
                    // DashboardSnapshot 额外含 session_id/documents/current_todo_id。
                    let tasks = if let Some(arr) = v.get("tasks").and_then(|x| x.as_array()) {
                        arr.iter()
                            .filter_map(|item| {
                                Some(qaqh_client::DashboardTask {
                                    id: item.get("id")?.as_str()?.to_owned(),
                                    subject: item.get("subject")?.as_str().unwrap_or("").to_owned(),
                                    description: item
                                        .get("description")?
                                        .as_str()
                                        .unwrap_or("")
                                        .to_owned(),
                                    status: item
                                        .get("status")?
                                        .as_str()
                                        .unwrap_or("idle")
                                        .to_owned(),
                                    evidence: item
                                        .get("evidence")
                                        .and_then(|e| e.as_str())
                                        .map(str::to_owned),
                                })
                            })
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    let recent_edits = v
                        .get("recent_edits")
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| s.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let session_id_out = v
                        .get("session_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or(&session_id)
                        .to_owned();
                    Ok(qaqh_client::DomainDashboardSnapshot {
                        session_id: session_id_out,
                        documents: Vec::new(),
                        recent_edits,
                        tasks,
                        current_todo_id: v
                            .get("current_todo_id")
                            .and_then(|x| x.as_str())
                            .map(str::to_owned),
                    })
                }
                Err(e) => {
                    let msg = e.to_string();
                    // fallback: todo.status 是同一数据源的另一视图
                    let v2 = api
                        .query(QueryRequest::TodoStatus {
                            session_id: session_id.clone(),
                        })
                        .await;
                    match v2 {
                        Ok(v) => {
                            let items = v
                                .get("items")
                                .and_then(|x| x.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let tasks = items
                                .iter()
                                .filter_map(|item| {
                                    Some(qaqh_client::DashboardTask {
                                        id: item.get("id")?.as_str()?.to_owned(),
                                        subject: item
                                            .get("title")
                                            .or(item.get("subject"))?
                                            .as_str()?
                                            .to_owned(),
                                        description: item
                                            .get("description")?
                                            .as_str()
                                            .unwrap_or("")
                                            .to_owned(),
                                        status: item
                                            .get("status")?
                                            .as_str()
                                            .unwrap_or("idle")
                                            .to_owned(),
                                        evidence: item
                                            .get("evidence")
                                            .and_then(|e| e.as_str())
                                            .map(str::to_owned),
                                    })
                                })
                                .collect::<Vec<_>>();
                            if tasks.is_empty() {
                                Err(msg)
                            } else {
                                Ok(qaqh_client::DomainDashboardSnapshot {
                                    session_id: session_id.clone(),
                                    documents: Vec::new(),
                                    recent_edits: Vec::new(),
                                    tasks,
                                    current_todo_id: v
                                        .get("current_id")
                                        .and_then(|x| x.as_str())
                                        .map(str::to_owned),
                                })
                            }
                        }
                        Err(e2) => Err(format!("{msg}; todo.status: {e2}")),
                    }
                }
            };
            let _ = tx.send(AppMsg::Action(ActionResult::Dashboard {
                session_id,
                result: parsed,
            }));
        });
    }
}

/// 被拒 ack 的本地效果：**立即**撤销对应的 pending create，并给出失败文案。
///
/// 为什么必须撤销：`Rejected` 是**终态拒绝**（`qaqh-ringing/src/envelope.rs:199`
/// 的 ack 语义：accepted 才进入 actor，业务完成另经 `causation_id == command_id`
/// 的可靠事件返回）。被拒的 `SessionCreate` 因此**永远等不到**
/// `SessionStateEvent::Created`，`pending_creates` 里那条只能等 `handle_tick`
/// 的 15s `retain` 过期——这 15s 里状态栏一直显示 `· creating…`
/// 用户以为还在创建，实际早已失败。
///
/// 判据是**精确关联**，不是猜：`ack.command_id` 就是本侧为 `SessionCreate`
/// 生成并透传的那个 id（见 `App::new_session_with_cwd`），且 `qaqh-client` 的
/// `send_command` 会校验 ack 的 `command_id` 与提交时一致
/// （`qaqh-client/src/client.rs:381`），不符即返回 `Err`。故无需按 `session_id`
/// 或 label 反查——create 的 `session_id` 恒为 `None`，label 也不是唯一键。
///
/// 注意 `Err` 分支**不适用**本函数：传输失败（超时/HTTP 非 2xx）是**结果未知**，
/// 命令可能已在后端执行，晚到的 `Created` 仍会经 `causation_id` 回来；提前撤销
/// 反而会丢掉那次自动开标签页。故只有终态拒绝才撤销，`Err` 仍留给 15s 兜底。
///
/// # 测试分层（PR #18 二轮复审阻断项，已闭环）
///
/// 两层都要有，缺一层就漏：
///
/// 1. **本函数**：下方 `mod tests` 锁「拒绝 → 撤销 + 文案」。
/// 2. **调用点**：`mod.rs` 的 `tests::rejected_create_ack_from_handler_clears_pending_create`
///    打穿 `App::handle` → `handle_action`，锁状态栏 `· creating…` 的消失。
///    只测第 1 层时，把调用那一行换回旧行为（只 toast、不撤销）会**全绿**——
///    「全绿」掩盖主修复唯一生效的那层没有网。
///
/// 第 2 层依赖 `App::new_for_test` + `Runtime::stub_for_test`（桩 runtime 无
/// `Client`）；本仓该基建与 PR #17（`fix/subagent-lifecycle`）同形，合并时是
/// 普通文本冲突，取任一即可。
///
/// # `is_create`：提示文案的判据是「命令种类」，不是「撤销成功」
///
/// 「（会话未创建）」此前只在 `remove` 命中时追加（PR #18 二轮复审建议 1）：
/// ack 迟到 >15s 时 `handle_tick` 已清掉那条 pending，用户会看到「被拒绝」却
/// **没有**「未创建」——而会话确实没建成，提示反而缺失。现按**该 ack 是否属于
/// create 命令**判断，与本地 pending 是否还在无关。
///
/// 判据的权威来源是 wire 层不变式：`RingingCommandEnvelope::validate` 要求
/// **session_id 缺失时命令必须是 `SessionCreate`**（`qaqh-ringing/src/envelope.rs:177-186`，
/// 否则报 `missing_session_id`），而 `send_command` 发前必过 `validate`。故调用方用
/// `session_id.is_none()` 即可判定，无需猜 label。
pub(super) fn apply_rejected_ack(
    pending_creates: &mut HashMap<String, Instant>,
    label: &str,
    ack: &qaqh_client::ClientV2CommandAck,
    is_create: bool,
) -> String {
    let rejected = ack.status == qaqh_client::RingingCommandAckStatus::Rejected;
    if rejected {
        pending_creates.remove(&ack.command_id);
    }
    let detail = format!(
        "{} {}",
        ack.code.as_deref().unwrap_or_default(),
        ack.message.as_deref().unwrap_or_default()
    );
    let detail = detail.trim();
    let mut msg = if detail.is_empty() {
        format!("{label} 被拒绝")
    } else {
        format!("{label} 被拒绝: {detail}")
    };
    if rejected && is_create {
        // 与状态栏的 `creating…` 必须同时消失，否则用户仍不知道会话到底建没建。
        msg.push_str("（会话未创建）");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{ClientV2CommandAck, RingingCommandAckStatus};

    // 覆盖分层（见 `apply_rejected_ack` 的「测试分层」小节）：本模块只锁函数本身，
    // **调用点**由 `app::tests::rejected_create_ack_from_handler_clears_pending_create`
    // 打穿 `App::handle` 覆盖——两层缺一层就会漏（只测本模块时，把 `mod.rs` 里那行
    // 调用换回旧行为会全绿）。

    fn ack(command_id: &str, status: RingingCommandAckStatus) -> ClientV2CommandAck {
        ClientV2CommandAck {
            command_id: command_id.to_string(),
            status,
            code: Some("rate_limited".into()),
            message: Some("too many sessions".into()),
            retry_after_ms: Some(500),
            existing: None,
        }
    }

    fn pending_with(command_id: &str) -> HashMap<String, Instant> {
        let mut pending = HashMap::new();
        pending.insert(command_id.to_string(), Instant::now());
        pending
    }

    /// CNB issue #4 缺陷 1：被拒的 `SessionCreate` 必须**立即**撤销 pending create，
    /// 否则状态栏的 `· creating…` 会一直挂到 `handle_tick` 的 15s 过期。
    ///
    /// 变异验证（实测）：把 `apply_rejected_ack` 里的 `pending_creates.remove(...)`
    /// 换成不删（= 旧行为「只 toast」）→ 本测试红。
    #[test]
    fn rejected_create_ack_clears_pending_create_immediately() {
        let mut pending = pending_with("cmd-create-1");
        let msg = apply_rejected_ack(
            &mut pending,
            "新会话",
            &ack("cmd-create-1", RingingCommandAckStatus::Rejected),
            true,
        );
        assert!(
            pending.is_empty(),
            "被拒的 create 不得滞留（旧行为下 15s 内状态栏一直 creating…），实测残留 {:?}",
            pending.keys().collect::<Vec<_>>()
        );
        assert!(
            msg.contains("未创建"),
            "提示必须说清会话没建成，实测：{msg}"
        );
        assert!(
            msg.contains("rate_limited") && msg.contains("too many sessions"),
            "后端给的 code/message 不得丢，实测：{msg}"
        );
    }

    /// 反向闸 1：`Accepted` 不得撤销 pending——命令已进入 actor，`Created` 事件
    /// 还会经 `causation_id` 回来；提前撤销会让新建的标签页永不自动打开。
    #[test]
    fn accepted_ack_keeps_pending_create() {
        let mut pending = pending_with("cmd-create-2");
        let msg = apply_rejected_ack(
            &mut pending,
            "新会话",
            &ack("cmd-create-2", RingingCommandAckStatus::Accepted),
            true,
        );
        assert_eq!(
            pending.len(),
            1,
            "Accepted 不是失败，不得撤销 pending create"
        );
        assert!(
            !msg.contains("未创建"),
            "Accepted 不该产出失败文案，实测：{msg}"
        );
    }

    /// 反向闸 2：别的命令被拒不得误伤新建会话的 pending（command_id 是精确键）。
    #[test]
    fn rejected_ack_for_other_command_leaves_pending_create() {
        let mut pending = pending_with("cmd-create-3");
        let msg = apply_rejected_ack(
            &mut pending,
            "撤销回合",
            &ack("cmd-other-9", RingingCommandAckStatus::Rejected),
            false,
        );
        assert_eq!(pending.len(), 1, "无关命令的拒绝不得撤销 pending create");
        assert!(msg.contains("撤销回合"), "文案仍应归属该命令，实测：{msg}");
    }

    /// PR #18 二轮复审建议 1（迟到 ack 缺口）：`（会话未创建）` 的判据是「该 ack
    /// 是否属于 create 命令」，**不是**「撤销是否命中」。ack 迟到 >15s 时 pending
    /// 已被 `handle_tick` 清掉——此时会话确实没建成，提示不能反而缺失。
    ///
    /// 变异验证（实测）：把判据改回 `rejected && removed`（旧行为）→ 本测试红。
    #[test]
    fn late_rejected_create_ack_still_says_not_created() {
        // pending 已被 15s retain 清掉：迟到 ack 命中不了任何条目。
        let mut pending: HashMap<String, Instant> = HashMap::new();
        let msg = apply_rejected_ack(
            &mut pending,
            "新会话",
            &ack("cmd-create-late", RingingCommandAckStatus::Rejected),
            true,
        );
        assert!(
            msg.contains("未创建"),
            "迟到 ack 下会话同样没建成，提示不得缺失，实测：{msg}"
        );
    }

    fn install_team(app: &mut App, root: &str, snapshot: qaqh_client::ClientV2TeamSnapshot) {
        app.teams
            .entry(root.into())
            .or_default()
            .replace_from_snapshot(snapshot);
    }

    fn nested_snapshot() -> qaqh_client::ClientV2TeamSnapshot {
        serde_json::from_value(serde_json::json!({
            "root_session_id": "root",
            "agents": [
                {
                    "agent_id": "root",
                    "agent_path": "/root",
                    "status": "running",
                    "residency": "loaded"
                },
                {
                    "agent_id": "parent",
                    "agent_path": "/root/parent",
                    "status": "running",
                    "residency": "loaded",
                    "parent_agent_path": "/root"
                },
                {
                    "agent_id": "sub",
                    "agent_path": "/root/parent/sub",
                    "status": "running",
                    "residency": "loaded",
                    "parent_agent_path": "/root/parent"
                },
                {
                    "agent_id": "grand",
                    "agent_path": "/root/parent/sub/grand",
                    "status": "running",
                    "residency": "loaded",
                    "parent_agent_path": "/root/parent/sub"
                }
            ],
            "unread_messages": [],
            "revision": 1,
            "last_fact_seq": 1
        }))
        .expect("team snapshot")
    }

    /// issue #2 缺陷 3：父 session_id **不在**本地 tabs（daemon 主动关父 / 父本身是
    /// 子代理）时，也必须按 Team projection 的 parent_agent_path 回收后代。
    #[test]
    fn close_tab_by_session_id_reclaims_children_of_non_tab_parent() {
        let (mut app, _rx) = App::new_for_test();

        for session_id in ["root", "parent", "sub", "grand"] {
            app.sessions
                .insert(session_id.into(), SessionState::new(session_id.into()));
        }
        install_team(&mut app, "root", nested_snapshot());
        for session_id in ["sub", "grand"] {
            app.subagent_session_ids.insert(session_id.into());
        }
        assert!(
            !app.tabs.contains(&"parent".to_string()),
            "前提：父不是本地标签"
        );

        app.close_tab_by_session_id("parent");

        assert!(
            !app.subagent_session_ids.contains("sub")
                && !app.subagent_session_ids.contains("grand"),
            "父不在 tabs 时子代理/孙代同样必须停止 timeline 跟踪"
        );
        assert!(
            !app.sessions.contains_key("sub") && !app.sessions.contains_key("grand"),
            "子代理/孙代的本地快照随父一起回收"
        );
        assert!(
            app.tracked_session_ids.is_empty(),
            "跟踪集必须只剩真正打开的标签（此处没有标签）"
        );
        assert!(
            app.teams["root"].agent_by_id("grand").is_some(),
            "roster 条目不能被 live 回收删掉"
        );
    }

    /// 回归护栏：父在 tabs 时行为不变（回收子代理 + 关标签）。
    #[test]
    fn close_tab_by_session_id_still_closes_tab_and_children() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("parent".into());
        app.sessions
            .insert("parent".into(), SessionState::new("parent".into()));
        app.sessions
            .insert("sub".into(), SessionState::new("sub".into()));
        install_team(
            &mut app,
            "parent",
            serde_json::from_value(serde_json::json!({
                "root_session_id": "parent",
                "agents": [
                    {
                        "agent_id": "parent",
                        "agent_path": "/root",
                        "status": "running",
                        "residency": "loaded"
                    },
                    {
                        "agent_id": "sub",
                        "agent_path": "/root/sub",
                        "status": "running",
                        "residency": "loaded",
                        "parent_agent_path": "/root"
                    }
                ],
                "unread_messages": [],
                "revision": 1,
                "last_fact_seq": 1
            }))
            .expect("team snapshot"),
        );
        app.subagent_session_ids.insert("sub".into());

        app.close_tab_by_session_id("parent");

        assert!(app.tabs.is_empty());
        assert!(!app.sessions.contains_key("parent"));
        assert!(!app.teams.contains_key("parent"));
        assert!(!app.subagent_session_ids.contains("sub"));
        assert!(!app.sessions.contains_key("sub"));
    }
}
