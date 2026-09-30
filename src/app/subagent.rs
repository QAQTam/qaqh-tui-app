//! 子代理观测导航与 timeline 跟踪。
//!
//! roster 身份完全来自 `TeamSnapshot/TeamDelta`；本模块只负责：
//! - 对 loaded 子代理建立只读 timeline 订阅；
//! - 在父/子 agent 之间移动观测焦点；
//! - 终态/卸载后停止 live 订阅，但保留 roster 条目与本地 transcript 快照。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

impl App {
    // ── 视图栈 ──

    /// 当前正在查看的会话 session_id：inspect 优先，否则活动标签。
    pub fn view_session_id(&self) -> Option<String> {
        self.inspect.clone().or_else(|| self.active_session_id())
    }

    pub fn inspecting(&self) -> bool {
        self.inspect.is_some()
    }

    pub(super) fn exit_inspect(&mut self) {
        self.inspect = None;
    }

    // ── 跟踪 ──

    /// 确保子代理 `SessionState` 存在并进入 timeline 跟踪集。
    ///
    /// `parent` 只用于防止把会话自己当成自己的子代理；身份不来自该参数。
    pub(super) fn ensure_subagent_tracked(&mut self, parent: &str, session_id: &str) {
        if session_id.is_empty() || parent == session_id {
            return;
        }
        if !self.sessions.contains_key(session_id) {
            self.sessions.insert(
                session_id.to_owned(),
                SessionState::new(session_id.to_owned()),
            );
        }
        if self.subagent_session_ids.insert(session_id.to_owned()) {
            self.sync_tracked();
            self.attach_subagent_session_id(session_id.to_owned());
        }
    }

    /// `SessionAttach`（无 actor 副作用）→ bootstrap。timeline 流由 runtime
    /// 在 session_id 进入跟踪集后自动建立。
    fn attach_subagent_session_id(&mut self, session_id: String) {
        self.spawn_api(move |api, tx| async move {
            let attach = api
                .send_command(
                    Some(&session_id),
                    RingingCommand::Control(ControlCommand::SessionAttach {
                        session_id: session_id.clone(),
                    }),
                    Default::default(),
                )
                .await;
            if let Err(e) = attach {
                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                    session_id: Some(session_id.clone()),
                    label: "attach",
                    result: Err(e),
                }));
                return;
            }
            let result = api.bootstrap(&session_id).await;
            let client_session_id = api.v2_client_session_id().await;
            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap {
                session_id,
                result,
                client_session_id,
            }));
        });
    }

    /// 终态/卸载：停止 live timeline，但保留本地快照与 roster 条目。
    pub(super) fn untrack_subagent(&mut self, session_id: &str) {
        if self.subagent_session_ids.remove(session_id) {
            self.sync_tracked();
        }
    }

    // ── 按键导航 ──

    /// 子代理观测导航：Ctrl+↑ 深入/在同级间循环、Ctrl+↓ 逐层返回、Esc 返回。
    pub(super) fn subagent_nav_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if ctrl => {
                let Some(base) = self.view_session_id() else {
                    return false;
                };
                if self.child_agent_ids(&base).is_empty() {
                    return false;
                }
                self.cycle_subagent();
                true
            }
            KeyCode::Down if ctrl => {
                if let Some(cur) = self.inspect.clone() {
                    self.inspect = self.subagent_parent(&cur);
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

    /// 在活动 agent 的子代理间循环切换：未观测时取最后一个 running，
    /// 否则取最后一个 child；已观测时顺序前进并回绕。
    fn cycle_subagent(&mut self) {
        let Some(base) = self.view_session_id() else {
            return;
        };
        let viewable = self.child_agent_ids(&base);
        if viewable.is_empty() {
            return;
        }
        let next = match &self.inspect {
            Some(cur) => {
                let idx = viewable
                    .iter()
                    .position(|session_id| session_id == cur)
                    .map(|index| (index + 1) % viewable.len())
                    .unwrap_or(0);
                viewable[idx].clone()
            }
            None => {
                let running = viewable.iter().rev().find(|agent_id| {
                    self.team_for_agent(agent_id)
                        .and_then(|(_, team)| team.agent_by_id(agent_id))
                        .is_some_and(|agent| {
                            agent.status == qaqh_client::ClientV2TeamAgentStatus::Running
                        })
                });
                running
                    .cloned()
                    .unwrap_or_else(|| viewable.last().unwrap().clone())
            }
        };
        self.inspect = Some(next);
    }

    /// 查找某子代理 session_id 的直属父会话 session_id（Ctrl+↓ 逐层上溯用）。
    /// 直属父是标签会话 → None（回到标签视图）；直属父也是子代理 → 返回父 session_id。
    pub(crate) fn subagent_parent(&self, sub_session_id: &str) -> Option<String> {
        let parent = self
            .team_for_agent(sub_session_id)
            .and_then(|(_, team)| team.parent_agent_id(sub_session_id))?;
        (!self.tabs.contains(&parent)).then_some(parent)
    }

    /// 观测态按键：仅滚动/翻页作用于子代理视图。
    pub(super) fn inspect_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::PageUp => self.scroll_up(20),
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

    fn app_with_team() -> App {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("root".into());
        app.sessions
            .insert("root".into(), SessionState::new("root".into()));
        let snapshot: qaqh_client::ClientV2TeamSnapshot =
            serde_json::from_value(serde_json::json!({
                "root_session_id": "root",
                "agents": [
                    {
                        "agent_id": "root",
                        "agent_path": "/root",
                        "role": "root",
                        "status": "running",
                        "residency": "loaded"
                    },
                    {
                        "agent_id": "child",
                        "agent_path": "/root/child",
                        "nickname": "child",
                        "status": "running",
                        "residency": "loaded",
                        "parent_agent_path": "/root"
                    },
                    {
                        "agent_id": "grand",
                        "agent_path": "/root/child/grand",
                        "status": "completed",
                        "residency": "unloaded",
                        "parent_agent_path": "/root/child"
                    }
                ],
                "unread_messages": [],
                "revision": 1,
                "last_fact_seq": 1
            }))
            .expect("team snapshot");
        app.teams
            .entry("root".into())
            .or_default()
            .replace_from_snapshot(snapshot);
        app
    }

    #[test]
    fn navigation_uses_agent_path_parent_links() {
        let app = app_with_team();
        assert_eq!(app.child_agent_ids("root"), vec!["child".to_string()]);
        assert_eq!(app.child_agent_ids("child"), vec!["grand".to_string()]);
        assert_eq!(app.subagent_parent("child"), None);
        assert_eq!(app.subagent_parent("grand"), Some("child".to_string()));
    }

    #[tokio::test]
    async fn unloaded_child_does_not_create_a_live_subscription() {
        let mut app = app_with_team();
        app.reconcile_team_tracking("root");
        assert!(app.subagent_session_ids.contains("child"));
        assert!(!app.subagent_session_ids.contains("grand"));
    }
}
