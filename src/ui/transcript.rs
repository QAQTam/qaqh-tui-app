//! transcript 视图：读取渲染缓存（App 在每帧前统一重建）+ 精确滚动 + 滚动条。

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::app::render_line::{RenderStyle, SpanStyle};
use crate::app::App;
use crate::ui::theme;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(seed) = app.active_seed() else { return };
    let Some(sess) = app.sessions.get(&seed) else { return };

    let width = area.width.saturating_sub(1); // 右侧滚动条留 1 列
    let lines: Vec<crate::app::render_line::RenderLine> = match &sess.rendered {
        Some(cached) if cached.width == width => cached.lines.clone(),
        _ => crate::app::render_transcript::render_transcript_with_opts(sess, width, app.show_reasoning),
    };

    let total = lines.len();
    let height = area.height as usize;
    let bottom_offset = if sess.scroll.follow {
        0
    } else {
        sess.scroll.offset.min(total.saturating_sub(height))
    };
    let top = total.saturating_sub(height).saturating_sub(bottom_offset);

    let visible: Vec<Line> = lines
        .iter()
        .skip(top)
        .take(height)
        .map(|rl| {
            let spans: Vec<Span> = rl
                .spans
                .iter()
                .map(|s| {
                    let style = match &s.style {
                        RenderStyle::Semantic(ss) => theme::style_of(*ss),
                        RenderStyle::Direct(st) => *st,
                    };
                    Span::styled(s.text.clone(), style)
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(visible), area);

    if total > height {
        let mut sb = ScrollbarState::new(total.saturating_sub(height))
            .position(top)
            .viewport_content_length(height);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::new().fg(ratatui::style::Color::DarkGray)),
            area,
            &mut sb,
        );
    }
}

/// 会话信息行（转成 ratatui）。
pub fn draw_session_info(f: &mut Frame, app: &App, area: Rect) {
    let Some(sess) = app.active_session() else { return };
    let lines = crate::app::render_transcript::render_session_info(sess, area.width);
    let rat: Vec<Line> = lines
        .iter()
        .map(|rl| {
            let spans: Vec<Span> = rl
                .spans
                .iter()
                .map(|s| {
                    let style = match &s.style {
                        RenderStyle::Semantic(ss) => theme::style_of(*ss),
                        RenderStyle::Direct(st) => *st,
                    };
                    Span::styled(s.text.clone(), style)
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(rat), area);
}

#[allow(dead_code)]
fn _keep_spanstyle(_: SpanStyle) {}
