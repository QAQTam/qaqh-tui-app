//! 对话动作（发送/取消/压缩/撤销）、滚动与工具卡展开（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
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
                        if let Ok(status) = client.command_status(&command_id).await
                            && status.state.is_terminal() {
                                state = Some(status);
                                break;
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

    pub(super) fn request_rebaseline(&mut self, seed: &str) {
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

    pub(super) fn send_control_command(&mut self, seed: String, command: ControlCommand, label: &'static str) {
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

    /// PageUp：滚动；到顶且还有更早回合 → 触发分页加载。
    pub(super) fn page_up(&mut self) {
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

    pub(super) fn toggle_tool_expand(&mut self) {
        let Some(seed) = self.active_seed() else { return };
        let Some(sess) = self.sessions.get_mut(&seed) else { return };
        // 收集所有可折叠工具（有输出或 diff），按时间逆序；携带 name 以计算视觉展开态
        let mut candidates: Vec<(String, String)> = Vec::new();
        for turn in sess.timeline.turns.iter().rev() {
            for round in turn.rounds.iter().rev() {
                for block in round.blocks.iter().rev() {
                    if let Some(tool) = &block.tool {
                        let has_content = tool.output.as_deref().is_some_and(|s| !s.trim().is_empty())
                            || !tool.progress.trim().is_empty()
                            || tool.diff.as_deref().is_some_and(|d| !d.trim().is_empty());
                        if has_content {
                            candidates.push((tool.tool_call_id.clone(), tool.name.clone()));
                        }
                    }
                }
            }
        }
        if candidates.is_empty() { return; }
        // 视觉展开态 = expanded_raw ^ is_default_expanded(name)，F7 在此视觉上切换
        let is_visual_expanded = |id: &str, name: &str| {
            let raw = sess.expanded_tools.contains(id);
            raw ^ crate::app::render_transcript::is_default_expanded(name)
        };
        // 策略：优先展开最近的“视觉收起”；若全部已展开，则收起最近的展开态（循环）
        let mut target: Option<String> = None;
        for (id, name) in &candidates {
            if !is_visual_expanded(id, name) {
                target = Some(id.clone());
                break;
            }
        }
        if target.is_none() {
            // 全部已视觉展开 → 收起最近一个
            target = candidates.first().map(|(id, _)| id.clone());
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

}
