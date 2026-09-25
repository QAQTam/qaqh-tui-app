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
            SlashCmd::Workspace => {
                if let Some(sess) = self.active_session_mut() {
                    sess.composer.clear();
                }
                self.slash_selected = 0;
                self.show_workspace = true;
                if let Some(seed) = self.active_seed() {
                    self.fetch_dashboard(seed);
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
    fn export_active_session(&mut self, path: Option<String>) {
        let Some(sess) = self.active_session() else {
            self.toast(NoticeLevel::Error, "无活动会话，无法导出");
            return;
        };
        let md = crate::app::export::export_markdown(sess);
        let target = match path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => {
                let expanded = crate::app::slash::expand_tilde(p);
                std::path::PathBuf::from(expanded)
            }
            None => crate::app::export::default_export_path(&sess.seed),
        };
        match std::fs::write(&target, md) {
            Ok(()) => self.toast(NoticeLevel::Info, format!("已导出：{}", target.display())),
            Err(e) => self.toast(NoticeLevel::Error, format!("导出失败：{e}")),
        }
    }

    /// `/history` 详情里的「导出此回合」。
    ///
    /// 与详情视图共用 `export_turn_markdown`，所以导出的就是屏幕上看到的那份；
    /// 默认落到当前目录 `qaqh-turn-{seed 前 8 位}-{序号}-{时间戳}.md`。
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
        let short: String = sess.seed.chars().take(8).collect();
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
                if let Some(seed) = self.active_seed() {
                    self.overlays.push(Overlay::AttachPath {
                        input: Vec::new(),
                        cursor: 0,
                        seed,
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
    /// 目标 seed **只**来自 `AttachSubmit`（即 `Overlay::AttachPath` 里存的那个），
    /// 这里不再查 `active_seed()`——那会让「判据说属于 seed X、执行挂到活动标签」
    /// 两条路径相反，切标签后附件落到用户没在看的会话上。
    pub fn upload_attachment(&mut self, submit: AttachSubmit) {
        let (seed, path) = submit.into_parts();
        // 目标会话已关闭：剪枝理论上已经拦住了（关闭标签会剪掉它的 overlay），
        // 这里兜住竞态（overlay 打开期间会话被 daemon 关掉），别静默丢附件。
        if !self.sessions.contains_key(&seed) {
            self.toast(NoticeLevel::Error, format!("附件目标会话已关闭：{seed}"));
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
                    // `upload_content` 立刻返回 Err——`seed` 仍然照实回传。
                    let uploaded = api.upload_content(&seed, &media, bytes).await;
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded {
                        seed,
                        path,
                        result: uploaded,
                    }));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded {
                        seed,
                        path,
                        result: Err(e),
                    }));
                }
            }
        });
    }

    // ───────────────────────── toast / 滚动 ─────────────────────────
}
