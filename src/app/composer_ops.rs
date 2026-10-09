//! composer 按键、斜杠命令与附件上传（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    // ── slash 二级菜单辅助 ──
    pub fn slash_visible(&self) -> bool {
        if !self.overlays.is_empty() {
            return false;
        }
        if let Some(sess) = self.active_session() {
            let val = sess.composer.value();
            !crate::app::slash::completions_for(&val).is_empty()
        } else {
            false
        }
    }

    pub fn slash_candidates(&self) -> Vec<crate::app::slash::SlashDef> {
        if let Some(sess) = self.active_session() {
            crate::app::slash::completions_for(&sess.composer.value())
                .into_iter()
                .cloned()
                .collect()
        } else {
            vec![]
        }
    }

    pub(super) fn clamp_slash_selected(&mut self) {
        let n = self.slash_candidates().len();
        if n == 0 {
            self.slash_selected = 0;
        } else if self.slash_selected >= n {
            self.slash_selected = n - 1;
        }
    }

    pub(super) fn autocomplete_slash(&mut self) {
        let candidates = self.slash_candidates();
        if candidates.is_empty() {
            return;
        }
        let idx = self.slash_selected.min(candidates.len() - 1);
        let name = candidates[idx].name;
        if let Some(sess) = self.active_session_mut() {
            let new = format!("/{name} ");
            sess.composer.input = new.chars().collect();
            sess.composer.cursor = sess.composer.input.len();
        }
        self.slash_selected = 0;
    }

    pub(super) fn execute_slash_text(&mut self, raw: &str) -> bool {
        let trimmed = raw.trim();
        if trimmed.is_empty() || !trimmed.starts_with('/') {
            return false;
        }
        // 裸 "/" 留给菜单，不算命令
        if trimmed == "/" {
            return false;
        }
        let Some(cmd) = crate::app::slash::parse(trimmed) else {
            return false;
        };
        match cmd {
            SlashCmd::New { cwd } => {
                // 静默创建：按 effective_cwd 回退链；二级编辑仅按 Tab 按需触发
                match cwd {
                    Some(p) => {
                        let raw = p.trim().to_string();
                        if raw.is_empty() {
                            if let Some(sess) = self.active_session_mut() {
                                sess.composer.clear();
                            }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(None);
                        } else if raw == "?" || raw.eq_ignore_ascii_case("edit") {
                            let initial = self.effective_cwd(None).unwrap_or_default();
                            self.overlays.push(Overlay::CwdInput {
                                input: initial.chars().collect(),
                                cursor: initial.len(),
                            });
                            if let Some(sess) = self.active_session_mut() {
                                sess.composer.clear();
                            }
                            self.slash_selected = 0;
                        } else {
                            let expanded = crate::app::slash::expand_tilde(&raw);
                            if !crate::app::slash::is_absolute_path(&expanded) {
                                self.toast(NoticeLevel::Error, format!("cwd 需为绝对路径：{raw}"));
                                return true;
                            }
                            if let Some(sess) = self.active_session_mut() {
                                sess.composer.clear();
                            }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(Some(expanded));
                        }
                    }
                    None => {
                        if let Some(sess) = self.active_session_mut() {
                            sess.composer.clear();
                        }
                        self.slash_selected = 0;
                        self.new_session_with_cwd(None);
                    }
                }
                true
            }
            SlashCmd::Help => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.toggle_overlay(Overlay::Help);
                true
            }
            SlashCmd::Sessions => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.open_session_list();
                true
            }
            SlashCmd::Settings => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.toggle_settings();
                true
            }
            SlashCmd::Remote => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.open_remote();
                true
            }
            SlashCmd::History => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                // 打开即全屏（alternate screen）；数据源是 timeline 模型。
                self.overlays.push(Overlay::History {
                    selected: 0,
                    detail: false,
                    scroll: 0,
                });
                true
            }
            SlashCmd::Subagents => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.overlays.push(Overlay::Subagents {
                    selected: 0,
                    filter: String::new(),
                });
                if let Some(session_id) = self.active_session_id() {
                    self.fetch_team(session_id);
                }
                true
            }
            SlashCmd::Workspace => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.show_workspace = true;
                if let Some(session_id) = self.active_session_id() {
                    self.fetch_dashboard(session_id);
                }
                true
            }
            SlashCmd::Clear => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                true
            }
            SlashCmd::Export { path } => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.export_active_session(path);
                true
            }
            SlashCmd::Unknown(s) => {
                if s.is_empty() {
                    false
                } else {
                    self.toast(NoticeLevel::Error, format!("未知命令：/{s}"));
                    if let Some(sess) = self.active_session_mut() {
                        sess.composer.clear();
                    }
                    true
                }
            }
        }
    }

    /// `/export`：当前会话 → Markdown 文件 + toast 反馈（M4）。
    ///
    /// 无活动会话 / 写文件失败 → Error toast，不静默。
    /// 审计 F4：显式路径是 TUI 少有的任意文件写原语，而导出正文里的
    /// assistant 文本块逐字来自模型——模型可以引导用户把会话写到
    /// `~/.bashrc`、`~/.ssh/authorized_keys` 这类敏感位置。覆盖已有文件
    /// 或路径含隐藏组件时先弹二次确认；确认层绑定发起时的会话。
    fn export_active_session(&mut self, path: Option<String>) {
        let Some(session_id) = self.active_session_id() else {
            self.toast(NoticeLevel::Error, "无活动会话，无法导出");
            return;
        };
        let explicit = path.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let target = match explicit {
            Some(p) => std::path::PathBuf::from(crate::app::slash::expand_tilde(p)),
            None => crate::app::export::default_export_path(&session_id),
        };
        if explicit.is_some() && export_path_needs_confirm(&target) {
            self.overlays.push(Overlay::Confirm {
                action: ConfirmAction::ExportOverwrite {
                    session_id: session_id.clone(),
                    path: target,
                },
            });
            return;
        }
        self.export_session_to(&session_id, &target);
    }

    /// 实际写盘 + toast。`/export` 直写路径与 `Confirm(ExportOverwrite)` 的
    /// 确认回调共用这里；导出内容取 `session_id` 指定的会话，不是当下的
    /// 活动标签——与 `ConfirmAction::session_id()` 的判据同源。
    pub(super) fn export_session_to(&mut self, session_id: &str, target: &std::path::Path) {
        let Some(sess) = self.sessions.get(session_id) else {
            self.toast(NoticeLevel::Error, "无活动会话，无法导出");
            return;
        };
        let md = crate::app::export::export_markdown(sess);
        match std::fs::write(target, md) {
            Ok(()) => self.toast(NoticeLevel::Info, format!("已导出：{}", target.display())),
            Err(e) => self.toast(NoticeLevel::Error, format!("导出失败：{e}")),
        }
    }

    /// `/history` 详情里的「导出此回合」。
    ///
    /// 与详情视图共用 `export_turn_markdown`，所以导出的就是屏幕上看到的那份；
    /// 默认落到当前目录 `qaqh-turn-{session_id 前 8 位}-{序号}-{时间戳}.md`。
    pub(super) fn export_history_turn(&mut self, index: usize) {
        let Some(sess) = self.active_session() else {
            self.toast(NoticeLevel::Error, "无活动会话，无法导出");
            return;
        };
        let Some(turn) = sess.timeline.turns.get(index) else {
            self.toast(NoticeLevel::Error, "该回合不在当前窗口内");
            return;
        };
        let number = sess.timeline.turn_number(index);
        let md = crate::app::export::export_turn_markdown(turn, number as usize);
        let short: String = sess.session_id.chars().take(8).collect();
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let target = std::path::PathBuf::from(format!("qaqh-turn-{short}-{number}-{ts}.md"));
        match std::fs::write(&target, md) {
            Ok(()) => self.toast(
                NoticeLevel::Info,
                format!("已导出回合：{}", target.display()),
            ),
            Err(e) => self.toast(NoticeLevel::Error, format!("导出失败：{e}")),
        }
    }

    /// CwdInput 二级弹窗确认
    pub(super) fn confirm_cwd_input(&mut self, raw: String) {
        let cwd = raw.trim().to_owned();
        if cwd.is_empty() {
            self.new_session_with_cwd(None);
            return;
        }
        let cwd = crate::app::slash::expand_tilde(&cwd);
        if !crate::app::slash::is_absolute_path(&cwd) {
            self.toast(NoticeLevel::Error, format!("cwd 需为绝对路径：{cwd}"));
            return;
        }
        self.new_session_with_cwd(Some(cwd));
    }

    /// 品牌首屏输入框按键：复用 [`Composer`] 的编辑语义，但发送动作先走
    /// [`Self::start_draft_conversation`]，由 `SessionCreate` 落成后把草稿带进
    /// 真实会话 composer，用户确认后再发送。
    pub(super) fn draft_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if !self.pending_creates.is_empty() {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if alt => {
                self.draft_composer.insert('\n');
            }
            KeyCode::Char('j') if ctrl => self.draft_composer.insert('\n'),
            KeyCode::Enter => self.start_draft_conversation(),
            KeyCode::Esc => self.draft_composer.clear(),
            KeyCode::Backspace => {
                if ctrl {
                    self.draft_composer.word_left();
                    let cursor = self.draft_composer.cursor;
                    self.draft_composer.input.truncate(cursor);
                } else {
                    self.draft_composer.backspace();
                }
            }
            KeyCode::Delete => self.draft_composer.delete(),
            KeyCode::Left => {
                if ctrl {
                    self.draft_composer.word_left();
                } else {
                    self.draft_composer.left();
                }
            }
            KeyCode::Right => {
                if ctrl {
                    self.draft_composer.word_right();
                } else {
                    self.draft_composer.right();
                }
            }
            KeyCode::Home => self.draft_composer.home(),
            KeyCode::End => self.draft_composer.end(),
            KeyCode::Char('u') if ctrl => self.draft_composer.clear(),
            KeyCode::Char('k') if ctrl => {
                let cursor = self.draft_composer.cursor;
                self.draft_composer.input.truncate(cursor);
            }
            KeyCode::Char('w') if ctrl => {
                self.draft_composer.word_left();
                let cursor = self.draft_composer.cursor;
                self.draft_composer.input.truncate(cursor);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.draft_composer.insert(c);
            }
            _ => {}
        }
    }

    /// Composer 按键。
    pub(super) fn composer_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if self.active_v2_read_only() && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            let action = if key.code == KeyCode::Enter {
                "发送"
            } else {
                "中止"
            };
            self.reject_if_v2_read_only(action);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 粘贴护栏：洪流期的 Enter 一律降级为空格（终端不支持括号粘贴时，
        // 粘贴文本以按键流到达，回车会误触发送）。支持括号粘贴的终端不走这里。
        if matches!(key.code, KeyCode::Enter) && self.paste_guard.flooding() {
            if let Some(sess) = self.active_session_mut() {
                sess.composer.insert(' ');
            }
            if self.paste_guard.take_toast() {
                self.toast(
                    NoticeLevel::Warn,
                    "检测到粘贴洪流：已抑制 Enter 自动发送（当前终端不支持括号粘贴）",
                );
            }
            return;
        }
        // 斜杠菜单优先：Up/Down/Tab/Esc/Enter 劫持（需在借用前计算）
        let slash_vis = self.slash_visible();
        match key.code {
            KeyCode::Esc if slash_vis => {
                self.slash_selected = 0;
            }
            KeyCode::Tab if slash_vis => {
                self.autocomplete_slash();
            }
            KeyCode::Tab => {
                if self.autocomplete_mention() {
                    return;
                }
                // composer 为 /new 或 /n 且无参时，Tab 打开二级编辑（显式 CwdInput）
                let val = self
                    .active_session()
                    .map(|s| s.composer.value())
                    .unwrap_or_default();
                let trimmed = val.trim().to_string();
                if trimmed == "/new" || trimmed == "/n" {
                    let initial = self.effective_cwd(None).unwrap_or_default();
                    self.overlays.push(Overlay::CwdInput {
                        input: initial.chars().collect(),
                        cursor: initial.len(),
                    });
                    if let Some(sess) = self.active_session_mut() {
                        sess.composer.clear();
                    }
                    self.slash_selected = 0;
                }
            }
            // 多行输入：Alt+Enter / Ctrl+J 插入换行（Enter 仍为发送）。
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::ALT)
                    || (ctrl && key.code == KeyCode::Char('j')) =>
            {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.insert('\n');
                }
            }
            KeyCode::Enter
                if slash_vis
                    || self.active_session().is_some_and(|s| {
                        !s.composer.value().contains('\n')
                            && s.composer.value().trim_start().starts_with('/')
                    }) =>
            {
                // 若为 slash 输入，优先走 slash 执行或补全
                let val = self
                    .active_session()
                    .map(|s| s.composer.value())
                    .unwrap_or_default();
                let trimmed = val.trim().to_string();
                if trimmed.starts_with('/') {
                    if trimmed == "/" {
                        self.autocomplete_slash();
                        return;
                    }
                    // 若菜单可见且输入仍是前缀（无空格），Tab/Enter 应补全而非直接执行部分命令
                    let has_space = trimmed.contains(char::is_whitespace);
                    if slash_vis && !has_space {
                        // 若输入已是完整命令（如 "/new"），直接执行；否则补全
                        let without = trimmed
                            .strip_prefix('/')
                            .unwrap_or(trimmed.as_str())
                            .to_ascii_lowercase();
                        let exact = crate::app::slash::SLASH_COMMANDS
                            .iter()
                            .any(|d| d.name == without || (d.name == "new" && without == "n"));
                        if exact {
                            if self.execute_slash_text(&trimmed) {
                                return;
                            }
                        } else {
                            self.autocomplete_slash();
                            return;
                        }
                    } else if self.execute_slash_text(&trimmed) {
                        return;
                    } else if slash_vis {
                        self.autocomplete_slash();
                        return;
                    }
                }
                self.send_message();
            }
            KeyCode::Enter => {
                self.send_message();
            }
            KeyCode::Esc => self.cancel_turn(),
            KeyCode::Backspace => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_left();
                        let cur = s.composer.cursor;
                        while s.composer.input.len() > cur {
                            s.composer.input.pop();
                        }
                    } else {
                        s.composer.backspace();
                    }
                    true
                } else {
                    false
                };
                if need_clamp {
                    self.clamp_slash_selected();
                }
            }
            KeyCode::Delete => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    s.composer.delete();
                    true
                } else {
                    false
                };
                if need_clamp {
                    self.clamp_slash_selected();
                }
            }
            KeyCode::Left => {
                if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_left();
                    } else {
                        s.composer.left();
                    }
                }
            }
            KeyCode::Right => {
                if let Some(s) = self.active_session_mut() {
                    if ctrl {
                        s.composer.word_right();
                    } else {
                        s.composer.right();
                    }
                }
            }
            KeyCode::Home => {
                if ctrl {
                    self.scroll_top();
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.home();
                }
            }
            KeyCode::End => {
                if ctrl {
                    self.scroll_bottom();
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.end();
                }
            }
            KeyCode::Up => {
                if slash_vis {
                    let n = self.slash_candidates().len();
                    if n > 0 {
                        if self.slash_selected == 0 {
                            self.slash_selected = n - 1;
                        } else {
                            self.slash_selected -= 1;
                        }
                    }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_up();
                }
            }
            KeyCode::Down => {
                if slash_vis {
                    let n = self.slash_candidates().len();
                    if n > 0 {
                        self.slash_selected = (self.slash_selected + 1) % n;
                    }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_down();
                }
            }
            KeyCode::PageUp => self.scroll_up(20),
            KeyCode::PageDown => self.scroll_down(20),
            KeyCode::Char('a') if ctrl => {
                if let Some(session_id) = self.active_session_id() {
                    self.overlays.push(Overlay::AttachPath {
                        input: Vec::new(),
                        cursor: 0,
                        session_id,
                    });
                }
            }
            KeyCode::Char('p') if ctrl => self.toggle_mode(),
            KeyCode::Char('y') if ctrl => self.undo_turn(),
            KeyCode::Char('e') if ctrl => self.compact(),
            KeyCode::Char('u') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    s.composer.clear();
                }
            }
            KeyCode::Char('k') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    let cur = s.composer.cursor;
                    s.composer.input.truncate(cur);
                }
            }
            KeyCode::Char('w') if ctrl => {
                if let Some(s) = self.active_session_mut() {
                    s.composer.word_left();
                    let cur = s.composer.cursor;
                    while s.composer.input.len() > cur {
                        s.composer.input.pop();
                    }
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    s.composer.insert(c);
                    true
                } else {
                    false
                };
                if need_clamp {
                    self.clamp_slash_selected();
                }
            }
            _ => {}
        }
    }

    /// 上传附件到 [`AttachSubmit`] 指定的会话。
    ///
    /// 目标 session_id **只**来自 `AttachSubmit`（即 `Overlay::AttachPath` 里存的那个），
    /// 这里不再查 `active_session_id()`——那会让「判据说属于 session_id X、执行挂到活动标签」
    /// 两条路径相反，切标签后附件落到用户没在看的会话上。
    pub fn upload_attachment(&mut self, submit: AttachSubmit) {
        let (session_id, path) = submit.into_parts();
        // 目标会话已关闭：剪枝理论上已经拦住了（关闭标签会剪掉它的 overlay），
        // 这里兜住竞态（overlay 打开期间会话被 daemon 关掉），别静默丢附件。
        if !self.sessions.contains_key(&session_id) {
            self.toast(
                NoticeLevel::Error,
                format!("附件目标会话已关闭：{session_id}"),
            );
            return;
        }
        self.spawn_api(move |api, tx| async move {
            let read_path = path.clone();
            let result =
                tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
                    let bytes = std::fs::read(&read_path).map_err(|e| e.to_string())?;
                    let media = guess_media_type(&read_path);
                    Ok((bytes, media))
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            match result {
                Ok((bytes, media)) => {
                    // 上传走 qaqh-client（multipart 组装在 client 侧），返回的
                    // ContentRef 过桥回本仓镜像类型。没有连接（测试替身）时
                    // `upload_content` 立刻返回 Err——`session_id` 仍然照实回传。
                    let uploaded = api.upload_content(&session_id, &media, bytes).await;
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded {
                        session_id,
                        path,
                        result: uploaded,
                    }));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded {
                        session_id,
                        path,
                        result: Err(e),
                    }));
                }
            }
        });
    }

    // ───────────────────────── toast / 滚动 ─────────────────────────
}

/// 审计 F4：导出目标是否需要二次确认。
///
/// 两条规则：① 覆盖任何**已存在**的文件（不可恢复的破坏）；
/// ② 路径含隐藏组件——`~/.bashrc`、`~/.ssh/authorized_keys` 是模型引导
/// 社工的最常见落点。`./x.md` 的 `CurDir` 不算隐藏，常规导出不受影响。
fn export_path_needs_confirm(target: &std::path::Path) -> bool {
    if target.exists() {
        return true;
    }
    target
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .any(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::SessionState;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// 审计 F4 回归：已存在文件与隐藏路径必须走确认；常规路径不拦截。
    #[test]
    fn export_confirm_targets_existing_or_hidden_paths() {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-export-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".ssh")).expect("mkdir");
        let existing = dir.join("notes.md");
        std::fs::write(&existing, "old").expect("seed");

        assert!(export_path_needs_confirm(&existing), "覆盖已存在文件需确认");
        assert!(
            export_path_needs_confirm(&dir.join(".bashrc")),
            "隐藏文件需确认"
        );
        assert!(
            export_path_needs_confirm(&dir.join(".ssh").join("authorized_keys")),
            "隐藏目录下的文件需确认"
        );
        assert!(
            !export_path_needs_confirm(&dir.join("qaqh-export-新.md")),
            "常规新路径不拦截"
        );
        assert!(
            !export_path_needs_confirm(&dir.join("./notes-2.md")),
            "相对路径的 ./ 前缀不算隐藏"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn app_with_session() -> App {
        let (mut app, _rx) = App::new_for_test();
        let session_id = "session-export".to_string();
        let session = SessionState::new(session_id.clone());
        app.tabs.push(session_id.clone());
        app.sessions.insert(session_id, session);
        app
    }

    /// 审计 F4 回归：`/export` 指向已存在文件时先弹确认且不写盘；
    /// 按 `y` 后才写入；确认层绑定发起会话。
    #[test]
    fn export_to_existing_path_asks_before_writing() {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-export-flow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("transcript.md");
        std::fs::write(&target, "old-content").expect("seed");

        let mut app = app_with_session();
        let command = format!("/export {}", target.display());
        assert!(app.execute_slash_text(&command), "命令应被消费");

        let Some(Overlay::Confirm {
            action: ConfirmAction::ExportOverwrite { session_id, path },
        }) = app.overlays.last()
        else {
            panic!("应弹出导出确认：{:?}", app.overlays.last());
        };
        assert_eq!(session_id, "session-export", "确认层绑定发起会话");
        assert_eq!(path, &target);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "old-content",
            "确认前不得写盘"
        );

        // `y` = 确认写入；确认层同时出栈。
        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        )));
        assert!(app.overlays.is_empty(), "确认后弹层应出栈");
        let written = std::fs::read_to_string(&target).unwrap();
        assert_ne!(written, "old-content", "确认后应写盘");
        assert!(written.contains("# "), "导出内容应为 Markdown：{written}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
