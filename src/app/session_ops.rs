//! 会话/标签生命周期与 LRU 焦点管理（自 app/mod.rs 按域拆分，行为不变）。

use super::*;

impl App {
    pub fn open_session_tab(&mut self, seed: &str) {
        if self.tabs.iter().any(|s| s == seed) {
            self.active = self.tabs.iter().position(|s| s == seed).unwrap_or(0);
            return;
        }
        self.tabs.push(seed.to_owned());
        self.sessions
            .insert(seed.to_owned(), SessionState::new(seed.to_owned()));
        self.active = self.tabs.len() - 1;
        self.sync_tracked();
        // attach + bootstrap（timeline 流由 runtime 自动建立）。
        self.attach_and_bootstrap(seed.to_owned());
    }

    pub(super) fn attach_and_bootstrap(&mut self, seed: String) {
        self.spawn_api(move |api, tx| async move {
            let ack = api
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(ControlCommand::SessionResume { seed: seed.clone() }),
                    Default::default(),
                )
                .await;
            if let Err(e) = ack {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    seed: Some(seed.clone()),
                    label: "resume",
                    result: Err(e),
                }));
                return;
            }
            let result = api.bootstrap(&seed).await;
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap { seed, result }));
        });
    }

    pub fn new_session(&mut self) {
        self.new_session_with_cwd(None);
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
                seed: None,
                label: "新会话",
                result,
            }));
        });
    }

    pub(super) fn close_tab_by_seed(&mut self, seed: &str) {
        // 子代理回收**不依赖** seed 是否在本地 `tabs`：daemon 主动关父会话、
        // 或父本身就是子代理时，父 seed 从来不在 tabs 里——旧实现把整段回收罩在
        // `tabs` 命中内，这些子代理的 timeline 流与本地快照就永远留在跟踪集里。
        self.reclaim_subagents(seed);
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
            return;
        }
        // 不在 tabs：没有标签栈可调。会话确实已消失 → 停止跟踪并把（可能挂在
        // 别人名下的）条目收口为 `Closed`（权威终态标签仍可覆盖它）；本地快照
        // **保留**——终态子代理仍要能被查看。
        self.mark_subagent_closed(seed);
        self.tracked_seeds.remove(seed);
        self.focus_order.retain(|s| s != seed);
        if self.last_focused.as_deref() == Some(seed) {
            self.last_focused = None;
        }
        self.sync_tracked();
    }

    /// 按父子关系回收 `seed` 名下的全部子代理（含多层嵌套）：停止 timeline
    /// 跟踪并移除本地快照（子代理视图无宿主；daemon 侧 ephemeral 会话自会回收）。
    ///
    /// 先收集整个后代集合再统一删除——边删边找会把孙代一起弄丢（父条目随
    /// 快照删除后，父子关系就无从查起）。
    fn reclaim_subagents(&mut self, seed: &str) {
        let mut descendants: Vec<String> = Vec::new();
        let mut frontier = vec![seed.to_owned()];
        while let Some(parent) = frontier.pop() {
            let children: Vec<String> = self
                .sessions
                .get(&parent)
                .map(|s| s.subagents.iter().filter_map(|e| e.seed.clone()).collect())
                .unwrap_or_default();
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

    pub fn active_seed(&self) -> Option<String> {
        self.tabs.get(self.active).cloned()
    }

    pub fn active_session(&self) -> Option<&SessionState> {
        self.tabs
            .get(self.active)
            .and_then(|s| self.sessions.get(s))
    }

    pub(super) fn active_session_mut(&mut self) -> Option<&mut SessionState> {
        let seed = self.tabs.get(self.active)?.clone();
        self.sessions.get_mut(&seed)
    }

    // ───────────────────────── 命令发送 ─────────────────────────

    pub fn fetch_session_list(&mut self) {
        self.spawn_api(move |api, tx| async move {
            // session.list 回数组，逐项解析为**权威类型**（G2）。
            // 仍逐项宽松：单条形状不符只跳过该条，不让整个列表失败——
            // 与 G1 同款「解析失败一律降级而不是崩」的契约。
            let list = api
                .client
                .query(QueryRequest::SessionList)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
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
                .client
                .query(QueryRequest::SessionActivity)
                .await
                .map_err(|e| e.to_string())
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

    pub fn archive_session(&mut self, seed: String) {
        self.send_control_command(
            seed.clone(),
            ControlCommand::SessionArchive { seed },
            "归档",
        );
    }

    pub fn unarchive_session(&mut self, seed: String) {
        self.send_control_command(
            seed.clone(),
            ControlCommand::SessionUnarchive { seed },
            "取消归档",
        );
    }

    pub fn delete_session(&mut self, seed: String) {
        self.send_control_command(seed.clone(), ControlCommand::SessionDelete { seed }, "删除");
    }

    pub(super) fn fetch_dashboard(&mut self, seed: String) {
        if seed.is_empty() || self.dashboard_fetching.contains(&seed) {
            return;
        }
        self.dashboard_fetching.insert(seed.clone());
        self.spawn_api(move |api, tx| async move {
            let value = api
                .client
                .query(QueryRequest::SessionDashboard { seed: seed.clone() })
                .await;
            let parsed: Result<qaqh_client::DomainDashboardSnapshot, String> = match value {
                Ok(v) => {
                    // session.dashboard 返回 {tasks: [{id,subject,status…}], recent_edits: […]}；
                    // DashboardSnapshot 额外含 seed/documents/current_todo_id。
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
                    let seed_out = v
                        .get("seed")
                        .and_then(|x| x.as_str())
                        .unwrap_or(&seed)
                        .to_owned();
                    Ok(qaqh_client::DomainDashboardSnapshot {
                        seed: seed_out,
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
                        .client
                        .query(QueryRequest::TodoStatus { seed: seed.clone() })
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
                                    seed: seed.clone(),
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
                seed,
                result: parsed,
            }));
        });
    }

    pub(super) fn click_tab(&mut self, column: u16) {
        // 与 ui::tab_bar 的布局约定一致：品牌段 10 列，其后每 tab 占
        // " [n] title " 的宽度；仅处理前 9 个。
        let mut col: u16 = 10;
        for (idx, seed) in self.tabs.iter().enumerate().take(9) {
            let title = self
                .sessions
                .get(seed)
                .map(|s| s.title())
                .unwrap_or_default();
            let label_w = format!(" {} {} ", idx + 1, truncate_str(&title, 18))
                .chars()
                .count() as u16;
            if column >= col && column < col + label_w {
                self.active = idx;
                // 切标签即退出子代理观测（观测作用域属于原标签）。
                if self.inspecting() {
                    self.exit_inspect();
                }
                return;
            }
            col += label_w;
        }
    }
    /// 焦点切换的内存回收（会话隔离的最后一环）：
    /// 1) 非 active 标签全部丢弃渲染缓存（聚焦时按需重建）；
    /// 2) 超出 LRU 窗口的标签丢弃 timeline 模型（轻状态/挂起交互/用量保留），
    ///    标记 needs_rebaseline；
    /// 3) 回到被逐出的标签时自动 re-baseline（服务端是权威历史）。
    pub(super) fn touch_focus(&mut self, active: &str) {
        // 活动标签与被观测的子代理同等保护（实时视图不能被 LRU 逐出）。
        let mut focus_seeds: Vec<String> = vec![active.to_owned()];
        if let Some(inspect) = self.inspect.clone()
            && self.sessions.contains_key(&inspect)
            && inspect != active
        {
            focus_seeds.push(inspect);
        }
        for seed in &focus_seeds {
            self.focus_order.retain(|s| s != seed);
        }
        for seed in focus_seeds.iter().rev() {
            self.focus_order.insert(0, seed.clone());
        }

        for (seed, s) in self.sessions.iter_mut() {
            if seed != active {
                s.segments = None;
            }
        }

        let keep: HashSet<String> = self
            .focus_order
            .iter()
            .take(ACTIVE_MODELS)
            .cloned()
            .collect();
        for (seed, s) in self.sessions.iter_mut() {
            if !keep.contains(seed) && s.ready && !s.needs_rebaseline {
                s.timeline = timeline_model::TimelineModel::default();
                s.segments = None;
                s.ready = false;
                s.needs_rebaseline = true;
                s.scroll.follow = true;
                s.scroll.offset = 0;
            }
        }

        if self
            .sessions
            .get(active)
            .is_some_and(|s| s.needs_rebaseline && !s.loading_older)
        {
            self.request_rebaseline(active);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::subagent::{SubagentEntry, SubagentState};

    fn entry(tool_call_id: &str, seed: &str) -> SubagentEntry {
        SubagentEntry {
            tool_call_id: tool_call_id.into(),
            seed: Some(seed.into()),
            name: "explore".into(),
            state: SubagentState::Running,
        }
    }

    /// issue #2 缺陷 3：父 seed **不在**本地 tabs（daemon 主动关父 / 父本身是
    /// 子代理）时，也必须按父子关系回收它的子代理（含多层嵌套）。
    /// 旧实现把整段回收罩在 `tabs` 命中内 → 零动作，流与快照永久滞留。
    #[test]
    fn close_tab_by_seed_reclaims_children_of_non_tab_parent() {
        let mut app = App::new_for_test();

        let mut parent = SessionState::new("parent".into());
        parent.subagents.push(entry("c1", "sub"));
        app.sessions.insert("parent".into(), parent);

        // 子代理自己又派生了孙代。
        let mut sub = SessionState::new("sub".into());
        sub.subagents.push(entry("c2", "grand"));
        app.sessions.insert("sub".into(), sub);

        app.sessions
            .insert("grand".into(), SessionState::new("grand".into()));
        for s in ["sub", "grand"] {
            app.subagent_seeds.insert(s.into());
        }
        assert!(
            !app.tabs.contains(&"parent".to_string()),
            "前提：父不是本地标签"
        );

        app.close_tab_by_seed("parent");

        assert!(
            !app.subagent_seeds.contains("sub") && !app.subagent_seeds.contains("grand"),
            "父不在 tabs 时子代理/孙代同样必须停止 timeline 跟踪"
        );
        assert!(
            !app.sessions.contains_key("sub") && !app.sessions.contains_key("grand"),
            "子代理/孙代的本地快照随父一起回收"
        );
        assert!(
            app.tracked_seeds.is_empty(),
            "跟踪集必须只剩真正打开的标签（此处没有标签）"
        );
    }

    /// 回归护栏：父在 tabs 时行为不变（回收子代理 + 关标签）。
    #[test]
    fn close_tab_by_seed_still_closes_tab_and_children() {
        let mut app = App::new_for_test();
        app.tabs.push("parent".into());
        let mut parent = SessionState::new("parent".into());
        parent.subagents.push(entry("c1", "sub"));
        app.sessions.insert("parent".into(), parent);
        app.sessions
            .insert("sub".into(), SessionState::new("sub".into()));
        app.subagent_seeds.insert("sub".into());

        app.close_tab_by_seed("parent");

        assert!(app.tabs.is_empty());
        assert!(!app.sessions.contains_key("parent"));
        assert!(!app.subagent_seeds.contains("sub"));
        assert!(!app.sessions.contains_key("sub"));
    }
}
