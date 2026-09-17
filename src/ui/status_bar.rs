//! 底部状态栏：连接相位 / epoch / toast / 用量 / 时钟。

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{App, ConnPhase};
use crate::ui::theme;
use qaqh_client::NoticeLevel;

/// 流告警文案的显示预算（列）。
///
/// 前缀「 ⚠ ready·流告警」约 12 列、epoch 约 9 列，右侧还有用量/活动/时钟
/// （约 30 列）——固定截 30 列会在 80 列终端上把中间 toast 挤没。这里按区域宽度
/// 收缩：给右侧留 [`RIGHT_RESERVE`] 列，下限 6 列（还能认出是「有东西」），
/// 上限 30 列（再长也没有信息量）。
pub const RIGHT_RESERVE: usize = 58;

pub fn issue_budget(width: usize) -> usize {
    width.saturating_sub(RIGHT_RESERVE).clamp(6, 30)
}

/// epoch 只在 `Ready` 显示（纯函数，便于回归测试）。
///
/// `ReadyWithIssue` 恰恰表示「有流正在重连」，此时 `epoch` 极可能是重连前旧实例
/// 的值——显示它会给出「还是同一个 daemon」的错误暗示，与「相位可见性不许串味」
/// 冲突。`Opening` / `Lost` 同理（旧值或空值）。
pub fn shows_epoch(phase: &ConnPhase) -> bool {
    matches!(phase, ConnPhase::Ready)
}

/// 左侧连接指示（纯函数：`Ready` 相位下的流告警必须可见，这是回归点）。
///
/// 三个相位的语义各不相同，视觉上也不许混淆：
/// - `Ready`：连接健康。
/// - `ReadyWithIssue`：连接可用，但有流在自愈 → 给出告警文案，**不**给重连提示
///   （连接是好的，按 Ctrl+R 也只会被告知无需重连）。
/// - `Lost`：连不上 daemon → 给出重连入口 + 原因。
///
/// `issue_budget` 是原因文案的显示列上限（见 [`issue_budget`]）。
pub fn conn_spans(
    phase: &ConnPhase,
    conn_error: Option<&str>,
    issue_budget: usize,
) -> Vec<Span<'static>> {
    let issue_text = |err: Option<&str>, style| {
        err.map(|e| {
            Span::styled(
                format!(" {}", crate::app::truncate_str(e, issue_budget)),
                style,
            )
        })
    };
    match phase {
        ConnPhase::Ready => vec![Span::styled(" ● ready", theme::ok())],
        ConnPhase::ReadyWithIssue => {
            let mut spans = vec![Span::styled(" ⚠ ready·流告警", theme::warn())];
            spans.extend(issue_text(conn_error, theme::warn()));
            spans
        }
        ConnPhase::Opening => vec![Span::styled(" ◌ connecting", theme::warn())],
        ConnPhase::Lost => {
            let mut spans = vec![
                Span::styled(" ✗ lost", theme::err()),
                // T-03：失联时必须让用户看见重连入口，否则唯一手段是重启进程。
                Span::styled(" · Ctrl+R 重连", theme::warn()),
            ];
            spans.extend(issue_text(conn_error, theme::err()));
            spans
        }
    }
}

pub fn draw(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let width = area.width as usize;
    let mut left: Vec<Span> = conn_spans(
        &app.conn_phase,
        app.conn_error.as_deref(),
        issue_budget(width),
    );
    if shows_epoch(&app.conn_phase) {
        let ep = if app.epoch.len() > 8 {
            &app.epoch[..8]
        } else {
            &app.epoch
        };
        left.push(Span::styled(format!(" {ep}"), theme::dim()));
    }
    if !app.pending_creates.is_empty() {
        left.push(Span::styled(" · creating…", theme::dim()));
    }

    // 中间：最新 toast。
    let mut middle: Vec<Span> = Vec::new();
    if let Some(toast) = app.toasts.back() {
        let style = match toast.level {
            NoticeLevel::Info => theme::dim(),
            NoticeLevel::Warn => theme::warn(),
            NoticeLevel::Error => theme::err(),
        };
        let text = crate::app::truncate_str(&toast.text, width.saturating_sub(60).max(20));
        middle.push(Span::styled(format!(" {text}"), style));
    }

    // 右侧：用量 / 活动 / 时钟。
    let mut right: Vec<Span> = Vec::new();
    if let Some(sess) = app.active_session() {
        if let Some(usage) = sess.usage.as_ref() {
            right.push(Span::styled(
                format!(
                    " ↑{}k ↓{}k",
                    usage.prompt_tokens / 1000,
                    usage.completion_tokens / 1000
                ),
                theme::dim(),
            ));
            if let Some(limit) = sess.context_limit {
                let pct = if limit > 0 {
                    (usage.prompt_tokens as u64 * 100 / limit as u64).min(999)
                } else {
                    0
                };
                right.push(Span::styled(format!(" ({pct}%)"), theme::dim()));
            }
        }
        right.push(Span::styled(
            format!(" · {}", sess.activity_label()),
            theme::accent(),
        ));
        // B1 可观测：仅契约异常丢弃非零时展示（正常会话零噪声）。
        if let Some(dropped) = sess.timeline.dropped_summary() {
            right.push(Span::styled(format!(" · {dropped}"), theme::warn()));
        }
    }
    let now = chrono::Local::now().format("%H:%M");
    right.push(Span::styled(format!(" · {now} "), theme::dim()));

    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let mid_budget = width.saturating_sub(left_w + right_w);
    let mid_w: usize = middle.iter().map(|s| s.content.chars().count()).sum();
    if mid_w > mid_budget {
        middle.clear();
    }

    let mut spans = left;
    let used = spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>()
        + right_w
        + middle
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), Style::new()));
    }
    spans.extend(middle);
    spans.extend(right);

    f.render_widget(Line::from(spans), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{StreamIssues, reconcile_conn};
    use crate::runtime::StreamKey;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// 回归：`Ready` 相位下的流告警必须可见，且恢复后必须消失。
    ///
    /// 证伪方式：把 `ConnPhase::with_stream_issues` 改回「不动相位」（旧行为），
    /// 第一条断言立刻变红；把状态栏改回「只在 Lost 渲染 conn_error」同理。
    #[test]
    fn ready_phase_stream_issue_is_visible_and_clears_on_recovery() {
        let msg = "连接断开，3000ms 后重连".to_string();
        let stream = StreamKey::Channel(qaqh_client::Channel::Conversation);
        let mut issues = StreamIssues::default();

        // 旧行为：StreamIssue 只写 conn_error、相位停在 Ready → 状态栏什么都不显示。
        issues.raise(stream.clone(), msg.clone());
        let (phase, error) = reconcile_conn(&ConnPhase::Ready, &issues, None);
        assert_eq!(
            phase,
            ConnPhase::ReadyWithIssue,
            "Ready 收到流告警必须离开 Ready 相位"
        );
        let shown = text(&conn_spans(&phase, error.as_deref(), 30));
        assert!(
            shown.contains("连接断开"),
            "流告警文案必须在状态栏可见，实际：{shown:?}"
        );

        // 恢复：相位回 Ready，告警文案不再出现（不留幽灵）。
        issues.clear(&stream);
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::Ready);
        let cleared = text(&conn_spans(&phase, error.as_deref(), 30));
        assert!(
            !cleared.contains("连接断开"),
            "恢复后不得残留流告警，实际：{cleared:?}"
        );
    }

    /// 语义不许串味：`ReadyWithIssue` 不能长得像 `Lost`（否则用户会去重连一条好连接）。
    #[test]
    fn ready_with_issue_is_not_confused_with_lost() {
        let issue = text(&conn_spans(
            &ConnPhase::ReadyWithIssue,
            Some("流已关闭"),
            30,
        ));
        assert!(issue.contains("ready"), "实际：{issue:?}");
        assert!(
            !issue.contains("lost") && !issue.contains("重连"),
            "流告警不得暗示失联/重连，实际：{issue:?}"
        );

        let lost = text(&conn_spans(&ConnPhase::Lost, Some("与 daemon 失联"), 30));
        assert!(
            lost.contains("lost") && lost.contains("Ctrl+R"),
            "实际：{lost:?}"
        );
    }

    /// 相位映射：流告警只影响 `Ready` 家族，不动失联/连接中（可见性各归其位）。
    #[test]
    fn stream_alerts_only_map_the_ready_family() {
        assert_eq!(
            ConnPhase::Ready.with_stream_issues(true),
            ConnPhase::ReadyWithIssue
        );
        assert_eq!(
            ConnPhase::ReadyWithIssue.with_stream_issues(false),
            ConnPhase::Ready,
            "告警清空必须回到 Ready"
        );
        assert_eq!(
            ConnPhase::Lost.with_stream_issues(true),
            ConnPhase::Lost,
            "失联期间到达的流告警不该改变相位"
        );
        assert_eq!(
            ConnPhase::Lost.with_stream_issues(false),
            ConnPhase::Lost,
            "某条流重连成功不能把失联相位抹成正常"
        );
        assert_eq!(
            ConnPhase::Opening.with_stream_issues(true),
            ConnPhase::Opening
        );
    }

    /// epoch 只在 `Ready` 显示：告警态/失联/连接中的 epoch 可能是旧实例的值。
    ///
    /// 证伪方式：把 `shows_epoch` 改回 `matches!(Ready | ReadyWithIssue)`（本次审查
    /// 指出的旧写法）——第二条断言变红。
    #[test]
    fn epoch_is_only_shown_in_ready() {
        assert!(shows_epoch(&ConnPhase::Ready));
        assert!(
            !shows_epoch(&ConnPhase::ReadyWithIssue),
            "有流在重连时 epoch 可能是重连前的旧值，不能显示"
        );
        assert!(!shows_epoch(&ConnPhase::Opening));
        assert!(!shows_epoch(&ConnPhase::Lost));
    }

    /// 告警文案按区域宽度收缩（80 列终端上固定 30 列会把中间 toast 挤掉）。
    #[test]
    fn issue_budget_shrinks_on_narrow_terminals() {
        assert_eq!(issue_budget(200), 30, "宽终端封顶 30 列");
        assert_eq!(issue_budget(80), 22);
        assert_eq!(issue_budget(40), 6, "窄终端保底 6 列");
        assert!(issue_budget(100) >= issue_budget(70), "预算随宽度单调不减");
    }

    /// 证伪方式：把 `conn_spans` 里的截断改回硬编码 30——小预算下断言变红。
    #[test]
    fn issue_text_respects_the_budget() {
        let long = "连接断开，3000ms 后重连并且这条文案特别长长长长长长长长长长长长";
        let shown = text(&conn_spans(&ConnPhase::ReadyWithIssue, Some(long), 8));
        let tail = shown
            .strip_prefix(" ⚠ ready·流告警 ")
            .expect("前缀")
            .to_owned();
        assert!(
            tail.chars().count() <= 8,
            "告警文案必须按预算截断，实际 {} 列：{tail:?}",
            tail.chars().count()
        );
        assert!(shown.starts_with(" ⚠ ready·流告警"), "实际：{shown:?}");
    }
}
