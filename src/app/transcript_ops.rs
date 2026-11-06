//! 对话动作（发送/取消/模式切换）与滚动（自 app/mod.rs 拆分）。
//!
//! backend-v3 命令面拍平：`conversation.send_message` 走权威 payload 结构体，
//! 模式切换并入 `session.settings.patch`；压缩/撤销命令已从协议删除（见各函数）。

use super::*;
use qaqh_ringing::ConversationSendMessage;
use qaqh_ringing_contract::fact::InputPurpose;

impl App {
    /// 切换指定工具卡正文；鼠标和键盘共用此语义入口。
    pub fn toggle_tool_expanded(&mut self, block_id: &str) -> bool {
        let Some(session_id) = self.active_session_id() else {
            return false;
        };
        let changed = self
            .sessions
            .get_mut(&session_id)
            .is_some_and(|session| session.toggle_tool_expanded(block_id));
        if changed {
            self.redraw_requested = true;
        }
        changed
    }

    /// 切换指定历史 thinking 正文；鼠标和键盘共用此语义入口。
    pub fn toggle_thinking_expanded(&mut self, block_id: &str) -> bool {
        let Some(session_id) = self.active_session_id() else {
            return false;
        };
        let changed = self
            .sessions
            .get_mut(&session_id)
            .is_some_and(|session| session.toggle_thinking_expanded(block_id));
        if changed {
            self.redraw_requested = true;
        }
        changed
    }

    /// `Alt+T` 键盘路径：切换当前会话最后一段有正文的历史 thinking。
    pub fn toggle_latest_thinking(&mut self) {
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let block_id = self.sessions.get(&session_id).and_then(|session| {
            session
                .timeline
                .turns
                .iter()
                .rev()
                .flat_map(|turn| turn.rounds.iter().rev())
                .flat_map(|round| round.blocks.iter().rev())
                .find(|block| {
                    block.kind == crate::app::timeline_compat::TimelineBlockKind::Reasoning
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
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let block_id = self.sessions.get(&session_id).and_then(|session| {
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
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if sess.composer.is_empty() {
            return;
        }
        let (text, attachments) = sess.composer.take();
        // §6：暂存原草稿——后端在挂起回合期间拒绝普通发送（turn_busy）时，
        // ack 处理器据此回填 composer，用户不必整段重打。
        self.last_rejected_send = Some((session_id.clone(), text.clone()));
        sess.pending_user_pin = (!text.is_empty()).then(|| text.clone());
        sess.pinned_user_turn = None;
        sess.scroll.follow = false;
        sess.scroll.offset = 0;
        sess.scroll.tween = None;
        // 手动发送即视为首屏草稿已处置：清掉自动发送标记，防止 bootstrap 稍后
        // 落地时对空 composer 触发一次幽灵补发（send_message 对空输入会静默
        // 返回，标记却会一直挂着）。
        self.pending_initial_prompt = None;
        let content_refs: Vec<ContentRef> = attachments.into_iter().map(|a| a.content).collect();
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&session_id.clone()),
                    // backend-v3：命令面拍平，`conversation.send_message` 的
                    // payload 是权威结构体（`qaqh_ringing::ConversationSendMessage`）。
                    RingingCommand::ConversationSendMessage(ConversationSendMessage {
                        text,
                        images: vec![],
                        attachments: (!content_refs.is_empty()).then_some(content_refs),
                        // `message_id: None` = 回落到 command_id（用户/UI 消息的
                        // 既定语义）；`input_purpose: TriggerTurn` = 投递并触发回合
                        //（QueueOnly/Steer 只服务子代理注入，UI 用不到）。
                        message_id: None,
                        input_purpose: InputPurpose::TriggerTurn,
                        as_system: false,
                        // regular UI messages are neither inter-agent deliveries
                        // nor terminal notifications.
                        inter_agent: None,
                        subagent_terminal: None,
                    }),
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id),
                label: "发送",
                result,
            }));
        });
    }

    pub fn cancel_turn(&mut self) {
        if self.reject_if_v2_read_only("中止") {
            return;
        }
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let streaming = self
            .sessions
            .get(&session_id)
            .is_some_and(|s| s.streaming.is_some());
        if !streaming {
            return;
        }
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&session_id.clone()),
                    RingingCommand::ConversationCancel { turn_id: None },
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id),
                label: "中止",
                result,
            }));
        });
    }

    pub fn toggle_mode(&mut self) {
        if self.reject_if_v2_read_only("切换模式") {
            return;
        }
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let next = match self.sessions.get(&session_id).map(|s| s.mode) {
            Some(session::SessionMode::Plan) => session::SessionMode::Code,
            _ => session::SessionMode::Plan,
        };
        if let Some(sess) = self.sessions.get_mut(&session_id) {
            sess.mode = next; // 乐观更新
        }
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(
                    Some(&session_id.clone()),
                    // backend-v3：模式切换并入 session.settings.patch（"code"/"plan"）。
                    RingingCommand::SessionSettingsPatch {
                        patch: qaqh_ringing::SessionSettingsPatch {
                            mode: Some(next.wire().to_owned()),
                            ..Default::default()
                        },
                    },
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id),
                label: "切换模式",
                result,
            }));
        });
    }

    pub fn compact(&mut self) {
        if self.reject_if_v2_read_only("压缩") {
            return;
        }
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        // backend-v3 移除了手动压缩命令（`ConversationCompact` 不在命令目录）：
        // 压缩由 daemon 服务端自动执行，以 `TimelineEntry::Compaction` 落 timeline。
        // 这里保留入口只为了给 slash 调用方一个明确回执，而不是静默无效。
        self.toast(
            NoticeLevel::Info,
            "v3 daemon 会自动压缩上下文（历史以摘要条目呈现），无需手动触发",
        );
        let _ = session_id;
    }

    pub fn undo_turn(&mut self) {
        if self.reject_if_v2_read_only("撤销") {
            return;
        }
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let Some(turn_id) = self
            .sessions
            .get(&session_id)
            .and_then(|s| s.timeline.last_turn_id().map(str::to_owned))
        else {
            return;
        };
        self.undo_turn_from(session_id, turn_id);
    }

    /// 用户消息 context menu 的二次确认入口。
    pub fn confirm_undo_turn(&mut self, turn_id: String) {
        if self.reject_if_v2_read_only("撤销") {
            return;
        }
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let exists = self.sessions.get(&session_id).is_some_and(|session| {
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
            action: ConfirmAction::UndoTurn {
                session_id,
                turn_id,
            },
        });
    }

    /// `/model`：会话级模型切换（backend-v3 `session.settings.patch`）。
    ///
    /// 目录里的模型按（provider, model）字典序循环，选中即写
    /// `model_selection: Use { value }`，`settings_revision` CAS 基线取
    /// `session.list` 缓存里该会话的 meta。乐观更新本地 conversation 缓存；
    /// 409（revision 冲突）= meta 过期，下次列表刷新后再试。
    pub fn cycle_session_model(&mut self) {
        let Some(session_id) = self.active_session_id() else {
            return;
        };
        let selections = self.provider_catalog.selections();
        if selections.is_empty() {
            self.toast(
                NoticeLevel::Warn,
                "模型目录为空：先在设置页「模型目录」里添加 provider 与模型",
            );
            return;
        }
        let current = self
            .sessions
            .get(&session_id)
            .and_then(|s| s.conversation.as_ref())
            .and_then(|c| c.model.clone());
        // 本地缓存的是模型 ID（不含 provider）；目录里按 model_id 匹配首个。
        let idx = current
            .as_deref()
            .and_then(|model| selections.iter().position(|(_, m)| m == model))
            .map_or(0, |i| (i + 1) % selections.len());
        let next = selections[idx].clone();
        let (provider_id, model_id) = next;
        let settings_revision = self
            .session_list_cache
            .iter()
            .find(|entry| entry.meta.session_id == session_id)
            .map(|entry| entry.meta.settings_revision)
            .unwrap_or(0);
        let session_id_move = session_id.clone();
        let model_id_move = model_id.clone();
        self.spawn_api(move |api, tx| async move {
            let options = qaqh_client::CommandOptions {
                command_id: None,
                preconditions: vec![qaqh_ringing::CommandPrecondition::SettingsRevision {
                    revision: settings_revision,
                }],
            };
            let result = api
                .send_command(
                    Some(&session_id_move),
                    RingingCommand::SessionSettingsPatch {
                        patch: qaqh_ringing::SessionSettingsPatch {
                            model_selection: Some(qaqh_types::SettingValue::Use {
                                value: qaqh_types::ModelSelection {
                                    provider_id,
                                    model_id: model_id_move,
                                    options: Default::default(),
                                },
                            }),
                            ..Default::default()
                        },
                    },
                    options,
                )
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id_move),
                label: "切换模型",
                result: result.map_err(|e| e.to_string()),
            }));
        });
        // 乐观更新：worker 下一轮装配请求即用新模型。
        if let Some(sess) = self.sessions.get_mut(&session_id) {
            let cache = sess
                .conversation
                .get_or_insert_with(crate::app::session::ConversationCache::default);
            cache.model = Some(model_id);
        }
        self.redraw_requested = true;
    }

    pub fn undo_turn_from(&mut self, _session_id: String, _turn_id: String) {
        // backend-v3 移除了 `ConversationUndoTurn` 命令：回合一旦提交即为
        // 不可回滚事实（B09 裁定的「明确允许丢失暂态」的同一面）。入口保留
        // 是为了给确认弹窗一个明确回执，而不是静默无效。
        self.toast(
            NoticeLevel::Info,
            "v3 daemon 不支持撤销已提交回合（事实不可回滚）",
        );
    }


    pub fn load_older(&mut self) {
        // The current backend exposes one canonical conversation snapshot and
        // no legacy timeline paging endpoint. Production snapshots are marked
        // complete, so the renderer does not offer this action.
    }

    // ───────────────────────── 交互响应命令 ─────────────────────────

    /// 会话命令统一出口（backend-v3：命令面拍平，target 由
    /// `ApiCtx::send_command` 按 `Some(session_id)` 映射）。
    pub(super) fn send_session_command(
        &mut self,
        session_id: String,
        command: RingingCommand,
        label: &'static str,
    ) {
        self.spawn_api(move |api, tx| async move {
            let result = api
                .send_command(Some(&session_id), command, Default::default())
                .await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id),
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
        session_id: String,
        command: RingingCommand,
        label: &'static str,
    ) {
        self.spawn_api(move |api, tx| async move {
            let result = api.send_interaction_command(Some(&session_id), command).await;
            let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                session_id: Some(session_id),
                label,
                result,
            }));
        });
    }

    // ───────────────────────── 服务面 ─────────────────────────

    pub fn scroll_up(&mut self, lines: usize) {
        let Some(session_id) = self.view_session_id() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        // 交互滚动（wheel/PageUp）：视觉偏移缓动收敛到新目标。
        sess.pending_user_pin = None;
        sess.pinned_user_turn = None;
        sess.scroll.follow = false;
        let target = sess.scroll.offset.saturating_add(lines);
        if self.local_config.performance_mode {
            sess.scroll.offset = target;
            sess.scroll.tween = None;
        } else {
            sess.scroll.scroll_to(target);
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let Some(session_id) = self.view_session_id() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        sess.pending_user_pin = None;
        sess.pinned_user_turn = None;
        if sess.scroll.offset <= lines {
            // 重新咬住底部是语义跳变：follow 分支直接画底，无需缓动。
            sess.scroll.offset = 0;
            sess.scroll.follow = true;
        } else {
            let target = sess.scroll.offset - lines;
            if self.local_config.performance_mode {
                sess.scroll.offset = target;
                sess.scroll.tween = None;
            } else {
                sess.scroll.scroll_to(target);
            }
        }
    }

    pub fn scroll_top(&mut self) {
        let Some(session_id) = self.view_session_id() else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&session_id) {
            sess.pending_user_pin = None;
            sess.pinned_user_turn = None;
            sess.scroll.follow = false;
            sess.scroll.offset = usize::MAX / 2; // 渲染时 clamp
        }
    }

    pub fn scroll_bottom(&mut self) {
        let Some(session_id) = self.view_session_id() else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&session_id) {
            sess.pending_user_pin = None;
            sess.pinned_user_turn = None;
            sess.scroll.follow = true;
            sess.scroll.offset = 0;
        }
    }
}
