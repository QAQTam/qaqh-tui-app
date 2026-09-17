//! 底部状态栏：连接相位 / epoch / toast / 用量 / 时钟。

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{App, ConnPhase};
use crate::ui::theme;
use qaqh_client::NoticeLevel;

// ── 原因文案的显示预算 ──────────────────────────────────────────────
//
// 两个相位分设预算：它们**不是**同一类信息。
//
// 下面的两个 *_PREFIX_RESERVE 是「该相位下左端除原因文案外的显示列上界」，
// 按最坏情况取（含 ` · creating…` 的 12 列），**不是**精确值：
// - `ReadyWithIssue` 前缀「 ⚠ ready·流告警」12 列 + 原因前空格 1 列 + creating… 12 列
//   = 25；该相位**不显示 epoch**（见 [`shows_epoch`]）。
// - `Lost` 前缀「 ✗ lost · Ctrl+R 重连」19 列 + 1 + 12 = 32。
// 右侧宽度用 `draw()` 实测的 `right_w`（用量/百分比/活动标签/丢弃计数/时钟，
// 8~70 列，`dropped_summary` 带中文时最宽），不再猜一个固定预留常量。

/// `ReadyWithIssue` 下左端除原因文案外的列数上界（最坏情况，含 creating…）。
pub const READY_ISSUE_PREFIX_RESERVE: usize = 25;
/// `Lost` 下左端除原因文案外的列数上界（最坏情况，含 creating…）。
pub const LOST_PREFIX_RESERVE: usize = 32;

/// 流告警（`ReadyWithIssue`）原因文案的显示预算。
///
/// 告警文案可以短——它只是「某条流在自愈」的旁注，下限 6 列够认出「有东西」，
/// 上限 30 列（再长也没有信息量）。
pub fn issue_budget(width: usize, right_w: usize) -> usize {
    width
        .saturating_sub(right_w + READY_ISSUE_PREFIX_RESERVE)
        .clamp(6, 30)
}

/// 失联（`Lost`）原因文案的显示预算。
///
/// **下限刻意抬高**（20 列）：失联原因是本 issue 最需要可诊断的信息——T-03 的
/// 重连入口就靠它给用户上下文。若与告警文案共用 6 列下限，80 列终端 + 右侧较宽
/// （长 `dropped_summary`）时用户只能看到 6 列，等于没有原因。代价是极窄终端下
/// 左端可能超出区域宽度、挤掉中间 toast——这是刻意的取舍：失联时诊断信息优先。
pub fn lost_issue_budget(width: usize, right_w: usize) -> usize {
    width
        .saturating_sub(right_w + LOST_PREFIX_RESERVE)
        .clamp(20, 40)
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
///   （连接是好的，按 Ctrl+R 也只会被告知无需重连）。文案是「最近一条流告警」，
///   可能已经过期（那条流刚重连成功、或 `Lost` 正在路上）——它只描述「此刻还有流
///   在告警」这个集合非空的事实，集合清空即消失，不做逐条时效追踪。
/// - `Lost`：连不上 daemon → 给出重连入口 + 原因。
///
/// 原因文案的显示列上限**按相位取**：告警用 [`issue_budget`]（可以短），失联用
/// [`lost_issue_budget`]（下限 20 列，见那里的取舍）。调用方只给区域宽度与右侧
/// 实测宽度，选预算这件事留在函数里，免得两个调用点各写一套。
pub fn conn_spans(
    phase: &ConnPhase,
    conn_error: Option<&str>,
    width: usize,
    right_w: usize,
) -> Vec<Span<'static>> {
    let budget = match phase {
        ConnPhase::Lost => lost_issue_budget(width, right_w),
        _ => issue_budget(width, right_w),
    };
    let issue_text = |err: Option<&str>, style| {
        err.map(|e| Span::styled(format!(" {}", crate::app::truncate_str(e, budget)), style))
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

    // 右侧：用量 / 活动 / 时钟。**先算它**——左侧流告警文案的预算要扣掉右侧的
    // 实际宽度（用量+百分比+活动+丢弃计数+时钟，实测 8~70 列）。
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

    // 左侧：连接指示（原因文案的预算按相位取，宽度用右侧实测值，不靠常量猜）。
    let mut left: Vec<Span> =
        conn_spans(&app.conn_phase, app.conn_error.as_deref(), width, right_w);
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
        let shown = text(&conn_spans(&phase, error.as_deref(), 80, 8));
        assert!(
            shown.contains("连接断开"),
            "流告警文案必须在状态栏可见，实际：{shown:?}"
        );

        // 恢复：相位回 Ready，告警文案不再出现（不留幽灵）。
        issues.clear(&stream);
        let (phase, error) = reconcile_conn(&phase, &issues, error);
        assert_eq!(phase, ConnPhase::Ready);
        let cleared = text(&conn_spans(&phase, error.as_deref(), 80, 8));
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
            80,
            8,
        ));
        assert!(issue.contains("ready"), "实际：{issue:?}");
        assert!(
            !issue.contains("lost") && !issue.contains("重连"),
            "流告警不得暗示失联/重连，实际：{issue:?}"
        );

        let lost = text(&conn_spans(&ConnPhase::Lost, Some("与 daemon 失联"), 80, 8));
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

    /// 告警文案预算按区域宽度与**实测**右侧宽度收缩。
    ///
    /// 证伪方式：改回「固定预留常量」的旧写法（`width - 58` 夹 [6,30]）——
    /// 右侧很窄（8 列）时旧写法给 22，本测试要求 30；右侧很宽（54 列）时旧写法
    /// 仍给 22，本测试要求 6。两个方向都会变红。
    ///
    /// 期望值按 [`READY_ISSUE_PREFIX_RESERVE`] 算：`width - right_w - 25` 夹 [6,30]。
    #[test]
    fn issue_budget_follows_measured_right_width() {
        assert_eq!(issue_budget(200, 10), 30, "宽终端封顶 30 列");
        assert_eq!(issue_budget(80, 8), 30, "右侧很窄时预算不被常量压掉");
        assert_eq!(issue_budget(80, 54), 6, "右侧很宽时保底 6 列");
        assert_eq!(issue_budget(60, 25), 10, "60-25-25");
        assert_eq!(issue_budget(40, 25), 6);
        assert!(
            issue_budget(100, 10) >= issue_budget(70, 10),
            "预算随宽度单调不减"
        );
        assert!(
            issue_budget(80, 70) <= issue_budget(80, 10),
            "右侧越宽，留给告警文案的越少"
        );
    }

    /// 证伪方式：把 `conn_spans` 里的截断改回硬编码 30——小预算下断言变红。
    #[test]
    fn issue_text_respects_the_budget() {
        let long = "连接断开，3000ms 后重连并且这条文案特别长长长长长长长长长长长长";
        // 小预算场景：60 列 - 右侧 27 列 - 前缀 25 列 = 8。
        assert_eq!(issue_budget(60, 27), 8);
        let shown = text(&conn_spans(&ConnPhase::ReadyWithIssue, Some(long), 60, 27));
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

    /// **阻断 2（PR #20 第二轮复审）**：失联原因不能被压到 6 列。
    ///
    /// 80 列终端 + 右侧较宽（长 `dropped_summary`，实测可到 54~70 列）时，若 `Lost`
    /// 与流告警共用同一个 6 列下限，用户只看到 6 列——而失联原因是本 issue 最需要
    /// 可诊断的信息（T-03 的重连入口靠它给上下文）。
    ///
    /// 证伪方式：让 `Lost` 也走 `issue_budget`（返工前的写法）→ 下面
    /// `lost_issue_budget(80, 54) == 20` 与「原因至少 20 列」两条同时变红。
    #[test]
    fn lost_reason_keeps_a_readable_budget_when_the_right_side_is_wide() {
        let long = "与 daemon 失联（20s 内无任何频道连接）——按 R 重连";
        // 同一场景：告警可以短（下限 6），失联必须有 20 列。
        assert_eq!(issue_budget(80, 54), 6);
        assert_eq!(lost_issue_budget(80, 54), 20);

        let lost = text(&conn_spans(&ConnPhase::Lost, Some(long), 80, 54));
        let tail = lost
            .strip_prefix(" ✗ lost · Ctrl+R 重连 ")
            .expect("失联前缀")
            .to_owned();
        assert!(
            tail.chars().count() >= 20,
            "失联原因必须保有可诊断的宽度，实际 {} 列：{tail:?}",
            tail.chars().count()
        );

        // 宽终端上给得更多（上限 40）；窄终端保底 20。
        assert_eq!(lost_issue_budget(200, 10), 40);
        assert_eq!(lost_issue_budget(40, 10), 20);
        assert!(
            lost_issue_budget(80, 54) > issue_budget(80, 54),
            "失联的下限必须严格高于流告警"
        );
    }
}
