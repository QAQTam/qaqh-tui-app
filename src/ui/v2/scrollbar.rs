//! Shared scrollbar geometry for rendering, hit-testing and drag math.

use ratatui::layout::Rect;

/// Geometry for one rendered scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarMetrics {
    pub track: Rect,
    pub thumb: Rect,
    pub max_top: usize,
    pub thumb_top: usize,
    pub thumb_height: usize,
}

impl ScrollbarMetrics {
    /// Build metrics for a scrollable viewport.
    ///
    /// Returns `None` when the scrollbar is hidden: no space, empty viewport,
    /// or content that already fits.
    pub fn new(
        track: Rect,
        total: usize,
        viewport_height: usize,
        follow: bool,
        offset: usize,
    ) -> Option<Self> {
        if track.is_empty() || total <= viewport_height || viewport_height == 0 {
            return None;
        }
        let track_height = usize::from(track.height);
        let thumb_height = track_height
            .saturating_mul(viewport_height)
            .checked_div(total)
            .unwrap_or(1)
            .clamp(1, track_height);
        let max_top = total.saturating_sub(viewport_height);
        let top = crate::ui::viewport_top(total, viewport_height, follow, offset);
        let thumb_top = if max_top == 0 {
            0
        } else {
            top.saturating_mul(track_height.saturating_sub(thumb_height))
                .checked_div(max_top)
                .unwrap_or(0)
        }
        .min(track_height.saturating_sub(thumb_height));
        Some(Self {
            track,
            thumb: Rect::new(
                track.x,
                track.y.saturating_add(thumb_top as u16),
                track.width,
                thumb_height as u16,
            ),
            max_top,
            thumb_top,
            thumb_height,
        })
    }

    pub fn track_height(&self) -> usize {
        usize::from(self.track.height)
    }

    /// Top content line represented by a thumb top position.
    pub fn top_for_thumb_top(&self, thumb_top: usize) -> usize {
        let travel = self.track_height().saturating_sub(self.thumb_height);
        if travel == 0 {
            0
        } else {
            thumb_top
                .min(travel)
                .saturating_mul(self.max_top)
                .checked_div(travel)
                .unwrap_or(0)
        }
    }

    /// Scroll offset for a top content line.
    pub fn offset_for_top(&self, top: usize) -> usize {
        self.max_top.saturating_sub(top.min(self.max_top))
    }

    /// Clicking the track centers the thumb on the clicked row.
    pub fn offset_for_track_row(&self, row: u16) -> usize {
        let relative = usize::from(row.saturating_sub(self.track.y));
        let thumb_top = relative
            .saturating_sub(self.thumb_height / 2)
            .min(self.track_height().saturating_sub(self.thumb_height));
        self.offset_for_top(self.top_for_thumb_top(thumb_top))
    }

    /// Dragging the thumb preserves the pointer's grab offset.
    pub fn offset_for_drag_row(&self, row: u16, grab_offset_y: u16) -> usize {
        let relative = usize::from(row.saturating_sub(self.track.y));
        let thumb_top = relative
            .saturating_sub(usize::from(grab_offset_y))
            .min(self.track_height().saturating_sub(self.thumb_height));
        self.offset_for_top(self.top_for_thumb_top(thumb_top))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(offset: usize) -> ScrollbarMetrics {
        ScrollbarMetrics::new(Rect::new(10, 2, 1, 20), 100, 20, false, offset)
            .expect("scrollable metrics")
    }

    #[test]
    fn thumb_stays_inside_track_and_offsets_are_monotonic() {
        let bottom = metrics(0);
        assert_eq!(bottom.thumb.y, 18);
        assert_eq!(bottom.thumb.height, 4);

        let middle = metrics(40);
        let top = metrics(80);
        assert!(bottom.thumb.y > middle.thumb.y && middle.thumb.y > top.thumb.y);
        assert_eq!(top.thumb.y, 2);
        assert_eq!(top.thumb.bottom(), 6);
    }

    #[test]
    fn track_click_centers_thumb_on_row() {
        let metrics = metrics(0);
        let offset = metrics.offset_for_track_row(12);
        let clicked =
            ScrollbarMetrics::new(metrics.track, 100, 20, false, offset).expect("metrics");
        assert!((usize::from(clicked.thumb.y)..usize::from(clicked.thumb.bottom())).contains(&12));
    }

    #[test]
    fn drag_preserves_grab_offset() {
        let metrics = metrics(40);
        let row = metrics.thumb.y.saturating_add(1);
        let offset = metrics.offset_for_drag_row(row, 1);
        let dragged =
            ScrollbarMetrics::new(metrics.track, 100, 20, false, offset).expect("metrics");
        assert_eq!(dragged.thumb.y, metrics.thumb.y);
    }
}
