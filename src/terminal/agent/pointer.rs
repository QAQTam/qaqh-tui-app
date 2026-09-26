//! 帧绑定指针状态机。
//!
//! 事件只读取 `FrameHitMap`；状态机本身不查 App、不重算布局。P0-A 先覆盖
//! hover / pressed / release / capture 的核心转移，P0-C 再把批次失效和
//! presented frame 发布接进终端循环。

use ratatui::crossterm::event::MouseButton;

use crate::ui::v2::hit::{FrameHitMap, HitRegion, PointerTarget, ScrollbarPart};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvent {
    Moved {
        column: u16,
        row: u16,
    },
    Down {
        button: MouseButton,
        column: u16,
        row: u16,
    },
    Up {
        button: MouseButton,
        column: u16,
        row: u16,
    },
    Drag {
        button: MouseButton,
        column: u16,
        row: u16,
    },
    ScrollUp {
        column: u16,
        row: u16,
    },
    ScrollDown {
        column: u16,
        row: u16,
    },
    Leave,
    RouteChanged,
    Resized,
    FocusLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerAction {
    None,
    Redraw,
    Pressed {
        target: PointerTarget,
        column: u16,
        row: u16,
    },
    Activate {
        target: PointerTarget,
        column: u16,
        row: u16,
    },
    Scroll {
        up: bool,
        column: u16,
        row: u16,
    },
    CaptureStarted {
        target: PointerTarget,
        grab_offset: (u16, u16),
    },
    CaptureDragged {
        target: PointerTarget,
        column: u16,
        row: u16,
        grab_offset: (u16, u16),
    },
    CaptureEnded {
        target: PointerTarget,
    },
    CaptureCancelled {
        target: PointerTarget,
    },
    Invalidate,
}

/// 绘制层读取的只读指针快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerVisual {
    pub hovered: Option<PointerTarget>,
    pub pressed: Option<PointerTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerCapture {
    pub target: PointerTarget,
    pub grab_offset: (u16, u16),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerState {
    hovered: Option<PointerTarget>,
    pressed: Option<PointerTarget>,
    capture: Option<PointerCapture>,
}

impl PointerState {
    #[cfg(test)]
    pub fn hovered(&self) -> Option<&PointerTarget> {
        self.hovered.as_ref()
    }

    #[cfg(test)]
    pub fn pressed(&self) -> Option<&PointerTarget> {
        self.pressed.as_ref()
    }

    #[cfg(test)]
    pub fn capture(&self) -> Option<&PointerCapture> {
        self.capture.as_ref()
    }

    /// 当前状态的只读快照；绘制镜像只能从这里派生。
    pub fn visual(&self) -> PointerVisual {
        PointerVisual {
            hovered: self.hovered.clone(),
            pressed: self.pressed.clone(),
        }
    }

    /// 帧失效、路由切换、焦点丢失时清空全部瞬时状态。
    pub fn clear(&mut self) {
        self.hovered = None;
        self.pressed = None;
        self.capture = None;
    }

    pub fn handle(&mut self, frame: Option<&FrameHitMap>, event: PointerEvent) -> PointerAction {
        match event {
            PointerEvent::Moved { column, row } => {
                let next = hit(frame, column, row).map(|region| region.target.clone());
                let changed = self.hovered != next;
                self.hovered = next;
                if changed {
                    PointerAction::Redraw
                } else {
                    PointerAction::None
                }
            }
            PointerEvent::Down {
                button,
                column,
                row,
            } => self.down(frame, button, column, row),
            PointerEvent::Up {
                button,
                column,
                row,
            } => self.up(frame, button, column, row),
            PointerEvent::Drag {
                button,
                column,
                row,
            } => {
                if button != MouseButton::Left {
                    return PointerAction::None;
                }
                match self.capture.as_ref() {
                    Some(capture) => PointerAction::CaptureDragged {
                        target: capture.target.clone(),
                        column,
                        row,
                        grab_offset: capture.grab_offset,
                    },
                    None => PointerAction::None,
                }
            }
            PointerEvent::ScrollUp { column, row } => PointerAction::Scroll {
                up: true,
                column,
                row,
            },
            PointerEvent::ScrollDown { column, row } => PointerAction::Scroll {
                up: false,
                column,
                row,
            },
            PointerEvent::Leave => self.leave(),
            PointerEvent::RouteChanged | PointerEvent::Resized | PointerEvent::FocusLost => {
                self.clear();
                PointerAction::Invalidate
            }
        }
    }

    fn down(
        &mut self,
        frame: Option<&FrameHitMap>,
        button: MouseButton,
        column: u16,
        row: u16,
    ) -> PointerAction {
        if button != MouseButton::Left {
            return PointerAction::None;
        }
        let region = hit(frame, column, row);
        self.hovered = region.map(|region| region.target.clone());
        self.pressed = None;

        let Some(region) = region else {
            return PointerAction::Redraw;
        };
        if is_scrollbar_thumb(&region.target) {
            let grab_offset = (
                column.saturating_sub(region.rect.x),
                row.saturating_sub(region.rect.y),
            );
            self.capture = Some(PointerCapture {
                target: region.target.clone(),
                grab_offset,
            });
            return PointerAction::CaptureStarted {
                target: region.target.clone(),
                grab_offset,
            };
        }

        self.pressed = Some(region.target.clone());
        PointerAction::Pressed {
            target: region.target.clone(),
            column,
            row,
        }
    }

    fn up(
        &mut self,
        frame: Option<&FrameHitMap>,
        button: MouseButton,
        column: u16,
        row: u16,
    ) -> PointerAction {
        if button != MouseButton::Left {
            return PointerAction::None;
        }
        if let Some(capture) = self.capture.take() {
            self.hovered = hit(frame, column, row).map(|region| region.target.clone());
            self.pressed = None;
            return PointerAction::CaptureEnded {
                target: capture.target,
            };
        }

        let released = hit(frame, column, row).map(|region| region.target.clone());
        let pressed = self.pressed.take();
        self.hovered = released.clone();
        match (pressed, released) {
            (Some(pressed), Some(released)) if pressed == released => PointerAction::Activate {
                target: released,
                column,
                row,
            },
            _ => PointerAction::Redraw,
        }
    }

    fn leave(&mut self) -> PointerAction {
        self.hovered = None;
        if let Some(capture) = self.capture.take() {
            self.pressed = None;
            PointerAction::CaptureCancelled {
                target: capture.target,
            }
        } else {
            PointerAction::Redraw
        }
    }
}

fn hit(frame: Option<&FrameHitMap>, column: u16, row: u16) -> Option<&HitRegion> {
    frame?
        .resolve(column, row, MouseButton::Left)
        .ok()
        .flatten()
}

fn is_scrollbar_thumb(target: &PointerTarget) -> bool {
    matches!(target, PointerTarget::Scrollbar(ScrollbarPart::Thumb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ModalHit;
    use crate::ui::v2::hit::{AnchorExpectation, FrameId, HitRegion, VisualAnchor, z};
    use crate::ui::v2::route::ScreenRoute;
    use ratatui::layout::{Position, Rect, Size};

    fn target() -> PointerTarget {
        PointerTarget::Modal(ModalHit::PermissionApprove)
    }

    fn region(target: PointerTarget, rect: Rect, enabled: bool) -> HitRegion {
        HitRegion::new(
            rect,
            Rect::new(0, 0, 100, 50),
            Rect::new(0, 0, rect.width, rect.height),
            target,
            MouseButton::Left,
            enabled,
            z::MODAL_BUTTON,
            VisualAnchor {
                position: Position::new(rect.x, rect.y),
                expectation: AnchorExpectation::NonEmptyCell,
            },
        )
    }

    fn frame(regions: Vec<HitRegion>) -> FrameHitMap {
        FrameHitMap {
            frame_id: FrameId::new(1),
            route: ScreenRoute::Agent,
            terminal_size: Size::new(100, 50),
            scroll_offset: 0,
            regions,
        }
    }

    #[test]
    fn hover_does_not_activate() {
        let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), true)]);
        let mut state = PointerState::default();

        assert_eq!(
            state.handle(Some(&frame), PointerEvent::Moved { column: 1, row: 0 }),
            PointerAction::Redraw
        );
        assert_eq!(state.hovered(), Some(&target()));
        assert!(state.pressed().is_none());
    }

    #[test]
    fn down_does_not_activate_and_up_inside_activates_once() {
        let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), true)]);
        let mut state = PointerState::default();

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Down {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0
                }
            ),
            PointerAction::Pressed {
                target: target(),
                column: 1,
                row: 0,
            }
        );
        assert_eq!(state.pressed(), Some(&target()));

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Up {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0
                }
            ),
            PointerAction::Activate {
                target: target(),
                column: 1,
                row: 0,
            }
        );
        assert!(state.pressed().is_none());

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Up {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0
                }
            ),
            PointerAction::Redraw
        );
    }

    #[test]
    fn up_outside_does_not_activate() {
        let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), true)]);
        let mut state = PointerState::default();
        let _ = state.handle(
            Some(&frame),
            PointerEvent::Down {
                button: MouseButton::Left,
                column: 1,
                row: 0,
            },
        );

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Up {
                    button: MouseButton::Left,
                    column: 20,
                    row: 0
                }
            ),
            PointerAction::Redraw
        );
        assert!(state.pressed().is_none());
    }

    #[test]
    fn drag_out_and_back_can_activate() {
        let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), true)]);
        let mut state = PointerState::default();
        let _ = state.handle(
            Some(&frame),
            PointerEvent::Down {
                button: MouseButton::Left,
                column: 1,
                row: 0,
            },
        );
        let _ = state.handle(Some(&frame), PointerEvent::Moved { column: 20, row: 0 });
        assert_eq!(state.hovered(), None);
        assert_eq!(state.pressed(), Some(&target()));

        let _ = state.handle(Some(&frame), PointerEvent::Moved { column: 1, row: 0 });
        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Up {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0
                }
            ),
            PointerAction::Activate {
                target: target(),
                column: 1,
                row: 0,
            }
        );
    }

    #[test]
    fn disabled_target_never_enters_pressed() {
        let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), false)]);
        let mut state = PointerState::default();

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Down {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0
                }
            ),
            PointerAction::Redraw
        );
        assert!(state.pressed().is_none());
        assert_eq!(state.hovered(), None);
    }

    #[test]
    fn route_resize_and_focus_loss_clear_all_pointer_state() {
        for reset in [
            PointerEvent::RouteChanged,
            PointerEvent::Resized,
            PointerEvent::FocusLost,
        ] {
            let frame = frame(vec![region(target(), Rect::new(0, 0, 10, 1), true)]);
            let mut state = PointerState::default();
            let _ = state.handle(Some(&frame), PointerEvent::Moved { column: 1, row: 0 });
            let _ = state.handle(
                Some(&frame),
                PointerEvent::Down {
                    button: MouseButton::Left,
                    column: 1,
                    row: 0,
                },
            );

            assert_eq!(state.handle(Some(&frame), reset), PointerAction::Invalidate);
            assert!(state.hovered().is_none());
            assert!(state.pressed().is_none());
            assert!(state.capture().is_none());
        }
    }

    #[test]
    fn scrollbar_capture_keeps_drag_outside_and_releases_on_up() {
        let thumb = PointerTarget::Scrollbar(ScrollbarPart::Thumb);
        let frame = frame(vec![region(thumb.clone(), Rect::new(4, 2, 2, 6), true)]);
        let mut state = PointerState::default();

        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Down {
                    button: MouseButton::Left,
                    column: 5,
                    row: 4
                }
            ),
            PointerAction::CaptureStarted {
                target: thumb.clone(),
                grab_offset: (1, 2)
            }
        );
        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Drag {
                    button: MouseButton::Left,
                    column: 30,
                    row: 40
                }
            ),
            PointerAction::CaptureDragged {
                target: thumb.clone(),
                column: 30,
                row: 40,
                grab_offset: (1, 2),
            }
        );
        assert_eq!(
            state.handle(
                Some(&frame),
                PointerEvent::Up {
                    button: MouseButton::Left,
                    column: 30,
                    row: 40
                }
            ),
            PointerAction::CaptureEnded { target: thumb }
        );
        assert!(state.capture().is_none());
        assert!(state.pressed().is_none());
    }

    #[test]
    fn leave_cancels_capture() {
        let thumb = PointerTarget::Scrollbar(ScrollbarPart::Thumb);
        let frame = frame(vec![region(thumb.clone(), Rect::new(4, 2, 2, 6), true)]);
        let mut state = PointerState::default();
        let _ = state.handle(
            Some(&frame),
            PointerEvent::Down {
                button: MouseButton::Left,
                column: 5,
                row: 4,
            },
        );

        assert_eq!(
            state.handle(Some(&frame), PointerEvent::Leave),
            PointerAction::CaptureCancelled { target: thumb }
        );
        assert!(state.hovered().is_none());
        assert!(state.pressed().is_none());
        assert!(state.capture().is_none());
    }

    #[test]
    fn scroll_events_are_reported_without_hover_dependency() {
        let frame = frame(Vec::new());
        let mut state = PointerState::default();
        assert_eq!(
            state.handle(Some(&frame), PointerEvent::ScrollUp { column: 0, row: 0 }),
            PointerAction::Scroll {
                up: true,
                column: 0,
                row: 0
            }
        );
    }
}
