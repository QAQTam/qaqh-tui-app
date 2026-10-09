//! `/remote` 页的按键与动作（状态与载荷组装在 [`crate::app::remote`]）。
//!
//! 这个页面**不自己发 HTTP**：配对/设备三个调用全部经 `ApiCtx` → `qaqh_client`，
//! 与本仓「传输层只有 qaqh-client 一份」的约定保持一致。

use super::*;
use crate::app::remote::{
    PairTicket, RemoteField, RemotePending, RemoteState, build_pair_payload, clamp_device_name,
    host_name,
};
use crate::runtime::ConnectTarget;
use qaqh_client::PairScope;
use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

/// 签发档位（与 `PairScope` 一一对应，游标即下标）。
pub const PAIR_SCOPES: [PairScope; 3] = [PairScope::View, PairScope::Interact, PairScope::Admin];

/// 档位的人话标签。`admin` 与另外两档不在同一个风险量级上，文案必须说出来。
pub const PAIR_SCOPE_LABELS: [&str; 3] = [
    "view（只看）",
    "interact（可发消息）",
    "admin（等同运维，慎给）",
];

/// 配对码上手机那一侧的平台标签（进设备登记表，只是给人看的名字）。
const PAIR_PLATFORM: &str = "mobile";

impl App {
    /// 打开 `/remote`：预填当前连接目标 + 本机 daemon.json 快照。
    pub fn open_remote(&mut self) {
        if self
            .overlays
            .last()
            .is_some_and(|o| matches!(o, Overlay::Remote(_)))
        {
            return;
        }
        let mut state = RemoteState::default();
        if let ConnectTarget::Remote(target) = self.runtime.current_target() {
            state.draft_url = target.base_url;
            // token 不回显：草稿留空表示「不动它」，连接动作会拒绝空 token 并提示重填。
            state.status =
                Some("token 不回填（不落盘、不回显）；重新输入后按 Enter 连接".to_string());
        }
        state.refresh_lan();
        self.overlays.push(Overlay::Remote(state));
    }

    /// 滚轮滚动本页（`handle_workspace_scroll` 调用）。
    pub fn remote_scroll(&mut self, up: bool, step: usize) {
        self.with_remote(move |st| {
            st.scroll = if up {
                st.scroll.saturating_sub(step)
            } else {
                st.scroll.saturating_add(step).min(200)
            };
        });
    }

    /// 页面按键。返回 `true` = 已消费（与 `overlay_key` 的其它分支同约定）。
    pub(super) fn remote_overlay_key(&mut self, key: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyCode::*;
        let Some(Overlay::Remote(mut st)) = self.overlays.last().cloned() else {
            return false;
        };
        let consumed = match key.code {
            Esc => {
                if st.editing.is_some() {
                    st.editing = None;
                } else {
                    self.overlays.pop();
                }
                true
            }
            // 编辑态：把按键喂给缓冲，Enter 提交。
            _ if st.editing.is_some() => {
                self.remote_edit_key(&mut st, key);
                true
            }
            Tab | BackTab => {
                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    -1isize
                } else {
                    1
                };
                st.move_focus(delta);
                true
            }
            Up | Down if st.panel == 1 => {
                // 面板 1 的上下键归设备列表：字段靠 Tab 走，行靠上下走，两套游标不抢。
                let count = st.devices.len();
                if count > 0 {
                    let delta = if key.code == Up { -1isize } else { 1 };
                    st.device_sel = wrap_index(st.device_sel, count, delta);
                }
                true
            }
            Up | Down => {
                st.move_focus(if key.code == Up { -1 } else { 1 });
                true
            }
            PageUp | PageDown => {
                // 页面高度只有几十行（QR 约 40 行），软上限取 200：滚过头只会露出
                // 空白底，比在渲染层反算「总行数」再回传状态要诚实——那会让 draw
                // 反过来写状态。
                if key.code == PageUp {
                    st.scroll = st.scroll.saturating_sub(10);
                } else {
                    st.scroll = (st.scroll + 10).min(200);
                }
                true
            }
            Char('1') => {
                st.panel = 0;
                true
            }
            Char('2') => {
                st.panel = 1;
                true
            }
            Left | Right => {
                if st.focus() == RemoteField::Scope {
                    let delta = if key.code == Left { -1isize } else { 1 };
                    st.scope_index = wrap_index(st.scope_index, PAIR_SCOPES.len(), delta);
                }
                true
            }
            Enter => {
                self.remote_activate(&mut st);
                true
            }
            Char('r') => {
                self.remote_refresh_devices(&mut st);
                true
            }
            Char('x') => {
                self.remote_revoke_selected(&mut st);
                true
            }
            _ => false,
        };
        if consumed {
            self.replace_overlay(Overlay::Remote(st));
            // 状态行/票据倒计时都要立刻可见。
            self.force_redraw = true;
        }
        consumed
    }

    /// Esc 之外的一键提交入口：编辑态落到字段，非编辑态执行动作。
    fn remote_edit_key(&mut self, st: &mut RemoteState, key: KeyEvent) {
        use ratatui::crossterm::event::KeyCode::*;
        let Some(field) = st.editing else {
            return;
        };
        let buffer_len = st.buffer.len();
        match key.code {
            Enter => {
                let text: String = st.buffer.iter().collect();
                st.apply_edit(field, &text);
                st.editing = None;
            }
            Esc => st.editing = None,
            Backspace => {
                st.cursor = st.cursor.saturating_sub(1);
                if !st.buffer.is_empty() && st.cursor < buffer_len {
                    st.buffer.remove(st.cursor);
                } else if st.cursor == buffer_len && !st.buffer.is_empty() {
                    st.buffer.pop();
                }
            }
            Delete => {
                if st.cursor < st.buffer.len() {
                    st.buffer.remove(st.cursor);
                }
            }
            Left => st.cursor = st.cursor.saturating_sub(1),
            Right => st.cursor = (st.cursor + 1).min(st.buffer.len()),
            Home => st.cursor = 0,
            End => st.cursor = st.buffer.len(),
            Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                st.buffer.insert(st.cursor.min(st.buffer.len()), c);
                st.cursor += 1;
            }
            _ => {}
        }
    }

    /// Enter 在非编辑态的语义。
    fn remote_activate(&mut self, st: &mut RemoteState) {
        match st.focus() {
            RemoteField::BaseUrl | RemoteField::Token | RemoteField::DeviceName => {
                st.begin_edit();
            }
            RemoteField::Connect => self.remote_connect(st),
            RemoteField::GoLocal => self.remote_go_local(st),
            RemoteField::Scope => {
                st.scope_index = (st.scope_index + 1) % PAIR_SCOPES.len();
            }
            RemoteField::Issue => self.remote_issue_pairing(st),
            RemoteField::RefreshDevices => self.remote_refresh_devices(st),
        }
    }

    /// 直连远端：先本地校验（端口/scheme/token），再换 Client。
    fn remote_connect(&mut self, st: &mut RemoteState) {
        if st.busy {
            st.set_status("已有动作在途", false);
            return;
        }
        let target = match st.draft_target() {
            Ok(target) => target,
            Err(error) => {
                st.set_status(error, true);
                return;
            }
        };
        st.busy = true;
        st.set_status(format!("正在连接 {}…", target.base_url), false);
        self.runtime_reconnect_to(ConnectTarget::Remote(target));
    }

    /// 回到本地 daemon（恢复启动时的拉起标志）。
    fn remote_go_local(&mut self, st: &mut RemoteState) {
        if st.busy {
            st.set_status("已有动作在途", false);
            return;
        }
        let launch = match self.startup_target {
            ConnectTarget::Local {
                launch_daemon_if_missing,
            } => launch_daemon_if_missing,
            ConnectTarget::Remote(_) => true,
        };
        st.busy = true;
        st.set_status("正在回到本地 daemon…", false);
        self.runtime_reconnect_to(ConnectTarget::Local {
            launch_daemon_if_missing: launch,
        });
    }

    fn runtime_reconnect_to(&self, target: ConnectTarget) {
        let runtime = self.runtime.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let describe = target.describe();
            let result = runtime
                .reconnect_to(target)
                .await
                .map(|_| describe)
                .map_err(|e| e.to_string());
            let _ = tx.send(AppMsg::Action(ActionResult::RemoteReconnect { result }));
        });
    }

    /// 签发配对码：需要本机 daemon.json 里有局域网面，否则码里的 base_url 对手机无意义。
    fn remote_issue_pairing(&mut self, st: &mut RemoteState) {
        if st.busy {
            st.set_status("已有动作在途", false);
            return;
        }
        st.refresh_lan();
        // 一次 clone 把快照拿走：既不借着 `st` 再改 `st`，也保证「签发期间页面又
        // 变了」不会让载荷用到一半新一半旧的值。
        let Some(lan) = st.lan.clone() else {
            st.set_status("读不到 daemon.json：本机没有在跑的 daemon？", true);
            return;
        };
        let Some(base_url) = lan.lan_endpoint.clone() else {
            st.set_status(
                "daemon 没开局域网面，配对码无处可指。请在 daemon 侧运行 \
                 `qaqh-daemon server --bind <ip> --port <port>` 后回到本页重开。"
                    .to_string(),
                true,
            );
            return;
        };
        let name = clamp_device_name(&st.device_name);
        if name.is_empty() {
            st.set_status("先给设备起个名字（Enter 编辑「设备名」）", true);
            return;
        }
        let Some(scope) = PAIR_SCOPES.get(st.scope_index).copied() else {
            return;
        };
        st.busy = true;
        st.set_status("正在申请配对令牌…", false);
        self.remote_pending = Some(RemotePending {
            base_url,
            machine: host_name(),
            device_name: name.clone(),
            scope,
        });
        self.spawn_api(move |api, tx| async move {
            let result = api.issue_pairing_token(scope, &name, PAIR_PLATFORM).await;
            let _ = tx.send(AppMsg::Action(ActionResult::PairTicket(result)));
        });
    }

    fn remote_refresh_devices(&mut self, st: &mut RemoteState) {
        if st.busy {
            return;
        }
        st.busy = true;
        self.spawn_api(|api, tx| async move {
            let result = api.list_devices().await;
            let _ = tx.send(AppMsg::Action(ActionResult::Devices(result)));
        });
    }

    /// 两步吊销：第一次只置位，第二次才真发命令（与删除会话同一套纪律）。
    fn remote_revoke_selected(&mut self, st: &mut RemoteState) {
        let Some(device) = st.devices.get(st.device_sel) else {
            st.set_status("没有选中设备", true);
            return;
        };
        let device_id = device.device_id.clone();
        if st.revoke_armed.as_deref() != Some(device_id.as_str()) {
            st.revoke_armed = Some(device_id);
            st.set_status(
                format!("再按一次 x 确认吊销 {}（{}）", device.name, device.scope),
                false,
            );
            return;
        }
        st.revoke_armed = None;
        st.busy = true;
        self.spawn_api(move |api, tx| async move {
            let result = api.revoke_device(&device_id).await;
            let _ = tx.send(AppMsg::Action(ActionResult::DeviceRevoked {
                device_id: device_id.clone(),
                result,
            }));
        });
    }

    /// `/remote` 三类结果 + 重连结果的落地。
    pub(super) fn handle_remote_result(&mut self, ev: ActionResult) {
        match ev {
            ActionResult::RemoteReconnect { result } => {
                self.set_remote_busy(false);
                match result {
                    Ok(describe) => {
                        self.toast(NoticeLevel::Info, format!("已连接 {describe}"));
                        self.set_remote_status(format!("当前：{describe}"), false);
                    }
                    Err(error) => {
                        self.toast(NoticeLevel::Error, format!("重连失败：{error}"));
                        self.set_remote_status(format!("重连失败：{error}"), true);
                    }
                }
                // 换目标 = 所有流都作废：与手动重连同一套恢复路径（重新 attach +
                // 全量 bootstrap），否则页面上会是旧 daemon 的会话列表。
                if let Some(session_ids) = (!self.tabs.is_empty()).then(|| self.tabs.clone()) {
                    self.spawn_api(move |api, tx| async move {
                        for session_id in session_ids {
                            let attach = api
                                .send_command(
                                    Some(&session_id),
                                    RingingCommand::Control(ControlCommand::SessionResume {
                                        session_id: session_id.clone(),
                                    }),
                                    Default::default(),
                                )
                                .await;
                            if let Err(e) = attach {
                                let _ = tx.send(AppMsg::Action(ActionResult::CommandAck {
                                    session_id: Some(session_id.clone()),
                                    label: "resume",
                                    result: Err(e),
                                }));
                            }
                            let result = api.bootstrap(&session_id).await;
                            let client_session_id = api.v2_client_session_id().await;
                            let _ = tx.send(AppMsg::Action(ActionResult::Bootstrap {
                                session_id,
                                result,
                                client_session_id,
                            }));
                        }
                    });
                }
            }
            ActionResult::PairTicket(result) => {
                self.set_remote_busy(false);
                let pending = self.remote_pending.take();
                match (result, pending) {
                    (Ok(ticket), Some(pending)) => {
                        let payload = build_pair_payload(
                            &pending.base_url,
                            &ticket.pairing_token,
                            &ticket.tls_fp,
                            &pending.machine,
                        );
                        let expires_in = std::time::Duration::from_millis(ticket.expires_in_ms);
                        self.with_remote(|st| {
                            st.ticket = Some(PairTicket {
                                scope: pending.scope.as_str(),
                                device_name: pending.device_name,
                                payload,
                                expires_at: Instant::now() + expires_in,
                            });
                            st.set_status("配对码已生成（120 秒内有效），手机扫屏上的码", false);
                        });
                    }
                    (Ok(_), None) => {
                        // 签发成功但本地的载荷上下文丢了（切过页/重连过）：不能凭空拼
                        // 一个 base_url，直接丢弃并说明，免得画出一张指向错的码。
                        self.set_remote_status("配对令牌已签发，但缺少局域网地址，无法出码", true);
                    }
                    (Err(error), _) => {
                        self.set_remote_status(format!("pairing/tokens 失败：{error}"), true);
                    }
                }
            }
            ActionResult::Devices(result) => {
                self.set_remote_busy(false);
                match result {
                    Ok(devices) => {
                        let count = devices.len();
                        self.with_remote(move |st| {
                            st.devices = devices;
                            st.device_sel = st.device_sel.min(count.saturating_sub(1));
                            st.set_status(format!("设备 {count} 台"), false);
                        });
                    }
                    Err(error) => {
                        self.set_remote_status(format!("devices 失败：{error}"), true);
                    }
                }
            }
            ActionResult::DeviceRevoked { device_id, result } => {
                self.set_remote_busy(false);
                match result {
                    Ok(()) => {
                        self.with_remote(|st| {
                            st.devices.retain(|d| d.device_id != device_id);
                            st.set_status(format!("已吊销 {device_id}"), false);
                        });
                        // 吊销后重取列表：daemon 侧才是权威，本地剔除只是即时反馈。
                        self.remote_refresh_devices_fresh();
                    }
                    Err(error) => {
                        self.with_remote(|st| {
                            // 失败要把两步确认的置位**留着**吗？不留：用户已经执行过
                            // 一次确认，报错后再按 x 应当重新开始一轮确认。
                            st.revoke_armed = None;
                            st.set_status(format!("revoke 失败：{error}"), true);
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// 吊销成功后重取列表（不经过 busy 闸，避免与刚释放的标志抢顺序）。
    fn remote_refresh_devices_fresh(&mut self) {
        self.spawn_api(|api, tx| async move {
            let result = api.list_devices().await;
            let _ = tx.send(AppMsg::Action(ActionResult::Devices(result)));
        });
    }

    // ───────────────────────── 状态行的窄接口 ─────────────────────────
    //
    // 这几行代码反复出现「取出顶层 Remote overlay → 改 → 放回」。做成三个小助手，
    // 免得每个分支都手写一遍 `if let`，也保证不会漏掉 `replace_overlay`。

    fn with_remote<F: FnOnce(&mut RemoteState)>(&mut self, f: F) {
        if let Some(Overlay::Remote(mut st)) = self.overlays.last().cloned() {
            f(&mut st);
            self.replace_overlay(Overlay::Remote(st));
            self.force_redraw = true;
        }
    }

    fn set_remote_status(&mut self, text: impl Into<String>, is_error: bool) {
        let text = text.into();
        self.with_remote(move |st| st.set_status(text, is_error));
    }

    fn set_remote_busy(&mut self, busy: bool) {
        self.with_remote(move |st| st.busy = busy);
    }
}

/// 环形移动列表游标（`delta` 可为负）。字段游标的同一语义在
/// [`RemoteState::move_focus`] 里。
fn wrap_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RemoteTarget;

    #[test]
    fn wrap_index_wraps_both_directions() {
        assert_eq!(wrap_index(0, 4, -1), 3);
        assert_eq!(wrap_index(3, 4, 1), 0);
        assert_eq!(wrap_index(1, 0, 1), 0, "空列表不 panic");
    }

    #[test]
    fn scope_table_matches_labels() {
        assert_eq!(PAIR_SCOPES.len(), PAIR_SCOPE_LABELS.len());
        assert_eq!(PAIR_SCOPES[0].as_str(), "view");
        assert_eq!(PAIR_SCOPES[2].as_str(), "admin");
    }

    /// 打开页面时不能把已存 token 回填进草稿（本仓不持久化它），也不能让它出现在
    /// 任何可打印的字符串里。
    #[test]
    fn open_page_prefills_url_but_never_token() {
        let (mut app, _rx) = App::new_for_test();
        app.runtime = crate::runtime::Runtime::stub_for_test_with_target(ConnectTarget::Remote(
            RemoteTarget {
                base_url: "http://10.0.0.5:64413".into(),
                token: "supersecret".into(),
            },
        ));
        app.open_remote();
        let Some(Overlay::Remote(st)) = app.overlays.last() else {
            panic!("overlay 未打开");
        };
        assert_eq!(st.draft_url, "http://10.0.0.5:64413", "地址要预填");
        assert!(st.draft_token.is_empty(), "token 绝不回填");
        assert!(
            !format!("{st:?}").contains("supersecret"),
            "页面状态里不得出现 token 明文"
        );
    }

    /// 面板 1 只给「配对」相关的入口：没有绑定地址/端口的编辑行——daemon 要不要
    /// 暴露到局域网由 daemon 侧决定，页面里放一个改了也不生效的输入框是骗人。
    #[test]
    fn panel_one_offers_pairing_not_bind_editors() {
        let st = RemoteState {
            panel: 1,
            ..Default::default()
        };
        let fields = st.fields();
        assert_eq!(fields[0], RemoteField::DeviceName);
        assert!(
            !fields
                .iter()
                .any(|f| matches!(f, RemoteField::BaseUrl | RemoteField::Token)),
            "面板 1 不该出现连接类字段：{fields:?}"
        );
        assert_eq!(fields.len(), 4);
    }

    /// 待连的 URL 校验由 `RemoteTarget::new` 负责：这里只测「错在哪必须说清楚」，
    /// 尤其是 https 那条——它会被用户当成产品缺陷，而不是当成一条约束。
    #[test]
    fn connect_reports_the_reason_not_a_generic_failure() {
        let (mut app, _rx) = App::new_for_test();
        app.open_remote();
        app.with_remote(|st| {
            st.draft_url = "https://192.168.1.8:64413".into();
            st.draft_token = "t".into();
        });
        let Some(Overlay::Remote(st)) = app.overlays.last().cloned() else {
            panic!();
        };
        let error = st.draft_target().expect_err("https 必须被挡住");
        assert!(error.contains("https"), "{error}");
        assert!(error.contains("自签"), "{error}");
    }
}
