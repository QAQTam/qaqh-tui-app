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

/// 被拒 ack 的本地效果：**立即**撤销对应的 pending create，并给出失败文案。
///
/// 为什么必须撤销：`Rejected` 是**终态拒绝**（`qaqh-ringing/src/envelope.rs:199`
/// 的 ack 语义：accepted 才进入 actor，业务完成另经 `causation_id == command_id`
/// 的可靠事件返回）。被拒的 `SessionCreate` 因此**永远等不到**
/// `SessionStateEvent::Created`，`pending_creates` 里那条只能等 `handle_tick`
/// 的 15s `retain` 过期——这 15s 里状态栏一直显示 `· creating…`
/// （`ui/status_bar.rs:40`），用户以为还在创建，实际早已失败。
///
/// 判据是**精确关联**，不是猜：`ack.command_id` 就是本侧为 `SessionCreate`
/// 生成并透传的那个 id（见 `App::new_session_with_cwd`），且 `qaqh-client` 的
/// `send_command` 会校验 ack 的 `command_id` 与提交时一致
/// （`qaqh-client/src/client.rs:381`），不符即返回 `Err`。故无需按 `seed`
/// 或 label 反查——create 的 `seed` 恒为 `None`，label 也不是唯一键。
///
/// 注意 `Err` 分支**不适用**本函数：传输失败（超时/HTTP 非 2xx）是**结果未知**，
/// 命令可能已在后端执行，晚到的 `Created` 仍会经 `causation_id` 回来；提前撤销
/// 反而会丢掉那次自动开标签页。故只有终态拒绝才撤销，`Err` 仍留给 15s 兜底。
pub(super) fn apply_rejected_ack(
    pending_creates: &mut HashMap<String, Instant>,
    label: &str,
    ack: &qaqh_client::RingingCommandAck,
) -> String {
    let aborted_create = if ack.status == qaqh_client::RingingCommandAckStatus::Rejected {
        pending_creates.remove(&ack.command_id).is_some()
    } else {
        false
    };
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
    if aborted_create {
        // 与状态栏的 `creating…` 必须同时消失，否则用户仍不知道会话到底建没建。
        msg.push_str("（会话未创建）");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{RingingCommandAck, RingingCommandAckStatus};

    fn ack(command_id: &str, status: RingingCommandAckStatus) -> RingingCommandAck {
        RingingCommandAck {
            command_id: command_id.to_string(),
            status,
            code: Some("rate_limited".into()),
            message: Some("too many sessions".into()),
            retry_after_ms: Some(500),
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
        );
        assert_eq!(pending.len(), 1, "无关命令的拒绝不得撤销 pending create");
        assert!(msg.contains("撤销回合"), "文案仍应归属该命令，实测：{msg}");
    }
}
