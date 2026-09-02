//! 挂起交互：permission / ask / plan 的按键与应答（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    pub fn submit_ask(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(panel) = self.sessions.get_mut(&seed).and_then(|s| s.pending_ask.as_ref()) else {
            return;
        };
        let answers = match panel.collect_answers() {
            Ok(a) => a,
            Err(e) => {
                if let Some(sess) = self.sessions.get_mut(&seed)
                    && let Some(p) = sess.pending_ask.as_mut() {
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

    /// 交互弹窗按键。返回 true = 已消费。优先级 permission > ask > plan。
    pub(super) fn modal_key(&mut self, key: KeyEvent) -> bool {
        let Some(seed) = self.active_seed() else { return false };
        let route = {
            let Some(sess) = self.sessions.get(&seed) else { return false };
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
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_permissions.first_mut() {
                        p.trust_folder = !p.trust_folder;
                    }
            }
            D::None => {}
        }
        true
    }

    pub(super) fn ask_key(&mut self, seed: &str, key: KeyEvent) -> bool {
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
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
                        p.focus = p.focus.saturating_sub(1);
                    }
            }
            D::FocusDown => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                        && p.focus + 1 < p.questions.len() {
                            p.focus += 1;
                        }
            }
            D::Select { focus, option } => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut()
                        && p.questions
                            .get(focus)
                            .map(|q| option < q.options.len())
                            .unwrap_or(false)
                        {
                            p.selections[focus] = Some(option);
                            p.error = None;
                        }
            }
            D::StartEdit { focus } => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
                        p.editing_custom = Some(focus);
                        p.input = p.customs[focus].clone();
                    }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
                        p.input.push(c);
                    }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
                        p.input.pop();
                    }
            }
            D::EditCommit => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
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
            D::EditCancel => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_ask.as_mut() {
                        p.editing_custom = None;
                        p.input.clear();
                    }
            }
            D::Submit => self.submit_ask(),
            D::Dismiss => self.dismiss_ask(),
            D::None => {}
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
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut() {
                        p.entering_message = true;
                    }
            }
            D::Scroll(delta) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut() {
                        if delta > 0 {
                            p.scroll = p.scroll.saturating_add(delta as usize);
                        } else {
                            p.scroll = p.scroll.saturating_sub((-delta) as usize);
                        }
                    }
            }
            D::EditChar(c) => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut() {
                        p.message.push(c);
                    }
            }
            D::EditBackspace => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut() {
                        p.message.pop();
                    }
            }
            D::SubmitReject => self.respond_plan(false, false),
            D::CancelEdit => {
                if let Some(s) = self.sessions.get_mut(seed)
                    && let Some(p) = s.pending_plan.as_mut() {
                        p.entering_message = false;
                        p.message.clear();
                    }
            }
            D::None => {}
        }
        true
    }


}
