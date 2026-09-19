//! 覆盖层/首页按键路由（自 app/mod.rs 拆分，行为不变）。

use super::*;
use qaqh_client::TimelineBlockKind;

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

    /// §4.5：Ctrl+T 思考回放——**当前活动回合**的 reasoning body 在内存
    /// （D1 唯一保留点），零抓取。无活动回合/无思考内容 → toast，不推空浮层。
    /// 历史回合不在此列：offloaded 的远端只有预览壳，全文回放 v2 立项。
    pub(crate) fn open_thinking_overlay(&mut self) {
        let Some(seed) = self.view_seed() else {
            return;
        };
        let Some(sess) = self.sessions.get(&seed) else {
            return;
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(tid) = sess.timeline.running_turn_id()
            && let Some(t) = sess.timeline.turns.iter().find(|t| t.turn_id == tid)
        {
            for r in &t.rounds {
                for b in &r.blocks {
                    if b.kind == TimelineBlockKind::Reasoning && !b.text.is_empty() {
                        parts.push(b.text.clone());
                    }
                }
            }
        }
        if parts.is_empty() {
            self.toast(NoticeLevel::Info, "当前回合没有进行中的思考");
            return;
        }
        self.overlays.push(Overlay::Thinking {
            seed,
            scroll: 0,
            body: parts.join("\n\n"),
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
            Overlay::Thinking { body, .. } => {
                // 只读回放：滚动 + Esc 关闭。总行数按折行后算（与 draw 同一 wrap）。
                let total = body.lines().count().max(1);
                match key.code {
                    KeyCode::Esc => {
                        self.overlays.pop();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if let Some(Overlay::Thinking { scroll, .. }) = self.overlays.last_mut() {
                            *scroll = scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if let Some(Overlay::Thinking { scroll, .. }) = self.overlays.last_mut() {
                            *scroll = (*scroll + 1).min(total.saturating_sub(1));
                        }
                    }
                    KeyCode::PageUp => {
                        if let Some(Overlay::Thinking { scroll, .. }) = self.overlays.last_mut() {
                            *scroll = scroll.saturating_sub(20);
                        }
                    }
                    KeyCode::PageDown => {
                        if let Some(Overlay::Thinking { scroll, .. }) = self.overlays.last_mut() {
                            *scroll = (*scroll + 20).min(total.saturating_sub(1));
                        }
                    }
                    KeyCode::Char('e') => {
                        // M4（T15）：交给 `$PAGER` 全文浏览——置位后由 main.rs
                        // 在帧间挂起终端执行（本层拿不到 terminal）。
                        self.pending_pager = Some(body.clone());
                    }
                    _ => {}
                }
                true
            }
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
                        self.overlays.pop();
                        // 目标 seed 只从 overlay 自己的字段取（单一事实源，见
                        // `AttachSubmit`）——这里没有任何别的 seed 可查；空路径 → None。
                        if let Some(submit) = Overlay::attach_submit(&input, &seed) {
                            self.upload_attachment(submit);
                        }
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
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

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

    fn attach_with_path(seed: &str, path: &str) -> Overlay {
        Overlay::AttachPath {
            input: path.chars().collect(),
            cursor: path.chars().count(),
            seed: seed.into(),
        }
    }

    /// 与 `overlay_key` 的 Enter 分支同款取值（从 `AttachPath` 的字段直接构造提交）。
    fn submit_of(ov: &Overlay) -> Option<AttachSubmit> {
        match ov {
            Overlay::AttachPath { input, seed, .. } => Overlay::attach_submit(input, seed),
            _ => None,
        }
    }

    /// 附件提交的目标 seed 只能来自 overlay 自己（纯函数半边）。
    ///
    /// 证伪方式：把 `attach_submit` 的目标换成别的来源（活动标签 / 空串）→ 第一条
    /// 断言变红。**注意**：这条只锁「提交语义」；「Enter 分支真的把 overlay 的 seed
    /// 交出去」由下面的 `enter_uploads_to_the_overlays_own_seed_not_the_active_tab`
    /// 走全链路锁住（那条才是能证伪原缺陷的）。
    #[test]
    fn attach_submit_targets_its_own_seed() {
        let submit = submit_of(&attach_with_path("a", "/tmp/shot.png")).expect("非空路径应提交");
        let (target, path) = submit.into_parts();
        assert_eq!(target, "a", "目标 seed 必须取 overlay 存的那个");
        assert_eq!(path, "/tmp/shot.png");

        // 空 / 纯空白路径不提交（沿用旧行为）。
        assert!(submit_of(&attach("a")).is_none());
        assert!(submit_of(&attach_with_path("a", "   ")).is_none());
        // 路径两端空白被去掉。
        assert_eq!(
            submit_of(&attach_with_path("a", "  /tmp/x.png ")).map(|s| s.into_parts()),
            Some(("a".to_string(), "/tmp/x.png".to_string()))
        );
    }

    /// 剪枝判据与提交目标必须自洽：切到别的标签后 `AttachPath` 已被剪掉，不存在
    /// 「提交到别的标签」的窗口；只要它还在，目标就恒为自己那个 seed。
    ///
    /// 证伪方式：把 `bound_seed()` 对 `AttachPath` 改回 `None`（审查给过的备选
    /// 方案），或把剪枝改回 no-op——`overlays.is_empty()` 立刻变红。
    #[test]
    fn attach_target_and_pruning_agree_on_the_seed() {
        let mut overlays = vec![attach_with_path("a", "/tmp/x.png")];
        assert_eq!(
            submit_of(&overlays[0]).map(|s| s.into_parts().0),
            Some("a".to_string())
        );

        prune_seed_bound_overlays(&mut overlays, Some("b"));
        assert!(
            overlays.is_empty(),
            "切到别的标签后 AttachPath 必须被剪掉：{overlays:?}"
        );
        assert!(
            overlays.iter().filter_map(submit_of).next().is_none(),
            "没有 overlay 就没有可提交的目标"
        );
    }

    // ───────── 阻断 1（PR #20 第二轮复审）：Enter 分支的全链路 ─────────

    /// 从消息通道里取出上传结果的归属 seed（等后台任务投递）。
    async fn upload_seed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(AppMsg::Action(ActionResult::Uploaded { seed, .. })) => return Some(seed),
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("上传任务应在 5s 内投递结果")
    }

    fn test_app_with_tabs(
        seeds: &[&str],
        active: usize,
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
        let (mut app, rx) = App::new_for_test();
        for seed in seeds {
            app.tabs.push((*seed).to_string());
            app.sessions
                .insert((*seed).to_string(), SessionState::new((*seed).to_string()));
        }
        app.active = active;
        (app, rx)
    }

    /// 负向断言：在给定窗口内**不得**出现上传结果。
    ///
    /// 不能只 `try_recv()` 一下——上传任务是 spawn 出去的，立刻查空会假绿。
    async fn assert_no_upload(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, ms: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return;
            }
            match tokio::time::timeout(left, rx.recv()).await {
                Err(_) | Ok(None) => return,
                Ok(Some(AppMsg::Action(ActionResult::Uploaded { seed, .. }))) => {
                    panic!("不该起上传任务，却收到 seed={seed} 的上传结果")
                }
                Ok(Some(_)) => continue,
            }
        }
    }

    /// **阻断 1 的回归**：走 `overlay_key` 的 Enter 分支（经 `App::handle` 的完整
    /// 派发），overlay 绑 A、活动标签是 B，上传目标必须是 **A**。
    ///
    /// 证伪方式：把上传目标改回「提交那一刻的活动标签」（旧行为：`upload_attachment`
    /// 内部查 `active_seed()`）→ 本测试读到 `Uploaded { seed: "B" }`，断言立刻变红。
    /// 路径用一个不存在的文件：读取失败的分支同样会把归属 seed 原样回传，因此断言
    /// 不依赖网络，也不需要真 daemon。
    #[tokio::test]
    async fn enter_uploads_to_the_overlays_own_seed_not_the_active_tab() {
        let (mut app, mut rx) = test_app_with_tabs(&["A", "B"], 1);
        app.overlays
            .push(attach_with_path("A", "/nonexistent/qaqh-test-attach.png"));

        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        assert!(app.overlays.is_empty(), "Enter 后 AttachPath 应关闭");
        assert_eq!(
            upload_seed(&mut rx).await.as_deref(),
            Some("A"),
            "上传目标必须是 overlay 自己的 seed（A），而不是活动标签（B）"
        );
    }

    /// 空路径：Enter 照样关闭 overlay，但不起上传任务（沿用旧行为）。
    #[tokio::test]
    async fn enter_on_an_empty_path_closes_without_uploading() {
        let (mut app, mut rx) = test_app_with_tabs(&["A"], 0);
        app.overlays.push(attach("A"));

        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        assert!(app.overlays.is_empty(), "空路径也要关掉 overlay");
        assert_no_upload(&mut rx, 200).await;
    }

    /// 防线之一（上轮新增、此前零覆盖）：目标会话已关闭 → 可见提示 + 不起上传任务。
    #[tokio::test]
    async fn enter_on_a_closed_target_session_toasts_and_skips_upload() {
        let (mut app, mut rx) = test_app_with_tabs(&["B"], 0);
        app.overlays.push(attach_with_path("gone", "/tmp/x.png"));

        app.handle(AppMsg::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        assert!(app.overlays.is_empty());
        assert!(
            app.toasts
                .back()
                .is_some_and(|t| t.text.contains("附件目标会话已关闭")),
            "必须给出可见提示：{:?}",
            app.toasts
        );
        assert_no_upload(&mut rx, 200).await;
    }

    /// 防线之二（上轮新增、此前零覆盖）：上传成功但目标会话已消失 → 报错而不是静默丢弃。
    #[test]
    fn upload_result_for_a_vanished_session_is_visible() {
        let (mut app, _rx) = App::new_for_test();
        app.handle(AppMsg::Action(ActionResult::Uploaded {
            seed: "gone".into(),
            path: "/tmp/x.png".into(),
            result: Ok(qaqh_client::ContentRef {
                content_id: "c1".into(),
                media_type: "image/png".into(),
                sha256: "0".repeat(64),
                truncated: false,
            }),
        }));
        assert!(
            app.toasts
                .back()
                .is_some_and(|t| t.text.contains("目标会话已关闭")),
            "上传成功但会话没了也必须可见：{:?}",
            app.toasts
        );
    }

    /// 上传结果按 seed 归属：活动标签是 B，结果带 seed A → 附件落在 A 的 composer。
    #[test]
    fn upload_result_lands_on_the_seed_it_carries() {
        let (mut app, _rx) = test_app_with_tabs(&["A", "B"], 1);
        app.handle(AppMsg::Action(ActionResult::Uploaded {
            seed: "A".into(),
            path: "/tmp/shot.png".into(),
            result: Ok(qaqh_client::ContentRef {
                content_id: "c1".into(),
                media_type: "image/png".into(),
                sha256: "0".repeat(64),
                truncated: false,
            }),
        }));
        assert_eq!(
            app.sessions["A"].composer.attachments.len(),
            1,
            "附件必须落在 seed A 的会话上"
        );
        assert_eq!(
            app.sessions["B"].composer.attachments.len(),
            0,
            "活动标签 B 不该拿到附件"
        );
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
