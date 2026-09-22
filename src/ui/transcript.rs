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
    let height = area.height as usize;

    // 分段缓存由 `App::ensure_render_caches` 在每帧前维护；这里只取视窗
    // （O(可见)，不扫全量）。取不到（未维护 / 宽度不一致）时现场全量渲一次兜底。
    //
    // 滚动几何与缓存共用 `ui::viewport_top`，避免两处各算一份而错位。
    let fresh: Vec<crate::app::render_line::RenderLine>;
    let (total, top, visible_lines, anim_slots, unrendered): (
        usize,
        usize,
        Vec<&crate::app::render_line::RenderLine>,
        Vec<crate::app::render::ViewportSlot>,
        usize,
    ) = if let Some(bc) = sess.block_cache.as_ref().filter(|c| c.width == width) {
        // M1 块级缓存（T8 接线后为唯一来源）：行窗口与动画槽位同源。
        //
        // ⚠ 视口必须取 `refresh` 当时用的那一份（`bc.viewport`），**不能自己重算**：
        // refresh 会把估算高度换成精确高度、总行数随之改变，自己重算就会落到另一个
        // 窗口上（issue #33：窗口压在未渲染块 → debug 直接 panic、release 静默空屏）。
        // 高度不一致（同帧内不该发生）才退回重算——宁可晚一帧收敛，也不取错窗口。
        let total = bc.total_lines();
        let (top, height) = match bc.viewport {
            Some((t, h)) if h == height => (t, height),
            _ => (
                crate::ui::viewport_top(total, height, sess.scroll.follow, sess.scroll.offset),
                height,
            ),
        };
        (
            total,
            top,
            bc.window(top, height),
            bc.visible_slots(top, height),
            bc.unrendered_lines(top, height),
        )
    } else {
        // 兑底：缓存未就绪（首帧 / 宽度突变同一帧）→ 现场全量渲一次。
        fresh = crate::app::render_transcript::render_transcript_with_opts(sess, width);
        let total = fresh.len();
        let top = crate::ui::viewport_top(total, height, sess.scroll.follow, sess.scroll.offset);
        (
            total,
            top,
            fresh.iter().skip(top).take(height).collect(),
            Vec::new(),
            0,
        )
    };

    let mut visible: Vec<Line> = visible_lines
        .into_iter()
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

    // B1「丢弃必须可见」：release 下 `window()` 里的 `debug_assert` 会被编译掉，未渲染段
    // 既不产出行也不推进 `skip` → 实测「请求 30 行取到 0 行」的**静默空屏**（issue #33）。
    // 正常路径 `unrendered == 0`；>0 时显式提示，绝不无声少行。
    if unrendered > 0 {
        visible.push(Line::from(format!(
            "⛔ 视口内有 {unrendered} 行未渲染（内部几何不一致；复现条件见 issue #33）"
        )));
    }

    f.render_widget(Paragraph::new(visible), area);

    // 动画出带（plan §3.3 / 锁 8）：块级缓存行内只有占位空格，这里按当前帧
    // 覆盖字形。旧 SegmentCache 路径字形已烘焙（无槽位）→ 零开销直通。
    if !anim_slots.is_empty() {
        crate::app::render::apply_anim_slots(f.buffer_mut(), area, &anim_slots);
    }

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
