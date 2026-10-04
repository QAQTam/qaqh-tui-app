//! 左侧常驻会话栏（Agent 视图）。
//!
//! 数据源是 [`App::sidebar_rows`]（daemon 启动后被激活过 / 正在跑的会话）；
//! 渲染遵循鼠标优先的按钮规范（spec §5）：无框线，hover / pressed / 选中
//! 全部用整行背景色表达。整列另铺 `surface.light` 面板底色，与右侧**不铺底色**
//! 的消息区形成竖向分界。Working 会话的状态 glyph 走星芒动画
//! （200ms/帧，由 Tick 驱动重绘）。
//!
//! 命中目标与 `dispatch_pointer_action` 共用 [`AgentTarget::SidebarRow`] 的
//! 语义下标；下标必须来自同一次 [`App::sidebar_rows`]，绘制与点击各算一次
//! 但事实源相同。

use ratatui::Frame;
use ratatui::crossterm::event::MouseButton;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use qaqh_client::DomainActivityState;

use crate::app::anim;
use crate::app::App;
use crate::theme::Theme;
use crate::ui::v2::button::ButtonVisual;
use crate::ui::v2::fullscreen::FullscreenState;
use crate::ui::v2::hit::{
    AgentTarget, HitMapBuilder, PointerTarget, VisualAnchor, anchor_region, z,
};
use std::time::Instant;

/// 侧栏宽度（状态 glyph + 标题）。
pub const RAIL_WIDTH: u16 = 22;
/// 终端总宽低于该值时侧栏自动隐藏，给正文让路。
pub const RAIL_MIN_TOTAL_WIDTH: u16 = 90;

/// 本帧侧栏宽度；0 = 不显示。
#[must_use]
pub fn rail_width(total_width: u16) -> u16 {
    if total_width >= RAIL_MIN_TOTAL_WIDTH {
        RAIL_WIDTH
    } else {
        0
    }
}

/// 会话行的状态 glyph 与前景色。
///
/// 星芒（Working/Starting）每帧变化；WaitingUser=黄、Failed=红、
/// Idle=绿、Disconnected=灰。无活动记录但在跑的会话退化为空心圆。
fn status_glyph(
    state: Option<DomainActivityState>,
    running: bool,
    frame: u64,
    theme: &Theme,
) -> (&'static str, Style) {
    let running_style = Style::new().fg(theme.accent.running);
    match (state, running) {
        (Some(DomainActivityState::Working | DomainActivityState::Starting), _) => {
            (anim::claude_spinner_glyph(frame), running_style)
        }
        (Some(DomainActivityState::WaitingUser), _) => ("◆", Style::new().fg(theme.semantic.warning)),
        (Some(DomainActivityState::Failed), _) => ("✖", Style::new().fg(theme.accent.error)),
        (Some(DomainActivityState::Idle), _) => ("●", Style::new().fg(theme.accent.success)),
        (Some(DomainActivityState::Disconnected), _) => ("○", Style::new().fg(theme.text.dim)),
        (None, true) => ("○", Style::new().fg(theme.text.dim)),
        (None, false) => ("·", Style::new().fg(theme.text.dim)),
    }
}

/// 侧栏选中切换的余晖动画状态（渲染侧持有，App 不感知）。
///
/// 选中行切换后的短窗口内，旧行保留一帧 hover 底色——终端行高恒为 1，
/// 做不了逐像素滑动，"旧行余晖 + 新行点亮"在两三个 Tick 帧内读作高亮滑走。
#[derive(Debug, Default)]
pub struct SidebarAnim {
    /// 上一帧的选中行（检测切换用）。
    active_row: Option<usize>,
    /// 余晖：旧行下标 + 起始时刻。
    slide_from: Option<(usize, Instant)>,
}

/// 余晖窗口 ≈2 个 200ms Tick 帧。
const AFTERGLOW_MS: u128 = 360;

impl SidebarAnim {
    /// 检测选中行切换并推进动画；每帧绘制前调用一次。
    fn advance(&mut self, active_index: Option<usize>) {
        if active_index != self.active_row {
            if let (Some(prev), Some(_)) = (self.active_row, active_index) {
                self.slide_from = Some((prev, Instant::now()));
            }
            self.active_row = active_index;
        }
        if self
            .slide_from
            .is_some_and(|(_, at)| at.elapsed().as_millis() >= AFTERGLOW_MS)
        {
            self.slide_from = None;
        }
    }

    fn afterglow_row(&self) -> Option<usize> {
        self.slide_from.map(|(from, _)| from)
    }
}

/// 绘制侧栏并登记行命中区。
pub fn draw(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    pointer: &FullscreenState,
    anim: &mut SidebarAnim,
    hit_map: &mut HitMapBuilder,
) {
    let rows = app.sidebar_rows();
    anim.advance(rows.iter().position(|row| row.is_active));
    let afterglow = anim.afterglow_row();
    let width = usize::from(area.width);
    let frame_no = anim::frame_now();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " 无活跃会话",
            Style::new().fg(theme.text.dim),
        )));
    }
    for (index, row) in rows.iter().enumerate().take(usize::from(area.height)) {
        let y = area.y.saturating_add(index as u16);
        let visual = ButtonVisual::derive(
            true,
            row.is_active,
            pointer.sidebar_hover == Some(index),
            pointer.sidebar_pressed == Some(index),
        );
        let (glyph, glyph_style) = status_glyph(row.activity, row.running, frame_no, theme);
        // ▣ = 已在 tab 集里（点击即切换、零加载）；与 workspace 列表同词汇。
        let open_marker = if row.is_open { "▣" } else { " " };
        let title = fit_width(&row.title, width.saturating_sub(5));
        let mut line = Line::from(vec![
            Span::styled(format!(" {glyph} "), glyph_style),
            Span::styled(
                format!("{open_marker} "),
                Style::new().fg(if row.is_open {
                    theme.accent.assistant
                } else {
                    theme.text.dim
                }),
            ),
            Span::styled(title, Style::new().fg(theme.text.primary)),
        ])
        .patch_style(visual.surface_style(theme, theme.chrome.selection));
        // 余晖：刚失去选中的行保留 hover 底色（hover/pressed/选中优先级更高）。
        if afterglow == Some(index)
            && !row.is_active
            && pointer.sidebar_hover != Some(index)
            && pointer.sidebar_pressed != Some(index)
        {
            line = line.patch_style(Style::new().bg(theme.surface.hover));
        }
        if let Some(region) = anchor_region(
            Rect::new(area.x, y, area.width, 1),
            area,
            PointerTarget::Agent(AgentTarget::SidebarRow(index)),
            MouseButton::Left,
            true,
            z::AGENT_SIDEBAR_ROW,
            // 锚点对准状态 glyph 格（行首是空格，过不了 strict 探针）。
            VisualAnchor::non_empty(Position::new(area.x.saturating_add(1), y)),
        ) {
            hit_map.push(region);
        }
        lines.push(line);
    }
    // 整列铺 `surface.light`：会话栏与右侧消息区因此有一条明确的竖向分界
    // （消息区刻意不铺底色，保持终端背景）。`Paragraph::style` 会先
    // `buf.set_style(area, ..)`，空行与行尾自动被填满，无需手动补空格。
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(theme.surface.light)),
        area,
    );
}

/// 按显示宽度截断（宽字符感知）；与 workspace 列表同一规则。
fn fit_width(value: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;
    if value.width() <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in value.chars() {
        let width = ch.width().unwrap_or(0);
        if used.saturating_add(width) > max_width.saturating_sub(1) {
            break;
        }
        used += width;
        result.push(ch);
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    fn style_fg(style: Style) -> Option<ratatui::style::Color> {
        style.fg
    }

    #[test]
    fn status_glyph_follows_state_colors_from_the_spec() {
        let theme = theme();
        // Working = 星芒流动（前景 running 色）
        let (working_glyph, working_style) =
            status_glyph(Some(DomainActivityState::Working), true, 0, &theme);
        assert_eq!(anim::claude_spinner_glyph(0), working_glyph);
        assert_eq!(style_fg(working_style), Some(theme.accent.running));
        // 星芒随帧推进（"星光流动"）
        let (frame0, _) = status_glyph(Some(DomainActivityState::Working), true, 0, &theme);
        let (frame3, _) = status_glyph(Some(DomainActivityState::Working), true, 3, &theme);
        assert_ne!(frame0, frame3);

        let (waiting, waiting_style) =
            status_glyph(Some(DomainActivityState::WaitingUser), true, 0, &theme);
        assert_eq!(waiting, "◆");
        assert_eq!(style_fg(waiting_style), Some(theme.semantic.warning));

        let (failed, failed_style) =
            status_glyph(Some(DomainActivityState::Failed), true, 0, &theme);
        assert_eq!(failed, "✖");
        assert_eq!(style_fg(failed_style), Some(theme.accent.error));

        let (idle, idle_style) = status_glyph(Some(DomainActivityState::Idle), true, 0, &theme);
        assert_eq!(idle, "●");
        assert_eq!(style_fg(idle_style), Some(theme.accent.success));

        let (dead, dead_style) =
            status_glyph(Some(DomainActivityState::Disconnected), false, 0, &theme);
        assert_eq!(dead, "○");
        assert_eq!(style_fg(dead_style), Some(theme.text.dim));

        // 无活动记录但在跑：空心圆兜底，不出现"·"（那是未激活的占位）。
        let (running_unknown, _) = status_glyph(None, true, 0, &theme);
        assert_eq!(running_unknown, "○");
    }

    #[test]
    fn fit_width_truncates_cjk_within_budget() {
        assert_eq!(fit_width("标题很长的会话", 100), "标题很长的会话");
        let truncated = fit_width("标题很长的会话名称", 8);
        // 显示宽度不超预算，且以省略号收尾。
        use unicode_width::UnicodeWidthStr;
        assert!(truncated.width() <= 8);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn rail_width_hides_on_narrow_terminals() {
        assert_eq!(rail_width(89), 0);
        assert_eq!(rail_width(90), RAIL_WIDTH);
    }

    /// 会话栏整列铺 `surface.light`，与右侧不铺底色的消息区形成竖向分界。
    #[test]
    fn sidebar_rail_paints_panel_background() {
        use crate::app::session::SessionState;
        use crate::ui::v2::hit::{FrameId, HitMapBuilder};
        use crate::ui::v2::route::ScreenRoute;
        use qaqh_client::{SessionListEntry, SessionMeta};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, _rx) = crate::app::App::new_for_test();
        for id in ["s-1", "s-2"] {
            app.tabs.push(id.into());
            app.sessions.insert(id.into(), SessionState::new(id.into()));
            let mut entry = SessionListEntry {
                meta: SessionMeta {
                    session_id: id.into(),
                    title: Some(id.into()),
                    ..SessionMeta::default()
                },
                running: true,
                workspace_id: None,
            };
            entry.meta.title = Some(id.into());
            app.session_list_cache.push(entry);
        }
        app.active = 0;
        let theme = theme();
        let rail_area = Rect::new(0, 0, RAIL_WIDTH, 8);
        let mut anim = SidebarAnim::default();
        let mut hit_map = HitMapBuilder::new(
            FrameId::new(1),
            ScreenRoute::Agent,
            ratatui::layout::Size::new(100, 8),
            0,
        );
        let pointer = FullscreenState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("terminal");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &app,
                    rail_area,
                    &theme,
                    &pointer,
                    &mut anim,
                    &mut hit_map,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(1, 0)].bg,
            theme.chrome.selection,
            "选中行仍是 selection 底色（面板底色被覆盖）"
        );
        assert_eq!(
            buffer[(1, 1)].bg,
            theme.surface.light,
            "非选中行露出会话栏面板底色"
        );
        assert_eq!(
            buffer[(RAIL_WIDTH - 1, 7)].bg,
            theme.surface.light,
            "列表没占满时，空行也要铺满整列"
        );
    }

    #[test]
    fn sidebar_paints_afterglow_when_selection_moves() {
        use crate::app::session::SessionState;
        use crate::ui::v2::hit::{FrameId, HitMapBuilder};
        use crate::ui::v2::route::ScreenRoute;
        use qaqh_client::{SessionListEntry, SessionMeta};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, _rx) = crate::app::App::new_for_test();
        for id in ["s-1", "s-2"] {
            app.tabs.push(id.into());
            app.sessions
                .insert(id.into(), SessionState::new(id.into()));
            let mut entry = SessionListEntry {
                meta: SessionMeta {
                    session_id: id.into(),
                    title: Some(id.into()),
                    ..SessionMeta::default()
                },
                running: true,
                workspace_id: None,
            };
            entry.meta.title = Some(id.into());
            app.session_list_cache.push(entry);
        }
        app.active = 0;
        let theme = theme();
        let rail_area = Rect::new(0, 0, RAIL_WIDTH, 8);
        let mut anim = SidebarAnim::default();
        let mut hit_map = HitMapBuilder::new(
            FrameId::new(1),
            ScreenRoute::Agent,
            ratatui::layout::Size::new(100, 8),
            0,
        );
        let pointer = FullscreenState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("terminal");

        // 帧一：s-1 选中，无动画。
        terminal
            .draw(|frame| draw(frame, &app, rail_area, &theme, &pointer, &mut anim, &mut hit_map))
            .unwrap();
        assert_eq!(anim.active_row, Some(0));
        assert!(anim.slide_from.is_none());

        // 帧二：切到 s-2 → 旧行 s-1 保留余晖底色，新行点亮 selection。
        app.active = 1;
        terminal
            .draw(|frame| draw(frame, &app, rail_area, &theme, &pointer, &mut anim, &mut hit_map))
            .unwrap();
        assert_eq!(anim.active_row, Some(1));
        assert_eq!(anim.slide_from.map(|(from, _)| from), Some(0));
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(1, 0)].bg,
            theme.surface.hover,
            "刚失去选中的行应保留余晖底色"
        );
        assert_eq!(
            buffer[(1, 1)].bg,
            theme.chrome.selection,
            "新选中行是 selection 底色"
        );
    }
}
