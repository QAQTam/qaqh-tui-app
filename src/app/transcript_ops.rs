//! 对话动作（发送/取消/压缩/撤销）与滚动（自 app/mod.rs 拆分）。

use super::*;

impl App {
    /// 切换指定工具卡正文；鼠标和键盘共用此语义入口。
    pub fn toggle_tool_expanded(&mut self, block_id: &str) -> bool {
        let Some(seed) = self.active_seed() else {
            return false;
        };
        let changed = self
            .sessions
            .get_mut(&seed)
            .is_some_and(|session| session.toggle_tool_expanded(block_id));
        if changed {
            self.force_redraw = true;
        }
        changed
    }

    /// 切换指定历史 thinking 正文；鼠标和键盘共用此语义入口。
    pub fn toggle_thinking_expanded(&mut self, block_id: &str) -> bool {
        let Some(seed) = self.active_seed() else {
            return false;
        };
        let changed = self
            .sessions
            .get_mut(&seed)
            .is_some_and(|session| session.toggle_thinking_expanded(block_id));
        if changed {
            self.force_redraw = true;
        }
        changed
    }

    /// `Alt+T` 键盘路径：切换当前会话最后一段有正文的历史 thinking。
    pub fn toggle_latest_thinking(&mut self) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let block_id = self.sessions.get(&seed).and_then(|session| {
            session
                .timeline
                .turns
                .iter()
                .rev()
                .flat_map(|turn| turn.rounds.iter().rev())
                .flat_map(|round| round.blocks.iter().rev())
                .find(|block| {
                    block.kind == qaqh_client::TimelineBlockKind::Reasoning
                        && !block.text.is_empty()
                })
                .map(|block| block.block_id.clone())
        });
        if let Some(block_id) = block_id {
            self.toggle_thinking_expanded(&block_id);
        }
    }

    /// `Alt+E` 键盘路径：切换当前会话最后一张工具卡。
    pub fn toggle_latest_tool(&mut self) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let block_id = self.sessions.get(&seed).and_then(|session| {
            session
                .timeline
                .turns
                .iter()
                .rev()
                .flat_map(|turn| turn.rounds.iter().rev())
                .flat_map(|round| round.blocks.iter().rev())
                .find(|block| block.tool.is_some())
                .map(|block| block.block_id.clone())
        });
        if let Some(block_id) = block_id {
            self.toggle_tool_expanded(&block_id);
        }
    }

    pub fn send_message(&mut self) {
        if self.reject_if_v2_read_only("发送") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        if sess.composer.is_empty() {
            return;
        }
        let (text, attachments) = sess.composer.take();
        let content_refs: Vec<ContentRef> = attachments.into_iter().map(|a| a.content).collect();
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&seed.clone()),
                    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                        text,
                        images: vec![],
                        attachments: (!content_refs.is_empty()).then_some(content_refs),
                        // 后端锚点 8dbe22e（`45c6b63`）新增的两字段，均为
                        // `#[serde(default)]`，**行为中性**：
                        // - `message_id: None` = 回落到 command_id（用户/UI 消息的既定语义）；
                        // - `input_purpose: TriggerTurn` = 本行改动前「投递并触发回合」的行为。
                        //   `QueueOnly` 只服务子代理注入，UI 用不到。
                        //   类型名显式写出依赖后端 PR #289 的再导出（已合入 8dbe22e）；
                        //   在旧锚点 5ec1900 上只能写 `Default::default()`。
                        message_id: None,
                        input_purpose: ConversationInputPurpose::TriggerTurn,
                        as_system: false,
                        // Subagent V2 additions; regular UI messages are neither
                        // inter-agent deliveries nor terminal notifications.
                        inter_agent: None,
                        subagent_terminal: None,
                    }),
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "发送",
                result,
            }));
        });
    }

    pub fn cancel_turn(&mut self) {
        if self.reject_if_v2_read_only("中止") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        let streaming = self
            .sessions
            .get(&seed)
            .is_some_and(|s| s.streaming.is_some());
        if !streaming {
            return;
        }
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&seed.clone()),
                    RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                        turn_id: None,
                    }),
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "中止",
                result,
            }));
        });
    }

    pub fn toggle_mode(&mut self) {
        if self.reject_if_v2_read_only("切换模式") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        let next = match self.sessions.get(&seed).map(|s| s.mode) {
            Some(qaqh_client::ConversationMode::Plan) => qaqh_client::ConversationMode::Code,
            _ => qaqh_client::ConversationMode::Plan,
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.mode = next; // 乐观更新
        }
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&seed.clone()),
                    RingingCommand::Conversation(ConversationCommand::ConversationSetMode {
                        mode: next,
                    }),
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "切换模式",
                result,
            }));
        });
    }

    pub fn compact(&mut self) {
        if self.reject_if_v2_read_only("压缩") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&seed.clone()),
                    RingingCommand::Conversation(ConversationCommand::ConversationCompact {
                        turn_id: None,
                    }),
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label: "compact",
                result,
            }));
        });
    }

    pub fn undo_turn(&mut self) {
        if self.reject_if_v2_read_only("撤销") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        let Some(turn_id) = self
            .sessions
            .get(&seed)
            .and_then(|s| s.timeline.last_turn_id().map(str::to_owned))
        else {
            return;
        };
        self.undo_turn_from(seed, turn_id);
    }

    /// 用户消息 context menu 的二次确认入口。
    pub fn confirm_undo_turn(&mut self, turn_id: String) {
        if self.reject_if_v2_read_only("撤销") {
            return;
        }
        let Some(seed) = self.active_seed() else {
            return;
        };
        let exists = self.sessions.get(&seed).is_some_and(|session| {
            session
                .timeline
                .turns
                .iter()
                .any(|turn| turn.turn_id == turn_id)
        });
        if !exists {
            self.toast(NoticeLevel::Warn, "该回合已不在当前 timeline 窗口中");
            return;
        }
        self.overlays.push(Overlay::Confirm {
            action: ConfirmAction::UndoTurn { seed, turn_id },
        });
    }

    pub fn undo_turn_from(&mut self, seed: String, turn_id: String) {
        self.spawn_api(move |api, tx| async move {
            // command_id 由本侧生成：ack 之后要拿它轮询 receipt。
            let command_id = uuid::Uuid::new_v4().to_string();
            let result = api
                .send_command(
                    Some(&seed),
                    RingingCommand::Conversation(ConversationCommand::ConversationUndoTurn {
                        turn_id,
                    }),
                    qaqh_client::CommandOptions {
                        command_id: Some(command_id.clone()),
                        ..Default::default()
                    },
                )
                .await;
            match result {
                Ok(_) => {
                    // ACK ≠ 完成：轮询 receipt 到终态（对齐 winui，但消费其结果）。
                    let mut state: Option<RingingCommandStatus> = None;
                    for _ in 0..30 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if let Ok(status) = api
                            .command_status(&command_id)
                            .await
                            // 终态 = 成功/失败/拒绝（与旧镜像的 `is_terminal` 同义；
                            // 权威类型不提供该辅助方法）。
                            && matches!(
                                status.state,
                                qaqh_client::RingingCommandState::Succeeded
                                    | qaqh_client::RingingCommandState::Failed
                                    | qaqh_client::RingingCommandState::Rejected
                            )
                        {
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
        let seed = seed.to_owned();
        self.spawn_api(move |api, tx| async move {
            let result = api
                .timeline_page(&seed, None, crate::runtime::TIMELINE_PAGE_LIMIT)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::Rebaseline { seed, result }));
        });
    }

    pub fn load_older(&mut self) {
        let Some(seed) = self.active_seed() else {
            return;
        };
        let loading = self
            .sessions
            .get(&seed)
            .is_some_and(|s| s.loading_older || !s.timeline.has_more);
        if loading {
            return;
        }
        // 游标 = 本窗口最旧那个回合的**全局序号**。`turn_index` 为 None 时
        // （实时追加的回合不带序号）说明窗口里没有可当游标的回合，直接放弃。
        let first_index = self
            .sessions
            .get(&seed)
            .and_then(|s| s.timeline.turns.first().and_then(|t| t.turn_index));
        let Some(before) = first_index else { return };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.loading_older = true;
        }
        self.spawn_api(move |api, tx| async move {
            let result = api
                .timeline_page(&seed, Some(before), crate::runtime::TIMELINE_PAGE_LIMIT)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::LoadOlder { seed, result }));
        });
    }

    // ───────────────────────── 交互响应命令 ─────────────────────────

    pub(super) fn send_control_command(
        &mut self,
        seed: String,
        command: ControlCommand,
        label: &'static str,
    ) {
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(command),
                    Default::default(),
                )
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label,
                result,
            }));
        });
    }

    /// 交互应答（permission / ask / plan）专用：与 [`App::send_control_command`] 同款，
    /// 但走带 ack 上限的 `send_interaction_command`。超时经既有 `CommandAck` 错误分支
    /// 落到状态栏 toast。
    pub(super) fn send_interaction_command(
        &mut self,
        seed: String,
        command: ControlCommand,
        label: &'static str,
    ) {
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_interaction_command(Some(&seed), RingingCommand::Control(command))
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                seed: Some(seed),
                label,
                result,
            }));
        });
    }

    // ───────────────────────── 服务面 ─────────────────────────

    pub fn scroll_up(&mut self, lines: usize) {
        let Some(seed) = self.view_seed() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        sess.scroll.follow = false;
        sess.scroll.offset = sess.scroll.offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let Some(seed) = self.view_seed() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&seed) else {
            return;
        };
        if sess.scroll.offset <= lines {
            sess.scroll.offset = 0;
            sess.scroll.follow = true;
        } else {
            sess.scroll.offset -= lines;
        }
    }

    pub fn scroll_top(&mut self) {
        let Some(seed) = self.view_seed() else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.scroll.follow = false;
            sess.scroll.offset = usize::MAX / 2; // 渲染时 clamp
        }
    }

    pub fn scroll_bottom(&mut self) {
        let Some(seed) = self.view_seed() else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&seed) {
            sess.scroll.follow = true;
            sess.scroll.offset = 0;
        }
    }
}
