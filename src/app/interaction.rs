//! 挂起交互：permission / ask / plan 的按键与应答（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    pub fn submit_ask(&mut self) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let Some(panel) = self
            .sessions
            .get_mut(&seed)
            .and_then(|s| s.pending_ask.as_ref())
        else {
            return;
        };
        let missing = panel.first_unanswered();
        let answers = match panel.collect_answers() {
            Ok(a) => a,
            Err(e) => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && let Some(p) = sess.pending_ask.as_mut()
                {
                    if let Some(idx) = missing {
                        p.focus = idx;
                        p.scroll = 0;
                    }
                    p.error = Some(e);
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
            .map(|(question_id, answer)| qaqh_client::AskAnswer {
                question_id,
                answer,
            })
            .collect();
        self.send_control_command(
            seed,
            ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            },
            "提交回答",
        );
    }

    pub fn dismiss_ask(&mut self) {
        let Some(seed) = self.active_seed() else {
            return;
        };
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
        self.send_control_command(
            seed,
            ControlCommand::InteractionAskDismiss { interaction_id },
            "跳过 ask",
        );
    }

    pub fn respond_plan(&mut self, approved: bool, autonomous: bool) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let panel = self
            .sessions
            .get(&seed)
            .and_then(|s| s.pending_plan.as_ref());
        let Some(panel) = panel else { return };
        let interaction_id = panel.interaction_id.clone();
        let message = if approved {
            None
        } else {
            let m = panel.message.trim().to_owned();
            (!m.is_empty()).then_some(m)
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.pending_plan = None;
        }
        self.send_control_command(
            seed,
            ControlCommand::PlanReviewRespond {
                interaction_id,
                approved,
                message,
                autonomous,
            },
            "plan review",
        );
    }

    pub fn respond_permission(&mut self, approved: bool) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let Some(panel) = self
            .sessions
            .get(&seed)
            .and_then(|s| s.active_permission().cloned())
        else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            // 下架 + 记入已解决：应答之后补投的同 id 权限请求不再入队（防幽灵面板）。
            sess.resolve_permission(&panel.tool_call_id);
        }
        let cmd = RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
            tool_call_id: panel.tool_call_id,
            approved,
            trust_folder: panel.trust_folder,
        });
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(Some(&seed.clone()), cmd, Default::default())
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "权限",
                result,
            }));
        });
    }

    /// 交互弹窗按键。返回 true = 已消费。优先级 permission > ask > plan。
    pub(super) fn modal_key(&mut self, key: KeyEvent) -> bool {
        let Some(seed) = self.active_seed() else {
            return false;
        };
        let route = {
            let Some(sess) = self.sessions.get(&seed) else {
                return false;
            };
            keymap::modal_route(
                sess.active_permission().is_some(),
                sess.pending_ask.is_some(),
                sess.pending_plan.is_some(),
            )
        };
        match route {
            Some(ModalRoute::Permission) => self.permission_key(&seed, key),
            Some(ModalRoute::Ask) => self.ask_key(&seed, key),
            Some(ModalRoute::Plan) => self.plan_key(&seed, key),
            None => false,
        }
    }

    pub(super) fn permission_key(&mut self, seed: &str, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyCode;
        enum D {
            Approve,
            Deny,
            ToggleTrust,
            None,
        }
        let decision = {
            let Some(sess) = self.sessions.get(seed) else {
                return true;
            };
            let Some(perm) = sess.active_permission() else {
                return true;
            };
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
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_permissions.first_mut()
                {
                    p.trust_folder = !p.trust_folder;
                }
            }
            D::None => {}
        }
        true
    }

    pub(super) fn ask_key(&mut self, seed: &str, key: KeyEvent) -> bool {
        use crate::app::session::option_index_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        enum D {
            Switch(i32),
            Cursor(i32),
            CursorFirst,
            CursorLast,
            Scroll(i32),
            Select {
                question: usize,
                option: usize,
                advance: bool,
            },
            Toggle {
                question: usize,
                option: usize,
            },
            StartEdit {
                question: usize,
            },
            EditChar(char),
            EditBackspace,
            EditCommit,
            EditCancel,
            Dismiss,
            None,
        }
        let decision = {
            let Some(sess) = self.sessions.get(seed) else {
                return true;
            };
            let Some(ask) = sess.pending_ask.as_ref() else {
                return true;
            };
            let focus = ask.focus.min(ask.questions.len().saturating_sub(1));
            let Some(question) = ask.questions.get(focus) else {
                return true;
            };
            let on_custom = ask.is_on_custom_row();
            if ask.editing_custom.is_some() {
                match key.code {
                    KeyCode::Enter => D::EditCommit,
                    KeyCode::Esc => D::EditCancel,
                    KeyCode::Backspace => D::EditBackspace,
                    KeyCode::Char(c) if !c.is_control() => D::EditChar(c),
                    _ => D::None,
                }
            } else {
                match key.code {
                    KeyCode::Left | KeyCode::BackTab => D::Switch(-1),
                    KeyCode::Right | KeyCode::Tab => D::Switch(1),
                    KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => D::Cursor(-1),
                    KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => D::Cursor(1),
                    KeyCode::Home | KeyCode::Char('g') if key.modifiers.is_empty() => {
                        D::CursorFirst
                    }
                    KeyCode::End => D::CursorLast,
                    KeyCode::Char('G') if key.modifiers == KeyModifiers::SHIFT => D::CursorLast,
                    KeyCode::PageUp => D::Scroll(-5),
                    KeyCode::PageDown => D::Scroll(5),
                    KeyCode::Char(' ') if on_custom => D::StartEdit { question: focus },
                    KeyCode::Char(' ') => D::Toggle {
                        question: focus,
                        option: ask.option_cursor(focus),
                    },
                    KeyCode::Enter if on_custom => D::StartEdit { question: focus },
                    KeyCode::Enter => D::Select {
                        question: focus,
                        option: ask.option_cursor(focus),
                        advance: true,
                    },
                    KeyCode::Char('z') if key.modifiers.is_empty() && question.allow_custom => {
                        D::StartEdit { question: focus }
                    }
                    KeyCode::Char('e')
                        if key.modifiers.is_empty()
                            && question.allow_custom
                            && option_index_for_key('e')
                                .is_none_or(|index| index >= question.options.len()) =>
                    {
                        D::StartEdit { question: focus }
                    }
                    KeyCode::Char(c)
                        if key.modifiers.is_empty() && option_index_for_key(c).is_some() =>
                    {
                        let option = option_index_for_key(c).unwrap_or_default();
                        if option < question.options.len() {
                            D::Select {
                                question: focus,
                                option,
                                advance: true,
                            }
                        } else {
                            D::None
                        }
                    }
                    KeyCode::Esc => D::Dismiss,
                    _ => D::None,
                }
            }
        };
        let mut submit_after = false;
        match decision {
            D::Switch(delta) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                    && !p.questions.is_empty()
                {
                    let last = p.questions.len().saturating_sub(1);
                    p.focus = (p.focus as i32 + delta).clamp(0, last as i32) as usize;
                    p.scroll = 0;
                }
            }
            D::Cursor(delta) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.move_option_cursor(p.focus, delta);
                }
            }
            D::CursorFirst => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                    && let Some(cursor) = p.option_cursor.get_mut(p.focus)
                {
                    *cursor = 0;
                }
            }
            D::CursorLast => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    let last = p.option_count(p.focus).saturating_sub(1);
                    if let Some(cursor) = p.option_cursor.get_mut(p.focus) {
                        *cursor = last;
                    }
                }
            }
            D::Scroll(delta) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.scroll = if delta < 0 {
                        p.scroll.saturating_sub(delta.unsigned_abs() as u16)
                    } else {
                        p.scroll.saturating_add(delta as u16)
                    };
                }
            }
            D::Select {
                question,
                option,
                advance,
            } => {
                let mut on_last = false;
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.select_option(question, option);
                    if advance {
                        on_last = p.focus + 1 >= p.questions.len();
                        if !on_last {
                            p.focus += 1;
                            p.scroll = 0;
                        }
                    }
                }
                submit_after = on_last;
            }
            D::Toggle { question, option } => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.toggle_option(question, option);
                }
            }
            D::StartEdit { question } => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.editing_custom = Some(question);
                    if let Some(cursor) = p.option_cursor.get_mut(question)
                        && let Some(q) = p.questions.get(question)
                    {
                        *cursor = q.options.len();
                    }
                    p.input = p.customs[question].clone();
                    p.error = None;
                }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.input.push(c);
                }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.input.pop();
                }
            }
            D::EditCommit => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    let qi = p.editing_custom.take().unwrap_or(0);
                    if p.input.trim().is_empty() {
                        p.customs[qi].clear();
                    } else {
                        p.customs[qi] = p.input.trim().to_owned();
                        if let Some(selection) = p.selections.get_mut(qi) {
                            *selection = None;
                        }
                    }
                    p.input.clear();
                    p.error = None;
                    if p.focus + 1 < p.questions.len() {
                        p.focus += 1;
                        p.scroll = 0;
                    } else {
                        submit_after = true;
                    }
                }
            }
            D::EditCancel => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                {
                    p.editing_custom = None;
                    p.input.clear();
                }
            }
            D::Dismiss => self.dismiss_ask(),
            D::None => {}
        }
        if submit_after {
            self.submit_ask();
        }
        true
    }

    pub(super) fn plan_key(&mut self, seed: &str, key: KeyEvent) -> bool {
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
            let Some(sess) = self.sessions.get(seed) else {
                return true;
            };
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
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut()
                {
                    p.entering_message = true;
                }
            }
            D::Scroll(delta) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut()
                {
                    if delta > 0 {
                        p.scroll = p.scroll.saturating_add(delta as usize);
                    } else {
                        p.scroll = p.scroll.saturating_sub((-delta) as usize);
                    }
                }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut()
                {
                    p.message.push(c);
                }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut()
                {
                    p.message.pop();
                }
            }
            D::SubmitReject => self.respond_plan(false, false),
            D::CancelEdit => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut()
                {
                    p.entering_message = false;
                    p.message.clear();
                }
            }
            D::None => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::{AskPanel, SessionState};
    use qaqh_client::{AskMode, DomainAskQuestion as AskQuestion};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with_ask() -> App {
        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-ask".to_string();
        let mut session = SessionState::new(seed.clone());
        session.pending_ask = Some(AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            AskMode::Batch,
            vec![
                AskQuestion {
                    id: "q1".into(),
                    question: "第一题".into(),
                    options: vec!["A".into(), "B".into(), "C".into()],
                    allow_custom: true,
                },
                AskQuestion {
                    id: "q2".into(),
                    question: "第二题".into(),
                    options: vec!["D".into(), "E".into()],
                    allow_custom: true,
                },
            ],
        ));
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    #[test]
    fn number_shortcut_selects_one_based_option_and_advances() {
        let mut app = app_with_ask();
        app.ask_key(
            "seed-ask",
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
        );

        let ask = app
            .sessions
            .get("seed-ask")
            .and_then(|s| s.pending_ask.as_ref())
            .expect("ask panel");
        assert_eq!(ask.focus, 1);
        assert_eq!(ask.selections[0], Some(1));
    }

    #[test]
    fn left_and_right_switch_question_pages() {
        let mut app = app_with_ask();
        app.ask_key(
            "seed-ask",
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(
            app.sessions["seed-ask"]
                .pending_ask
                .as_ref()
                .expect("ask")
                .focus,
            1
        );
        app.ask_key("seed-ask", KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            app.sessions["seed-ask"]
                .pending_ask
                .as_ref()
                .expect("ask")
                .focus,
            0
        );
    }

    #[tokio::test]
    async fn enter_on_last_question_submits_complete_answers() {
        let mut app = app_with_ask();
        {
            let ask = app
                .sessions
                .get_mut("seed-ask")
                .and_then(|s| s.pending_ask.as_mut())
                .expect("ask panel");
            ask.selections[0] = Some(0);
            ask.focus = 1;
            ask.option_cursor[1] = 1;
        }
        app.ask_key(
            "seed-ask",
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert!(
            app.sessions["seed-ask"].pending_ask.is_none(),
            "last question Enter should submit"
        );
    }

    #[test]
    fn enter_on_custom_row_enters_input_mode() {
        let mut app = app_with_ask();
        {
            let ask = app
                .sessions
                .get_mut("seed-ask")
                .and_then(|s| s.pending_ask.as_mut())
                .expect("ask panel");
            ask.option_cursor[0] = ask.questions[0].options.len();
        }
        app.ask_key(
            "seed-ask",
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        let ask = app
            .sessions
            .get("seed-ask")
            .and_then(|s| s.pending_ask.as_ref())
            .expect("ask panel");
        assert_eq!(ask.editing_custom, Some(0));
    }

    #[test]
    fn blocking_modal_consumes_global_workspace_keys() {
        let mut app = app_with_ask();
        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        )));
        assert!(
            app.overlays.is_empty(),
            "阻塞式 ask 不应被 Ctrl+L 压到 SessionList 下面"
        );
    }

    #[tokio::test]
    async fn incomplete_submit_returns_to_first_missing_question() {
        let mut app = app_with_ask();
        {
            let ask = app
                .sessions
                .get_mut("seed-ask")
                .and_then(|s| s.pending_ask.as_mut())
                .expect("ask panel");
            ask.focus = 1;
            ask.selections[1] = Some(0);
        }
        app.ask_key(
            "seed-ask",
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        let ask = app
            .sessions
            .get("seed-ask")
            .and_then(|s| s.pending_ask.as_ref())
            .expect("ask panel");
        assert_eq!(ask.focus, 0);
        assert!(ask.error.is_some());
    }
}
