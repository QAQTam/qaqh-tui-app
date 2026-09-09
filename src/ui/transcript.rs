//! transcript 视图：读取渲染缓存（App 在每帧前统一重建）+ 精确滚动 + 滚动条。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::app::App;
use crate::app::render_line::{RenderStyle, SpanStyle};
use crate::ui::theme;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(seed) = app.view_seed() else {
        return;
    };
    let Some(sess) = app.sessions.get(&seed) else {
        return;
    };

    let width = area.width.saturating_sub(1); // 右侧滚动条留 1 列
    // 只借用 IR，不做全量深拷贝：每帧成本 O(可视行数) 而非 O(全量 IR)。
    // 未命中缓存时现场渲染一次（streaming/compacting/首帧），同样只转换可视窗口。
    let fresh: Vec<crate::app::render_line::RenderLine>;
    let lines: &[crate::app::render_line::RenderLine] = match &sess.rendered {
        Some(cached) if cached.width == width => &cached.lines,
        _ => {
            fresh = crate::app::render_transcript::render_transcript_with_opts(
                sess,
                width,
                app.show_reasoning,
            );
            &fresh
        }
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

/// 会话信息行（转成 ratatui）。观测子代理时展示子代理横幅。
pub fn draw_session_info(f: &mut Frame, app: &App, area: Rect) {
    if app.inspecting()
        && let Some(banner) = render_subagent_banner(app, area.width)
    {
        f.render_widget(Paragraph::new(banner), area);
        return;
    }
    let Some(sess) = app.view_session() else {
        return;
    };
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

/// 子代理观测横幅：名称 / 状态 / seed 前缀 / 在父会话子代理中的序位 + 按键提示。
fn render_subagent_banner(app: &App, width: u16) -> Option<Vec<Line<'static>>> {
    let inspect = app.inspect.clone()?;
    // 嵌套观测：直属父可能也是子代理；非嵌套时直属父即活动标签会话。
    let parent_seed = app
        .subagent_parent(&inspect)
        .or_else(|| app.active_seed())?;
    let parent = app.sessions.get(&parent_seed)?;
    let nested = !app.tabs.contains(&parent_seed);
    let entry = parent
        .subagents
        .iter()
        .enumerate()
        .find(|(_, e)| e.seed.as_deref() == Some(inspect.as_str()))?;
    let (idx, entry) = entry;
    let total = parent.subagents.iter().filter(|e| e.seed.is_some()).count();

    let short_seed: String = inspect.chars().take(8).collect();
    let live = app
        .sessions
        .get(&inspect)
        .is_some_and(|s| s.streaming.is_some());
    let state_label = if live && entry.state == crate::app::subagent::SubagentState::Running {
        "running…".to_string()
    } else {
        entry.state.label().to_string()
    };

    let hint = " Ctrl+↑/↓ 切换 · Esc 返回 ";
    // 嵌套时直属父是子代理：取其观测名（从更上层父的条目里查）。
    let scope = if nested {
        let parent_name = parent
            .subagents
            .iter()
            .find(|e| e.seed.as_deref() == Some(parent_seed.as_str()))
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "subagent".to_string());
        format!("{parent_name} › ")
    } else {
        String::new()
    };
    let left = format!(
        " ↳ 子代理 {}{} · {} · {} · {}/{} ",
        scope,
        entry.name,
        state_label,
        short_seed,
        idx + 1,
        total.max(1),
    );
    let left_w = left.chars().count();
    let hint_w = hint.chars().count();
    let pad = (width as usize).saturating_sub(left_w + hint_w);

    let mut spans = vec![Span::styled(left, theme::active_tab())];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    if width as usize > left_w {
        spans.push(Span::styled(hint, theme::dim()));
    }
    Some(vec![Line::from(spans)])
}
