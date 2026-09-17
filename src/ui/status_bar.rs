//! 底部状态栏：连接相位 / epoch / toast / 用量 / 时钟。

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{App, ConnPhase};
use crate::ui::theme;
use qaqh_client::NoticeLevel;

/// 左侧连接指示（纯函数：`Ready` 相位下的流告警必须可见，这是回归点）。
///
/// 三个相位的语义各不相同，视觉上也不许混淆：
/// - `Ready`：连接健康。
/// - `ReadyWithIssue`：连接可用，但有流在自愈 → 给出告警文案，**不**给重连提示
///   （连接是好的，按 Ctrl+R 也只会被告知无需重连）。
/// - `Lost`：连不上 daemon → 给出重连入口 + 原因。
pub fn conn_spans(phase: &ConnPhase, conn_error: Option<&str>) -> Vec<Span<'static>> {
    let issue_text = |err: Option<&str>| {
        err.map(|e| {
            Span::styled(
                format!(" {}", crate::app::truncate_str(e, 30)),
                theme::warn(),
            )
        })
    };
    match phase {
        ConnPhase::Ready => vec![Span::styled(" ● ready", theme::ok())],
        ConnPhase::ReadyWithIssue => {
            let mut spans = vec![Span::styled(" ⚠ ready·流告警", theme::warn())];
            spans.extend(issue_text(conn_error));
            spans
        }
        ConnPhase::Opening => vec![Span::styled(" ◌ connecting", theme::warn())],
        ConnPhase::Lost => {
            let mut spans = vec![
                Span::styled(" ✗ lost", theme::err()),
                // T-03：失联时必须让用户看见重连入口，否则唯一手段是重启进程。
                Span::styled(" · Ctrl+R 重连", theme::warn()),
            ];
            if let Some(err) = conn_error {
                spans.push(Span::styled(
                    format!(" {}", crate::app::truncate_str(err, 30)),
                    theme::err(),
                ));
            }
            spans
        }
    }
}

pub fn draw(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let width = area.width as usize;
    let mut left: Vec<Span> = conn_spans(&app.conn_phase, app.conn_error.as_deref());
    // epoch 只对「连接可用」的相位有意义（连接中/失联时它要么空要么是旧的）。
    if app.conn_phase.is_ready() {
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

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// 回归：`Ready` 相位下的流告警必须可见，且恢复后必须消失。
    ///
    /// 证伪方式：把 `ConnPhase::with_stream_issue` 改回「不动相位」（旧行为），
    /// 第一条断言立刻变红；把状态栏改回「只在 Lost 渲染 conn_error」同理。
    #[test]
    fn ready_phase_stream_issue_is_visible_and_clears_on_recovery() {
        let msg = "连接断开，3000ms 后重连".to_string();

        // 旧行为：StreamIssue 只写 conn_error、相位停在 Ready → 状态栏什么都不显示。
        let phase = ConnPhase::Ready.with_stream_issue();
        assert_eq!(
            phase,
            ConnPhase::ReadyWithIssue,
            "Ready 收到流告警必须离开 Ready 相位"
        );
        let shown = text(&conn_spans(&phase, Some(&msg)));
        assert!(
            shown.contains("连接断开"),
            "流告警文案必须在状态栏可见，实际：{shown:?}"
        );

        // 恢复：相位回 Ready，告警文案不再出现（不留幽灵）。
        let phase = phase.with_stream_recovered();
        assert_eq!(phase, ConnPhase::Ready);
        let cleared = text(&conn_spans(&phase, None));
        assert!(
            !cleared.contains("连接断开"),
            "恢复后不得残留流告警，实际：{cleared:?}"
        );
    }

    /// 语义不许串味：`ReadyWithIssue` 不能长得像 `Lost`（否则用户会去重连一条好连接）。
    #[test]
    fn ready_with_issue_is_not_confused_with_lost() {
        let issue = text(&conn_spans(&ConnPhase::ReadyWithIssue, Some("流已关闭")));
        assert!(issue.contains("ready"), "实际：{issue:?}");
        assert!(
            !issue.contains("lost") && !issue.contains("重连"),
            "流告警不得暗示失联/重连，实际：{issue:?}"
        );

        let lost = text(&conn_spans(&ConnPhase::Lost, Some("与 daemon 失联")));
        assert!(
            lost.contains("lost") && lost.contains("Ctrl+R"),
            "实际：{lost:?}"
        );
    }

    /// 流恢复只清流告警相位，不动失联/连接中（相位可见性各归其位）。
    #[test]
    fn stream_recovery_only_clears_the_issue_phase() {
        assert_eq!(
            ConnPhase::Lost.with_stream_recovered(),
            ConnPhase::Lost,
            "某条流重连成功不能把失联相位抹成正常"
        );
        assert_eq!(
            ConnPhase::Opening.with_stream_recovered(),
            ConnPhase::Opening
        );
        assert_eq!(
            ConnPhase::Lost.with_stream_issue(),
            ConnPhase::Lost,
            "失联期间到达的流告警不该改变相位"
        );
        assert_eq!(ConnPhase::Opening.with_stream_issue(), ConnPhase::Opening);
        assert!(ConnPhase::Ready.is_ready() && ConnPhase::ReadyWithIssue.is_ready());
        assert!(!ConnPhase::Lost.is_ready() && !ConnPhase::Opening.is_ready());
    }
}
