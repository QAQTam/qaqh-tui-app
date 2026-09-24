//! V2 全屏 shell 的交互状态与浮层按钮。
//!
//! 全屏模式没有终端原生 scrollback，滚动与可点击元素都由 App/UI 自己承担。
//! 本模块只保存鼠标的**语义状态**，并提供渲染与命中测试共用的几何，避免出现
//! “按钮画在这里、点击命中在那里”。

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;

const BUTTON_WIDTH: u16 = 22;
const BUTTON_HEIGHT: u16 = 3;
const BUTTON_BOTTOM_MARGIN: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FullscreenState {
    pub back_to_latest_hover: bool,
    pub back_to_latest_pressed: bool,
}

impl FullscreenState {
    pub fn clear_pointer(&mut self) {
        self.back_to_latest_hover = false;
        self.back_to_latest_pressed = false;
    }
}

/// 当前全屏 Agent 视口里可命中的浮层目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    BackToLatest,
}

/// “回到最新消息”按钮的矩形。渲染与 hit-test 必须共用。
pub fn back_to_latest_rect(area: Rect) -> Option<Rect> {
    if area.width < 8 || area.height < BUTTON_HEIGHT {
        return None;
    }
    let width = BUTTON_WIDTH.min(area.width.saturating_sub(2)).max(8);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let max_y = area
        .y
        .saturating_add(area.height)
        .saturating_sub(BUTTON_HEIGHT);
    let y = max_y.saturating_sub(BUTTON_BOTTOM_MARGIN).max(area.y);
    Some(Rect::new(x, y, width, BUTTON_HEIGHT))
}

pub fn hit_test(area: Rect, column: u16, row: u16) -> Option<Hit> {
    let rect = back_to_latest_rect(area)?;
    let inside = column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height);
    inside.then_some(Hit::BackToLatest)
}

fn button_style(hovered: bool, pressed: bool, theme: &Theme) -> Style {
    let surface = |color: Color| {
        if color == Color::Reset {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(color)
        }
    };
    if pressed {
        surface(theme.surface.highlight).add_modifier(Modifier::BOLD)
    } else if hovered {
        surface(theme.surface.hover)
    } else {
        Style::new().fg(theme.text.secondary)
    }
}

pub fn draw_back_to_latest(frame: &mut Frame, area: Rect, state: FullscreenState, theme: &Theme) {
    let Some(rect) = back_to_latest_rect(area) else {
        return;
    };
    let style = button_style(
        state.back_to_latest_hover,
        state.back_to_latest_pressed,
        theme,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .style(style),
        rect,
    );

    let label = if state.back_to_latest_pressed {
        " ↓ 松开回到最新 "
    } else {
        " ↓ 回到最新消息 "
    };
    let inner = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, style))).alignment(Alignment::Center),
        inner,
    );
}

/// 右侧滚动条。无内容可滚时完全隐藏，不占视觉注意力。
pub fn draw_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    viewport_height: usize,
    follow: bool,
    offset: usize,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 || total <= viewport_height {
        return;
    }

    let height = usize::from(area.height);
    let top = crate::ui::viewport_top(total, viewport_height, follow, offset);
    let (thumb_top, thumb_height) = scrollbar_thumb(total, viewport_height, height, top);

    let track_style = Style::new().fg(theme.chrome.border);
    let thumb_style = Style::new().fg(theme.text.secondary);
    let lines: Vec<Line<'static>> = (0..height)
        .map(|row| {
            if row >= thumb_top && row < thumb_top.saturating_add(thumb_height) {
                Line::from(Span::styled("┃", thumb_style))
            } else {
                Line::from(Span::styled("│", track_style))
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            area.x.saturating_add(area.width.saturating_sub(1)),
            area.y,
            1,
            area.height,
        ),
    );
}

fn scrollbar_thumb(
    total: usize,
    viewport_height: usize,
    track_height: usize,
    top: usize,
) -> (usize, usize) {
    if track_height == 0 || total == 0 || viewport_height == 0 {
        return (0, 0);
    }
    let thumb_height = track_height
        .saturating_mul(viewport_height)
        .checked_div(total)
        .unwrap_or(1)
        .clamp(1, track_height);
    let max_top = total.saturating_sub(viewport_height);
    let thumb_top = if max_top == 0 {
        0
    } else {
        top.saturating_mul(track_height.saturating_sub(thumb_height))
            .checked_div(max_top)
            .unwrap_or(0)
    }
    .min(track_height.saturating_sub(thumb_height));
    (thumb_top, thumb_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_is_centered_above_body_bottom() {
        let area = Rect::new(0, 0, 80, 24);
        let rect = back_to_latest_rect(area).expect("button");
        assert_eq!(rect.width, BUTTON_WIDTH);
        assert_eq!(rect.height, BUTTON_HEIGHT);
        assert_eq!(rect.x, (80 - BUTTON_WIDTH) / 2);
        assert_eq!(rect.y, 20);
    }

    #[test]
    fn hit_test_matches_rendered_rect() {
        let area = Rect::new(2, 3, 60, 20);
        let rect = back_to_latest_rect(area).expect("button");
        assert_eq!(hit_test(area, rect.x, rect.y), Some(Hit::BackToLatest));
        assert_eq!(
            hit_test(
                area,
                rect.x.saturating_add(rect.width),
                rect.y.saturating_add(rect.height)
            ),
            None
        );
    }

    #[test]
    fn tiny_area_has_no_button() {
        assert!(back_to_latest_rect(Rect::new(0, 0, 7, 10)).is_none());
        assert!(back_to_latest_rect(Rect::new(0, 0, 20, 2)).is_none());
    }

    #[test]
    fn scrollbar_thumb_stays_inside_track() {
        let (top, height) = scrollbar_thumb(1_000, 20, 20, 0);
        assert_eq!(top, 0);
        assert_eq!(height, 1);

        let (top, height) = scrollbar_thumb(1_000, 20, 20, 980);
        assert_eq!(top, 19);
        assert_eq!(height, 1);

        let (top, height) = scrollbar_thumb(100, 50, 20, 25);
        assert_eq!(height, 10);
        assert_eq!(top, 5);
    }
}
