//! 覆盖层/首页按键路由（自 app/mod.rs 拆分，行为不变）。

use super::*;

impl App {
    /// 标签/观测切换后调用：清掉不属于当前标签 seed 的 overlay。
    ///
    /// 判据见 [`Overlay::bound_seed`]——全局 overlay（设置/帮助/会话列表/cwd）保留。
    /// 不这么做的话，`Ctrl+W` 弹出的 `Confirm(CloseTab(旧 seed))` 会在切标签后
    /// 仍然吃 `y`，把确认动作落到那个已经不在前台的会话上。
    pub(super) fn prune_overlays_for_active_seed(&mut self) {
        let seed = self.active_seed();
        prune_seed_bound_overlays(&mut self.overlays, seed.as_deref());
    }

    pub fn open_session_list(&mut self) {
        if self
            .overlays
            .last()
            .is_some_and(|o| matches!(o, Overlay::SessionList { .. }))
        {
            self.overlays.pop();
            return;
        }
        let stale = self
            .session_list_at
            .map(|t| t.elapsed() > Duration::from_secs(3))
            .unwrap_or(true);
        if stale {
            self.fetch_session_list();
        }
        self.overlays.push(Overlay::SessionList {
            selected: 0,
            show_archived: false,
        });
    }

    /// 会话列表的过滤谓词（与渲染一致）。
    pub(super) fn filtered_sessions(&self, show_archived: bool) -> Vec<usize> {
        self.session_list_cache
            .iter()
            .enumerate()
            .filter(|(_, m)| (show_archived || !m.meta.archived) && !m.meta.ephemeral)
            .map(|(i, _)| i)
            .collect()
    }

    /// 首页（无 tab 时）按键：复用会话列表的导航，视觉更直接
    pub(super) fn home_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        // 允许 Ctrl 组合已被全局键处理，这里只处理首页专属
        let items = self.filtered_sessions(self.home_show_archived);
        let count = items.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    if self.home_selected == 0 {
                        self.home_selected = count - 1;
                    } else {
                        self.home_selected -= 1;
                    }
                }
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.home_selected = (self.home_selected + 1) % count;
                }
                return true;
            }
            KeyCode::Enter => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].meta.seed.clone();
                    self.open_session_tab(&seed);
                }
                return true;
            }
            KeyCode::Char('n') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_session();
                return true;
            }
            KeyCode::Char('r') => {
                self.fetch_session_list();
                return true;
            }
            KeyCode::Char('a') => {
                self.home_show_archived = !self.home_show_archived;
                self.home_selected = 0;
                return true;
            }
            KeyCode::Char('x') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].meta.seed.clone();
                    self.overlays.push(Overlay::Confirm {
                        action: ConfirmAction::ArchiveSession(seed),
                    });
                }
                return true;
            }
            KeyCode::Char('u') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].meta.seed.clone();
                    self.unarchive_session(seed);
                }
                return true;
            }
            KeyCode::Char('D') => {
                if let Some(&idx) = items.get(self.home_selected) {
                    let seed = self.session_list_cache[idx].meta.seed.clone();
                    self.overlays.push(Overlay::Confirm {
                        action: ConfirmAction::DeleteSession(seed),
                    });
                }
                return true;
            }
            _ => {}
        }
        // 首页下也允许 j/k 翻页等，防止落入 composer
        matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Enter
                | KeyCode::Char('n')
                | KeyCode::Char('r')
                | KeyCode::Char('a')
                | KeyCode::Char('x')
                | KeyCode::Char('u')
                | KeyCode::Char('D')
        )
    }

    pub(super) fn overlay_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let Some(top) = self.overlays.last().cloned() else {
            return false;
        };

        match top {
            Overlay::Help => {
                self.overlays.pop();
                true
            }
            Overlay::Settings(mut st) => {
                use ratatui::crossterm::event::KeyCode;
                // ── 编辑态：按键全部进缓冲 ──
                if st.editing.is_some() {
                    let mut buf = st.editing.take().expect("checked");
                    match key.code {
                        KeyCode::Esc => {} // 取消：editing 保持 None
                        KeyCode::Enter => {
                            if let Err(e) = st.commit_edit(self.config.as_ref(), buf) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                        KeyCode::Backspace => {
                            if buf.cursor > 0 {
                                buf.cursor -= 1;
                                buf.buf.remove(buf.cursor);
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Delete => {
                            if buf.cursor < buf.buf.len() {
                                buf.buf.remove(buf.cursor);
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Left => {
                            buf.cursor = buf.cursor.saturating_sub(1);
                            st.editing = Some(buf);
                        }
                        KeyCode::Right => {
                            if buf.cursor < buf.buf.len() {
                                buf.cursor += 1;
                            }
                            st.editing = Some(buf);
                        }
                        KeyCode::Home => {
                            buf.cursor = 0;
                            st.editing = Some(buf);
                        }
                        KeyCode::End => {
                            buf.cursor = buf.buf.len();
                            st.editing = Some(buf);
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            buf.buf.insert(buf.cursor.min(buf.buf.len()), c);
                            buf.cursor += 1;
                            st.editing = Some(buf);
                        }
                        _ => st.editing = Some(buf),
                    }
                    self.replace_overlay(Overlay::Settings(st));
                    return true;
                }

                // ── 浏览态 ──
                let row = st.row();
                let id = row.id;
                let kind = row.kind;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlays.pop();
                        return true; // 关闭即丢弃草稿（标题有 ● 未保存提示）
                    }
                    KeyCode::Up | KeyCode::Char('k') => st.move_focus(-1),
                    KeyCode::Down | KeyCode::Char('j') => st.move_focus(1),
                    KeyCode::PageUp => st.move_focus(-8),
                    KeyCode::PageDown => st.move_focus(8),
                    KeyCode::Enter => match kind {
                        FieldKind::Text
                        | FieldKind::Secret
                        | FieldKind::Number
                        | FieldKind::Float => {
                            st.editing = st.start_edit(self.config.as_ref());
                        }
                        FieldKind::Enum => {
                            if let Err(e) = st.cycle(self.config.as_ref(), 1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                        FieldKind::Toggle => {
                            let _ = st.cycle(self.config.as_ref(), 1);
                        }
                        FieldKind::Port => self.settings_port_activate(&mut st),
                    },
                    KeyCode::Left => match kind {
                        FieldKind::Port => self.settings_port_cycle(&mut st, -1),
                        _ => {
                            if let Err(e) = st.cycle(self.config.as_ref(), -1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                    },
                    KeyCode::Right => match kind {
                        FieldKind::Port => self.settings_port_cycle(&mut st, 1),
                        _ => {
                            if let Err(e) = st.cycle(self.config.as_ref(), 1) {
                                self.toast(NoticeLevel::Error, e);
                            }
                        }
                    },
                    KeyCode::Char('s') | KeyCode::Char('S') => self.save_settings(&mut st),
                    KeyCode::Char('r') => self.fetch_config(),
                    // 权限级别：聚焦该行时数字键即时生效（沿用旧面板行为）。
                    KeyCode::Char(c @ '1'..='4') if id == settings::FieldId::PermissionLevel => {
                        self.set_permission_level(c as u8 - b'0');
                    }
                    _ => {}
                }
                self.replace_overlay(Overlay::Settings(st));
                true
            }
            Overlay::AttachPath {
                mut input,
                mut cursor,
                seed,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlays.pop();
                    }
                    KeyCode::Enter => {
                        let path: String = input.iter().collect();
                        self.overlays.pop();
                        let path = path.trim().to_owned();
                        if !path.is_empty() {
                            self.upload_attachment(path);
                        }
                        let _ = seed;
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            input.remove(cursor - 1);
                            cursor -= 1;
                        }
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor,
                            seed,
                        });
                    }
                    KeyCode::Delete => {
                        if cursor < input.len() {
                            input.remove(cursor);
                        }
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor,
                            seed,
                        });
                    }
                    KeyCode::Left => {
                        cursor = cursor.saturating_sub(1);
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor,
                            seed,
                        });
                    }
                    KeyCode::Right => {
                        if cursor < input.len() {
                            cursor += 1;
                        }
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor,
                            seed,
                        });
                    }
                    KeyCode::Home => {
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor: 0,
                            seed,
                        });
                    }
                    KeyCode::End => {
                        let n = input.len();
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor: n,
                            seed,
                        });
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.insert(cursor.min(input.len()), c);
                        cursor += 1;
                        self.replace_overlay(Overlay::AttachPath {
                            input,
                            cursor,
                            seed,
                        });
                    }
                    _ => {}
                }
                true
            }
            Overlay::Confirm { action } => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => match action {
                        ConfirmAction::DeleteSession(seed) => self.delete_session(seed.clone()),
                        ConfirmAction::ArchiveSession(seed) => self.archive_session(seed.clone()),
                        ConfirmAction::CloseTab(seed) => {
                            let seed = seed.clone();
                            self.close_tab_by_seed(&seed);
                        }
                    },
                    _ => {}
                }
                self.overlays.pop();
                true
            }
            Overlay::SessionList {
                selected,
                show_archived,
            } => {
                let items = self.filtered_sessions(show_archived);
                let count = items.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlays.pop();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let next = selected.saturating_sub(1);
                        self.replace_overlay(Overlay::SessionList {
                            selected: next,
                            show_archived,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let next = (selected + 1).min(count.saturating_sub(1));
                        self.replace_overlay(Overlay::SessionList {
                            selected: next,
                            show_archived,
                        });
                    }
                    KeyCode::Char('a') => {
                        self.replace_overlay(Overlay::SessionList {
                            selected,
                            show_archived: !show_archived,
                        });
                    }
                    KeyCode::Char('r') => self.fetch_session_list(),
                    KeyCode::Char('n') => {
                        self.overlays.pop();
                        self.new_session();
                    }
                    KeyCode::Enter => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].meta.seed.clone();
                            self.overlays.pop();
                            self.open_session_tab(&seed);
                        }
                    }
                    KeyCode::Char('x') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].meta.seed.clone();
                            self.overlays.push(Overlay::Confirm {
                                action: ConfirmAction::ArchiveSession(seed),
                            });
                        }
                    }
                    KeyCode::Char('u') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].meta.seed.clone();
                            self.unarchive_session(seed);
                        }
                    }
                    KeyCode::Char('D') => {
                        if let Some(&meta_idx) = items.get(selected) {
                            let seed = self.session_list_cache[meta_idx].meta.seed.clone();
                            self.overlays.push(Overlay::Confirm {
                                action: ConfirmAction::DeleteSession(seed),
                            });
                        }
                    }
                    _ => {}
                }
                true
            }
            Overlay::CwdInput {
                mut input,
                mut cursor,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlays.pop();
                    }
                    KeyCode::Enter => {
                        let raw: String = input.iter().collect();
                        self.overlays.pop();
                        self.confirm_cwd_input(raw);
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            input.remove(cursor - 1);
                            cursor -= 1;
                        }
                        self.replace_overlay(Overlay::CwdInput { input, cursor });
                    }
                    KeyCode::Delete => {
                        if cursor < input.len() {
                            input.remove(cursor);
                        }
                        self.replace_overlay(Overlay::CwdInput { input, cursor });
                    }
                    KeyCode::Left => {
                        cursor = cursor.saturating_sub(1);
                        self.replace_overlay(Overlay::CwdInput { input, cursor });
                    }
                    KeyCode::Right => {
                        if cursor < input.len() {
                            cursor += 1;
                        }
                        self.replace_overlay(Overlay::CwdInput { input, cursor });
                    }
                    KeyCode::Home => {
                        self.replace_overlay(Overlay::CwdInput { input, cursor: 0 });
                    }
                    KeyCode::End => {
                        let n = input.len();
                        self.replace_overlay(Overlay::CwdInput { input, cursor: n });
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.insert(cursor.min(input.len()), c);
                        cursor += 1;
                        self.replace_overlay(Overlay::CwdInput { input, cursor });
                    }
                    _ => {}
                }
                true
            }
        }
    }

    pub(super) fn replace_overlay(&mut self, overlay: Overlay) {
        if !self.overlays.is_empty() {
            let n = self.overlays.len();
            self.overlays[n - 1] = overlay;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirm_close(seed: &str) -> Overlay {
        Overlay::Confirm {
            action: ConfirmAction::CloseTab(seed.into()),
        }
    }

    fn attach(seed: &str) -> Overlay {
        Overlay::AttachPath {
            input: Vec::new(),
            cursor: 0,
            seed: seed.into(),
        }
    }

    /// 回归：切标签后，绑在旧 seed 上的 overlay 必须消失。
    ///
    /// 证伪方式：去掉 `prune_overlays_for_active_seed()` 的调用（旧行为）——此时
    /// `Confirm(CloseTab("a"))` 会留在栈里，切到标签 b 后按 `y` 仍然关闭 a。
    /// 本测试直接锁住「切换后旧 seed 的 overlay 不在了」这个不变量。
    #[test]
    fn seed_bound_overlays_do_not_survive_a_tab_switch() {
        let mut overlays = vec![
            confirm_close("a"),
            Overlay::Settings(crate::app::settings::SettingsState::default()),
            attach("a"),
        ];
        prune_seed_bound_overlays(&mut overlays, Some("b"));
        assert_eq!(
            overlays.len(),
            1,
            "旧 seed 的确认/附件 overlay 必须被清掉：{overlays:?}"
        );
        assert!(
            matches!(overlays[0], Overlay::Settings(_)),
            "全局 overlay 不许被误伤：{overlays:?}"
        );
    }

    /// 反方向：目标就是同一个 seed 时不清；没有活动标签（首页）时 seed 绑定的一律作废。
    #[test]
    fn pruning_is_seed_scoped_not_a_blanket_clear() {
        let mut same = vec![confirm_close("a"), attach("a")];
        prune_seed_bound_overlays(&mut same, Some("a"));
        assert_eq!(same.len(), 2, "同 seed 的 overlay 不该被清：{same:?}");

        let mut home = vec![confirm_close("a"), Overlay::Help];
        prune_seed_bound_overlays(&mut home, None);
        assert_eq!(
            home.len(),
            1,
            "首页没有 seed，绑定的 overlay 作废：{home:?}"
        );
        assert!(matches!(home[0], Overlay::Help));
    }

    /// 多层栈 + 连续切换（审查建议 5）：全局层按**原相对顺序**留下，且栈顶必须是
    /// 全局层——渲染与按键路由都取 `overlays.last()`，绝不能让一个过期的确认层
    /// 接管按键。
    #[test]
    fn pruning_keeps_global_layers_in_order() {
        let mut overlays = vec![
            Overlay::Settings(crate::app::settings::SettingsState::default()),
            confirm_close("a"),
            Overlay::Help,
            attach("a"),
        ];
        prune_seed_bound_overlays(&mut overlays, Some("b"));
        assert_eq!(overlays.len(), 2, "只剩两个全局层：{overlays:?}");
        assert!(
            matches!(overlays[0], Overlay::Settings(_)),
            "相对顺序不变：{overlays:?}"
        );
        assert!(
            matches!(overlays[1], Overlay::Help),
            "相对顺序不变：{overlays:?}"
        );
        assert!(
            overlays.last().is_some_and(|o| o.bound_seed().is_none()),
            "栈顶必须是全局 overlay：{overlays:?}"
        );

        // 连续切换（b → a → 无标签）不会把已经作废的层捞回来。
        prune_seed_bound_overlays(&mut overlays, Some("a"));
        prune_seed_bound_overlays(&mut overlays, None);
        assert_eq!(overlays.len(), 2, "全局层始终在：{overlays:?}");
        assert!(matches!(overlays[1], Overlay::Help));
    }

    /// 判据本身（这是「别一刀切」的可执行说明）：哪些算 seed 绑定、哪些算全局。
    #[test]
    fn bound_seed_classifies_overlays() {
        assert_eq!(confirm_close("a").bound_seed(), Some("a"));
        assert_eq!(
            Overlay::Confirm {
                action: ConfirmAction::DeleteSession("x".into())
            }
            .bound_seed(),
            Some("x")
        );
        assert_eq!(
            Overlay::Confirm {
                action: ConfirmAction::ArchiveSession("y".into())
            }
            .bound_seed(),
            Some("y")
        );
        assert_eq!(attach("s").bound_seed(), Some("s"));

        assert_eq!(
            Overlay::Settings(crate::app::settings::SettingsState::default()).bound_seed(),
            None,
            "设置页是全局 overlay"
        );
        assert_eq!(Overlay::Help.bound_seed(), None);
        assert_eq!(
            Overlay::SessionList {
                selected: 0,
                show_archived: false
            }
            .bound_seed(),
            None,
            "会话列表是 daemon 全局的，不属于某个标签"
        );
        assert_eq!(
            Overlay::CwdInput {
                input: Vec::new(),
                cursor: 0
            }
            .bound_seed(),
            None,
            "/new 的 cwd 输入此时还没有 seed"
        );
    }
}
