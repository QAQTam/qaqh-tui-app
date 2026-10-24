//! 审计复现：断言期望的正确行为，当前版本预期失败；不接入默认测试。
use super::*;

fn app_with_session() -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
    let (mut app, rx) = App::new_for_test();
    app.tabs.push("audit-session".into());
    app.sessions.insert(
        "audit-session".into(),
        SessionState::new("audit-session".into()),
    );
    (app, rx)
}

#[test]
fn audit_remote_token_paste_must_not_reach_chat() {
    let (mut app, _rx) = app_with_session();
    let st = remote::RemoteState {
        editing: Some(remote::RemoteField::Token),
        ..Default::default()
    };
    app.overlays.push(Overlay::Remote(st));
    app.handle(AppMsg::Paste("audit-fake-bearer-token".into()));
    assert!(
        app.active_session().unwrap().composer.is_empty(),
        "Token 粘贴泄漏到聊天输入框"
    );
    let Some(Overlay::Remote(st)) = app.overlays.last() else {
        panic!()
    };
    assert_eq!(
        st.buffer.iter().collect::<String>(),
        "audit-fake-bearer-token"
    );
}

#[tokio::test]
async fn audit_settings_ack_must_preserve_edits_after_save() {
    let (mut app, _rx) = App::new_for_test();
    app.settings_saving = true;
    // 第一笔保存 maxTokens=2048 在途时，用户又编辑为 4096。
    let mut st = SettingsState::default();
    st.draft.max_tokens = Some(4096);
    app.overlays.push(Overlay::Settings(st));
    let saved = ConfigDto {
        max_tokens: 2048,
        ..Default::default()
    };
    app.handle(AppMsg::Action(ActionResult::ConfigWrite {
        label: "设置",
        result: Ok(serde_json::to_value(saved).unwrap()),
    }));
    let Some(Overlay::Settings(st)) = app.overlays.last() else {
        panic!()
    };
    assert_eq!(
        st.draft.max_tokens,
        Some(4096),
        "保存回包清掉了之后新写的草稿"
    );
}

#[tokio::test]
async fn audit_failed_send_must_retain_user_text() {
    let (mut app, mut rx) = app_with_session();
    app.active_session_mut()
        .unwrap()
        .composer
        .insert_str("请保留这段未送达的输入");
    app.send_message();
    // stub 没有连接：确定未送达，避免把网络超时的不确定结果当作可自动重发。
    let result = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle(result);
    assert_eq!(
        app.active_session().unwrap().composer.value(),
        "请保留这段未送达的输入"
    );
}

#[tokio::test]
async fn audit_rejected_control_must_not_overwrite_chat_draft() {
    let (mut app, _rx) = app_with_session();
    app.active_session_mut()
        .unwrap()
        .composer
        .insert_str("用户正在编辑的草稿");
    app.handle(AppMsg::Action(ActionResult::CommandAck {
        session_id: Some("audit-session".into()),
        label: "取消",
        result: Ok(qaqh_client::ClientV2CommandAck {
            command_id: "audit-command".into(),
            status: qaqh_client::RingingCommandAckStatus::Rejected,
            code: Some("not_driver".into()),
            message: Some("read only".into()),
            retry_after_ms: None,
            existing: None,
        }),
    }));
    assert_eq!(
        app.active_session().unwrap().composer.value(),
        "用户正在编辑的草稿"
    );
}

#[tokio::test]
async fn audit_failed_ask_response_must_remain_retryable() {
    let (mut app, mut rx) = app_with_session();
    let mut panel = session::AskPanel::new(
        "audit-interaction".into(),
        "audit-turn".into(),
        qaqh_client::AskMode::Single,
        vec![qaqh_client::DomainAskQuestion {
            id: "q1".into(),
            question: "继续？".into(),
            options: vec!["是".into()],
            allow_custom: false,
        }],
    );
    panel.select_option(0, 0);
    app.active_session_mut().unwrap().pending_ask = Some(panel);
    app.submit_ask();
    let result = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle(result);
    assert!(
        app.active_session().unwrap().pending_ask.is_some(),
        "确定发送失败后 ask 仍被永久下架"
    );
    assert!(!app.suppressed_interactions.contains("audit-interaction"));
}

#[test]
fn audit_exec_progress_fragments_must_append_without_cr() {
    let mut text = String::new();
    timeline_model::apply_bash_progress(&mut text, "hel");
    timeline_model::apply_bash_progress(&mut text, "lo\n");
    assert_eq!(text, "hello\n");
}

fn turn() -> timeline_model::Turn {
    timeline_model::Turn {
        turn_id: "audit-turn".into(),
        turn_index: Some(1),
        started_at_ms: None,
        user_text: "测试问题".into(),
        state: qaqh_client::TimelineTurnState::Completed,
        failure: None,
        sealed: true,
        offloaded: false,
        thinking: Default::default(),
        rounds: vec![],
    }
}

fn rendered_turn(turn: timeline_model::Turn) -> String {
    let mut model = timeline_model::TimelineModel::default();
    model.turns.push(turn);
    let theme = crate::theme::Theme::resolve(
        crate::theme::ThemeKind::QaqhNight,
        crate::theme::ColorSupport::TrueColor,
    );
    crate::ui::v2::adapter::fullscreen_sources(&model, &HashSet::new(), &HashSet::new())
        .iter()
        .flat_map(|source| {
            crate::ui::v2::transcript::render_block(&source.materialize(), 80, &theme)
        })
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn audit_turn_failure_must_be_visible() {
    let mut t = turn();
    t.state = qaqh_client::TimelineTurnState::Failed;
    t.failure = Some(qaqh_client::TimelineFailure {
        code: "quota_exceeded".into(),
        message: "quota exceeded".into(),
    });
    let text = rendered_turn(t);
    assert!(
        text.contains("quota exceeded") || text.contains("quota_exceeded"),
        "失败原因消失：{text}"
    );
}

#[test]
fn audit_offloaded_turn_must_not_look_complete() {
    let mut t = turn();
    t.offloaded = true;
    let text = rendered_turn(t);
    assert!(
        text.contains("offload")
            || text.contains("预览")
            || text.contains("归档")
            || text.contains("截断"),
        "offload 状态未展示：{text}"
    );
}

fn rendered_exec_progress(progress: &str, truncated: bool) -> String {
    let wire: qaqh_client::TimelineTool = serde_json::from_value(serde_json::json!({
        "tool_call_id": "audit-tool", "name": "exec", "state": "running",
        "progress": progress, "progress_truncated": truncated,
    }))
    .unwrap();
    let mut t = turn();
    t.sealed = false;
    t.state = qaqh_client::TimelineTurnState::Running;
    t.rounds.push(timeline_model::Round {
        round_num: 0,
        sealed: false,
        is_final: false,
        blocks: vec![timeline_model::Block {
            block_id: "audit-block".into(),
            block_order: 0,
            kind: qaqh_client::TimelineBlockKind::Tool,
            state: qaqh_client::TimelineBlockState::Open,
            text: String::new(),
            tool: Some(wire.into()),
            reasoning_kind: None,
            last_fragment: 0,
            rev: 1,
        }],
    });
    rendered_turn(t)
}

#[test]
fn audit_progress_truncation_must_be_visible() {
    let text = rendered_exec_progress("only retained tail", true);
    assert!(
        text.contains("截断") || text.contains("丢弃"),
        "progress_truncated 未传给视图：{text}"
    );
}

#[test]
fn audit_collapsed_tool_preview_must_bound_visual_rows() {
    let text = rendered_exec_progress(&"x".repeat(6000), false);
    assert!(
        text.lines().count() <= 10,
        "未展开工具卡却占了 {} 屏幕行",
        text.lines().count()
    );
}

#[test]
fn audit_remote_tls_endpoint_must_be_accepted() {
    assert!(
        crate::runtime::RemoteTarget::new("https://192.0.2.10:64413", "audit-fake-token").is_ok()
    );
}

#[test]
fn audit_browser_gate_must_reject_windows_shell_metacharacters() {
    // 只测试门禁，不启动浏览器，也不执行命令。Windows 分支仍需原生环境复测。
    assert!(!browser::is_supported(
        "https://example.invalid/&echo.AUDIT"
    ));
}

#[cfg(unix)]
#[test]
fn audit_pager_temp_file_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("qaqh-audit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = pager::write_temp_file(&root, "仅供审计的假数据").unwrap();
    let permissions = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(
        permissions & 0o077,
        0,
        "pager 文件允许组/其他用户读取：{permissions:o}"
    );
}
