//! ActivityBar 活动区（§4.4）：main 与 composer 之间固定 1 行。
//!
//! **在 TranscriptCache 之外**：每帧独立重画（一行成本，无需 AnimSlot 机制）。
//! 内容优先级：`思考 marquee > 运行中工具（icon + name + 摘要）> 空闲`——
//! 多轮回合里思考与工具交替，本行永远回答「现在在干嘛」。
//!
//! - 思考行：无换行横向滚动（`anim::marquee` 复用，CJK 宽度纪律现成），
//!   遇换行丢上一行只显**最新一行**；前缀 `anim::thinking_glyph`（M1 预留的
//!   `AnimKind::Thinking` 语义在这里落地）。
//! - F3（`show_activity`）控制本行显隐；隐藏时布局回收该行（不占位）。
//! - 子代理观测（Ctrl+↑）同一套：`active_session` 即当前查看会话，
//!   观测谁就显示谁的活动，无需额外数据源。

use crate::app::App;
use crate::app::anim;
use qaqh_client::{TimelineBlockKind, TimelineToolState};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// 活动区占行数：F3 显隐 + 仅在会话视图中占行（首页隐藏）。
pub(crate) fn height(app: &App) -> u16 {
    u16::from(app.show_activity && !app.tabs.is_empty())
}

pub(crate) fn draw(f: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    let spans = activity_spans(app, area.width as usize, anim::frame_now());
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 活动行内容（纯函数，便于测试）。
pub(crate) fn activity_spans(app: &App, width: usize, frame: u64) -> Vec<Span<'static>> {
    use ratatui::text::Span as S;
    let Some(sess) = app.active_session() else {
        return Vec::new();
    };
    let Some(tid) = sess.timeline.running_turn_id() else {
        return vec![S::styled(" · 空闲".to_string(), crate::ui::theme::dim())];
    };
    let Some(turn) = sess.timeline.turns.iter().find(|t| t.turn_id == tid) else {
        return Vec::new();
    };

    // 单趟扫描：最后一个非空 reasoning 块的最新行 + 第一个 Running 工具。
    let mut thinking_last_line: Option<&str> = None;
    let mut running_tool: Option<(&str, Option<&String>)> = None;
    for r in &turn.rounds {
        for b in &r.blocks {
            match b.kind {
                TimelineBlockKind::Reasoning if !b.text.is_empty() => {
                    thinking_last_line = b.text.lines().last();
                }
                TimelineBlockKind::Tool
                    if b.tool
                        .as_ref()
                        .is_some_and(|tc| tc.state == TimelineToolState::Running)
                        && running_tool.is_none() =>
                {
                    running_tool = b
                        .tool
                        .as_ref()
                        .map(|tc| (tc.name.as_str(), tc.summary.as_ref()));
                }
                _ => {}
            }
        }
    }

    // 优先级 1：思考 marquee。
    if let Some(line) = thinking_last_line {
        let glyph = anim::thinking_glyph(frame);
        let budget = width.saturating_sub(glyph.chars().count() + 1).max(1);
        return vec![
            S::styled(format!("{glyph} "), crate::ui::theme::accent()),
            S::styled(anim::marquee(line, budget, frame), crate::ui::theme::dim()),
        ];
    }
    // 优先级 2：运行中工具。
    if let Some((name, summary)) = running_tool {
        let mut text = format!("{} {name}", crate::app::render_transcript::tool_icon(name));
        if let Some(s) = summary {
            text.push(' ');
            text.push_str(s);
        }
        return vec![S::styled(
            truncate_cols(text, width),
            crate::ui::theme::dim(),
        )];
    }
    // 优先级 3：空闲。
    vec![S::styled(" · 空闲".to_string(), crate::ui::theme::dim())]
}

/// 按显示宽度截断（活动区单行纪律；CJK 安全由逐列累加保证）。
fn truncate_cols(s: String, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut w = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::SessionState;
    use crate::app::timeline_model as tm;

    fn app_with_running_turn(build: impl FnOnce(&mut tm::TimelineModel)) -> App {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        let mut m = tm::TimelineModel::default();
        build(&mut m);
        let mut sess = SessionState::new("seed".into());
        sess.timeline = m;
        app.sessions.insert("seed".to_string(), sess);
        app
    }

    fn running_reasoning(m: &mut tm::TimelineModel, text: &str) {
        use qaqh_client::{TimelineEntry, TimelineEvent as E};
        let e = |seq, ev| TimelineEntry {
            timeline_seq: seq,
            turn_id: "t1".into(),
            round_num: Some(0),
            event: ev,
        };
        m.apply(&e(
            1,
            E::TurnOpened {
                user_text: "q".into(),
            },
        ));
        m.apply(&e(
            2,
            E::BlockOpened {
                block: qaqh_client::TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: qaqh_client::TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&e(
            3,
            E::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: text.into(),
            },
        ));
    }

    #[test]
    fn height_follows_f3_and_home() {
        let (mut app, _rx) = App::new_for_test();
        assert_eq!(height(&app), 0, "首页不占行");
        app.tabs.push("seed".into());
        app.show_activity = true;
        assert_eq!(height(&app), 1);
        app.show_activity = false;
        assert_eq!(height(&app), 0, "F3 隐藏时布局回收该行");
    }

    #[test]
    fn thinking_line_wins_and_shows_latest_line_only() {
        let app = app_with_running_turn(|m| {
            running_reasoning(m, "思考第一行\n思考最新行");
        });
        let spans = activity_spans(&app, 60, 3);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("思考最新行"), "应显示最新思考行：{text}");
        assert!(!text.contains("思考第一行"), "旧行必须被丢弃：{text}");
    }

    #[test]
    fn idle_line_when_no_running_turn() {
        let app = app_with_running_turn(|_| {});
        let spans = activity_spans(&app, 60, 3);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("空闲"), "{text}");
    }

    #[test]
    fn running_tool_line_when_no_thinking() {
        use qaqh_client::{TimelineEntry, TimelineEvent as E};
        let app = app_with_running_turn(|m| {
            let e = |seq, ev| TimelineEntry {
                timeline_seq: seq,
                turn_id: "t1".into(),
                round_num: Some(0),
                event: ev,
            };
            m.apply(&e(
                1,
                E::TurnOpened {
                    user_text: "q".into(),
                },
            ));
            m.apply(&e(
                2,
                E::BlockOpened {
                    block: qaqh_client::TimelineBlock {
                        block_id: "b2".into(),
                        block_order: 0,
                        kind: TimelineBlockKind::Tool,
                        state: qaqh_client::TimelineBlockState::Open,
                        text: String::new(),
                        tool: Some(qaqh_client::TimelineTool {
                            tool_call_id: "b2".into(),
                            name: "bash".into(),
                            state: qaqh_client::TimelineToolState::Running,
                            summary: Some("ls -la".into()),
                            args_json: None,
                            output: None,
                            diff: None,
                            progress: String::new(),
                            progress_bytes_total: 0,
                            progress_stream: None,
                            progress_truncated: false,
                            failure: None,
                            permission: None,
                            display: None,
                        }),
                    },
                },
            ));
        });
        let spans = activity_spans(&app, 60, 3);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("bash") && text.contains("ls -la"), "{text}");
    }
}
