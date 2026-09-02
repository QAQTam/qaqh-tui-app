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
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(None);
                        } else if raw == "?" || raw.eq_ignore_ascii_case("edit") {
                            let initial = self.effective_cwd(None).unwrap_or_default();
                            self.overlays.push(Overlay::CwdInput { input: initial.chars().collect(), cursor: initial.len() });
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                        } else {
                            let expanded = crate::app::slash::expand_tilde(&raw);
                            if !crate::app::slash::is_absolute_path(&expanded) {
                                self.toast(NoticeLevel::Error, format!("cwd 需为绝对路径：{raw}"));
                                return true;
                            }
                            if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                            self.slash_selected = 0;
                            self.new_session_with_cwd(Some(expanded));
                        }
                    }
                    None => {
                        if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                        self.slash_selected = 0;
                        self.new_session_with_cwd(None);
                    }
                }
                true
            }
            SlashCmd::Help => {
                if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                self.slash_selected = 0;
                self.toggle_overlay(Overlay::Help);
                true
            }
            SlashCmd::Clear => {
                if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                self.slash_selected = 0;
                true
            }
            SlashCmd::Unknown(s) => {
                if s.is_empty() {
                    false
                } else {
                    self.toast(NoticeLevel::Error, format!("未知命令：/{s}"));
                    if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                    true
                }
            }
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

    /// Composer 按键。
    pub(super) fn composer_key(&mut self, key: KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
                let val = self.active_session().map(|s| s.composer.value()).unwrap_or_default();
                let trimmed = val.trim().to_string();
                if trimmed == "/new" || trimmed == "/n" {
                    let initial = self.effective_cwd(None).unwrap_or_default();
                    self.overlays.push(Overlay::CwdInput { input: initial.chars().collect(), cursor: initial.len() });
                    if let Some(sess) = self.active_session_mut() { sess.composer.clear(); }
                    self.slash_selected = 0;
                }
            }
            KeyCode::Enter if slash_vis || self.active_session().is_some_and(|s| s.composer.value().trim_start().starts_with('/')) => {
                // 若为 slash 输入，优先走 slash 执行或补全
                let val = self.active_session().map(|s| s.composer.value()).unwrap_or_default();
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
                        let without = trimmed.strip_prefix('/').unwrap_or(trimmed.as_str()).to_ascii_lowercase();
                        let exact = crate::app::slash::SLASH_COMMANDS.iter().any(|d| d.name == without || (d.name == "new" && without == "n"));
                        if exact {
                            if self.execute_slash_text(&trimmed) { return; }
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
            KeyCode::Enter => { self.send_message();},
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
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
            }
            KeyCode::Delete => {
                let need_clamp = if let Some(s) = self.active_session_mut() {
                    s.composer.delete();
                    true
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
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
                        if self.slash_selected == 0 { self.slash_selected = n - 1; } else { self.slash_selected -= 1; }
                    }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_up();
                }
            }
            KeyCode::Down => {
                if slash_vis {
                    let n = self.slash_candidates().len();
                    if n > 0 { self.slash_selected = (self.slash_selected + 1) % n; }
                } else if let Some(s) = self.active_session_mut() {
                    s.composer.history_down();
                }
            }
            KeyCode::PageUp => self.page_up(),
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
                } else { false };
                if need_clamp { self.clamp_slash_selected(); }
            }
            _ => {}
        }
    }

    pub fn upload_attachment(&mut self, path: String) {
        let Some(seed) = self.active_seed() else { return };
        self.spawn_api(move |client, tx| async move {
            let read_path = path.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
                let bytes = std::fs::read(&read_path).map_err(|e| e.to_string())?;
                let media = guess_media_type(&read_path);
                Ok((bytes, media))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
            match result {
                Ok((bytes, media)) => {
                    let uploaded = client.upload_content(&seed, &media, bytes).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppMsg::Action(ActionResult::Uploaded { seed, path, result: uploaded }));
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
