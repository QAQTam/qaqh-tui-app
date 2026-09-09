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
        self.spawn_api(move |client, tx| async move {
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
        self.active_session()
            .and_then(|s| s.meta.as_ref().and_then(|m| m.cwd.clone()))
    }

    pub fn new_session_with_cwd(&mut self, cwd: Option<String>) {
        let cwd = self.effective_cwd(cwd);
        let client = self.client.clone();
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
        self.pending_creates
            .insert(command_id.clone(), Instant::now());
        self.spawn_api(move |client, tx| async move {
            let result = client.command(&cmd).await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: None,
                label: "新会话",
                result,
            }));
        });
    }

    pub(super) fn close_tab_by_seed(&mut self, seed: &str) {
        if let Some(pos) = self.tabs.iter().position(|s| s == seed) {
            // 同标签拉起的子代理：停止跟踪并移除本地快照（标签都没了，
            // 子代理视图无宿主；daemon 侧 ephemeral 会话自会回收）。
            let sub_seeds: Vec<String> = self
                .sessions
                .get(seed)
                .map(|s| s.subagents.iter().filter_map(|e| e.seed.clone()).collect())
                .unwrap_or_default();
            for sub in sub_seeds {
                self.untrack_subagent(&sub);
                self.sessions.remove(&sub);
                if self.inspect.as_deref() == Some(sub.as_str()) {
                    self.inspect = None;
                }
            }
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
        self.spawn_api(move |client, tx| async move {
            let list = client.session_list().await.map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::SessionList(list)));
            let activity = client
                .service(methods::SESSION_ACTIVITY, &serde_json::json!({}))
                .await
                .map_err(|e| e.to_string());
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
        self.spawn_api(move |client, tx| async move {
            let value = client
                .service(
                    methods::SESSION_DASHBOARD,
                    &serde_json::json!({ "seed": seed.clone() }),
                )
                .await;
            let parsed: Result<crate::protocol::event::DashboardSnapshot, String> = match value {
                Ok(v) => {
                    // session.dashboard 返回 {tasks: [{id,subject,status…}], recent_edits: […]}；
                    // DashboardSnapshot 额外含 seed/documents/current_todo_id。
                    let tasks = if let Some(arr) = v.get("tasks").and_then(|x| x.as_array()) {
                        arr.iter()
                            .filter_map(|item| {
                                Some(crate::protocol::event::DashboardTask {
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
                    Ok(crate::protocol::event::DashboardSnapshot {
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
                    let v2 = client
                        .service(
                            methods::TODO_STATUS,
                            &serde_json::json!({ "seed": seed.clone() }),
                        )
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
                                    Some(crate::protocol::event::DashboardTask {
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
                                Ok(crate::protocol::event::DashboardSnapshot {
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
                s.rendered = None;
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
                s.rendered = None;
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
