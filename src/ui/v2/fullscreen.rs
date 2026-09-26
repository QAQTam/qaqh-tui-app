//! V2 全屏 shell 的交互状态与浮层按钮。
//!
//! 全屏模式没有终端原生 scrollback，滚动与可点击元素都由 App/UI 自己承担。
//! 本模块只保存鼠标的**语义状态**，并提供渲染与命中测试共用的几何，避免出现
//! “按钮画在这里、点击命中在那里”。

use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;
use crate::ui::v2::button::{ButtonState, ButtonVisual};
use crate::ui::v2::scrollbar::ScrollbarMetrics;

const BUTTON_WIDTH: u16 = 22;
const BUTTON_HEIGHT: u16 = 3;
const BUTTON_BOTTOM_MARGIN: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FullscreenState {
    pub back_to_latest_hover: bool,
    pub back_to_latest_pressed: bool,
}

/// 助手消息上的上下文动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAction {
    CopyMarkdown,
    Retry,
    Fork,
    UndoFromHere,
}

impl MessageAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::CopyMarkdown => "复制成 Markdown",
            Self::Retry => "重新回答",
            Self::Fork => "从这里继续",
            Self::UndoFromHere => "撤销此对话",
        }
    }

    pub const fn glyph(self) -> &'static str {
        match self {
            Self::CopyMarkdown => "⧉",
            Self::Retry => "↻",
            Self::Fork => "⑂",
            Self::UndoFromHere => "↶",
        }
    }

    /// Retry/Fork 要等后端原子语义；当前复制与 undo 已可用。
    pub const fn enabled(self) -> bool {
        matches!(self, Self::CopyMarkdown | Self::UndoFromHere)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    const fn actions(self) -> &'static [MessageAction] {
        const ASSISTANT: &[MessageAction] = &[
            MessageAction::CopyMarkdown,
            MessageAction::Retry,
            MessageAction::Fork,
        ];
        const USER: &[MessageAction] = &[MessageAction::UndoFromHere];
        match self {
            Self::Assistant => ASSISTANT,
            Self::User => USER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMenu {
    pub turn_id: String,
    pub block_id: String,
    pub role: MessageRole,
    pub selected: usize,
    pub hover: Option<usize>,
    pub pressed: Option<usize>,
    pub anchor: Position,
}

impl MessageMenu {
    pub fn new(turn_id: String, block_id: String, role: MessageRole, anchor: Position) -> Self {
        Self {
            turn_id,
            block_id,
            role,
            selected: 0,
            hover: None,
            pressed: None,
            anchor,
        }
    }

    pub fn actions(&self) -> &'static [MessageAction] {
        self.role.actions()
    }

    pub fn selected_action(&self) -> MessageAction {
        self.actions()[self.selected.min(self.actions().len() - 1)]
    }

    pub fn move_selection(&mut self, delta: isize) {
        let actions = self.actions();
        let count = actions.len();
        let mut index = self.selected;
        for _ in 0..count {
            index = if delta < 0 {
                (index + count - 1) % count
            } else {
                (index + 1) % count
            };
            if actions[index].enabled() {
                self.selected = index;
                return;
            }
        }
    }

    pub fn activate(&self) -> Option<MessageAction> {
        let action = self.selected_action();
        action.enabled().then_some(action)
    }
}

/// “回到最新消息”按钮的矩形。渲染与 HitMap 登记共用。
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

/// 「回到最新」按钮的样式：Idle 时用 secondary 前景（它是浮层，不该抢视线）。
///
/// 交互底色一律走全仓统一的 [`ButtonVisual::surface_style`]（spec §5）。
fn back_to_latest_style(visual: ButtonVisual, theme: &Theme) -> Style {
    let style = visual.surface_style(theme, Color::Reset);
    if visual.state() == ButtonState::Idle {
        style.fg(theme.text.secondary)
    } else {
        style
    }
}

pub fn draw_back_to_latest(frame: &mut Frame, area: Rect, state: FullscreenState, theme: &Theme) {
    let Some(rect) = back_to_latest_rect(area) else {
        return;
    };
    let style = back_to_latest_style(
        ButtonVisual::derive(
            true,
            false,
            state.back_to_latest_hover,
            state.back_to_latest_pressed,
        ),
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

const MENU_WIDTH: u16 = 34;

/// 菜单外框矩形。渲染、命中与 HitMap 登记必须共用。
pub fn message_menu_rect(area: Rect, menu: &MessageMenu) -> Rect {
    let height = menu.actions().len() as u16 + 2;
    let width = MENU_WIDTH.min(area.width.max(1)).max(8);
    let max_x = area.x.saturating_add(area.width.saturating_sub(width));
    let max_y = area
        .y
        .saturating_add(area.height.saturating_sub(height.min(area.height)));
    let x = menu.anchor.x.saturating_add(1).min(max_x).max(area.x);
    let y = menu.anchor.y.saturating_add(1).min(max_y).max(area.y);
    Rect::new(x, y, width, height.min(area.height))
}

/// 菜单第 `index` 个动作的整行矩形（不含外框）。
///
/// 返回 `None` 表示该行没画出来（动作越界或菜单太小）。渲染与 HitMap 登记
/// 都用它，所以"画出来的行"和"能点的行"必然一一对应。
pub fn message_menu_row_rect(area: Rect, menu: &MessageMenu, index: usize) -> Option<Rect> {
    let rect = message_menu_rect(area, menu);
    let inner = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    if inner.is_empty() || index >= usize::from(inner.height) || index >= menu.actions().len() {
        return None;
    }
    Some(Rect::new(
        inner.x,
        inner.y.saturating_add(index as u16),
        inner.width,
        1,
    ))
}

pub fn draw_message_menu(frame: &mut Frame, area: Rect, menu: &MessageMenu, theme: &Theme) {
    let rect = message_menu_rect(area, menu);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.chrome.border))
            .style(Style::new().bg(theme.surface.base).fg(theme.text.primary))
            .title(Span::styled(
                " 消息操作 ",
                Style::new().fg(theme.text.secondary),
            )),
        rect,
    );

    for (index, action) in menu.actions().iter().enumerate() {
        let Some(row_rect) = message_menu_row_rect(area, menu, index) else {
            continue;
        };
        let hovered = menu.hover == Some(index);
        let pressed = menu.pressed == Some(index);
        let selected = menu.selected == index;
        // 优先级与配色全部来自统一按钮模型（spec §5.2）：
        // Disabled > Pressed > Hovered > Focused > Idle。
        let visual = ButtonVisual::derive(action.enabled(), selected, hovered, pressed);
        let style = match visual.state() {
            ButtonState::Disabled => visual.foreground(theme),
            ButtonState::Pressed | ButtonState::Hovered => visual
                .surface_style(theme, Color::Reset)
                .fg(theme.text.bright),
            // 键盘选中但没悬停 → Focused 档，用和 Workspace 列表一致的选中底色。
            ButtonState::Idle if visual.focused() => visual
                .surface_style(theme, theme.chrome.selection)
                .fg(theme.text.bright),
            ButtonState::Idle => visual.foreground(theme),
        };
        let marker = if selected && action.enabled() {
            "▸"
        } else {
            " "
        };
        let suffix = if action.enabled() {
            ""
        } else {
            "  · 待后端"
        };
        let line = Line::from(vec![
            Span::styled(format!(" {marker} {} ", action.glyph()), style),
            Span::styled(action.label(), style),
            Span::styled(suffix, Style::new().fg(theme.text.dim)),
        ]);
        frame.render_widget(Paragraph::new(line).style(style), row_rect);
    }
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
    if area.width == 0 || area.height == 0 {
        return;
    }
    let track = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(1)),
        area.y,
        1,
        area.height,
    );
    let Some(metrics) = ScrollbarMetrics::new(track, total, viewport_height, follow, offset) else {
        return;
    };

    let track_style = Style::new().fg(theme.chrome.border);
    let thumb_style = Style::new().fg(theme.text.secondary);
    let lines: Vec<Line<'static>> = (0..usize::from(metrics.track.height))
        .map(|row| {
            if row >= metrics.thumb_top && row < metrics.thumb_top + metrics.thumb_height {
                Line::from(Span::styled("┃", thumb_style))
            } else {
                Line::from(Span::styled("│", track_style))
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), track);
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
    fn tiny_area_has_no_button() {
        assert!(back_to_latest_rect(Rect::new(0, 0, 7, 10)).is_none());
        assert!(back_to_latest_rect(Rect::new(0, 0, 20, 2)).is_none());
    }

    #[test]
    fn scrollbar_thumb_stays_inside_track() {
        let track = Rect::new(0, 0, 1, 20);
        let top = ScrollbarMetrics::new(track, 1_000, 20, false, 980).expect("metrics");
        assert_eq!(top.thumb_top, 0);
        assert_eq!(top.thumb_height, 1);

        let bottom = ScrollbarMetrics::new(track, 1_000, 20, false, 0).expect("metrics");
        assert_eq!(bottom.thumb_top, 19);
        assert_eq!(bottom.thumb_height, 1);

        let middle = ScrollbarMetrics::new(track, 100, 50, false, 25).expect("metrics");
        assert_eq!(middle.thumb_height, 10);
        assert_eq!(middle.thumb_top, 5);
    }

    #[test]
    fn message_menu_selection_skips_disabled_actions() {
        let mut menu = MessageMenu::new(
            "turn".into(),
            "block".into(),
            MessageRole::Assistant,
            Position::new(0, 0),
        );
        assert_eq!(menu.selected_action(), MessageAction::CopyMarkdown);
        menu.move_selection(1);
        assert_eq!(menu.selected_action(), MessageAction::CopyMarkdown);
        menu.move_selection(-1);
        assert_eq!(menu.selected_action(), MessageAction::CopyMarkdown);
    }

    #[test]
    fn user_menu_only_exposes_undo() {
        let menu = MessageMenu::new(
            "turn".into(),
            "turn:user".into(),
            MessageRole::User,
            Position::new(0, 0),
        );
        assert_eq!(menu.actions(), &[MessageAction::UndoFromHere]);
        assert_eq!(menu.selected_action(), MessageAction::UndoFromHere);
    }
}
