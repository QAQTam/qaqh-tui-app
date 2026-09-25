//! UI primitives shared by the V2 fullscreen shell.

pub mod v2;

/// Convert the scroll state into the first visible line index.
///
/// `follow` keeps the viewport pinned to the bottom. Otherwise `offset` counts
/// lines up from the bottom and is clamped to the available content.
pub fn viewport_top(total: usize, height: usize, follow: bool, offset: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if follow {
        total - height
    } else {
        total.saturating_sub(height).saturating_sub(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::viewport_top;

    #[test]
    fn viewport_top_follows_bottom_and_clamps_offset() {
        assert_eq!(viewport_top(100, 20, true, 0), 80);
        assert_eq!(viewport_top(100, 20, false, 10), 70);
        assert_eq!(viewport_top(100, 20, false, usize::MAX), 0);
        assert_eq!(viewport_top(10, 20, false, 5), 0);
        assert_eq!(viewport_top(10, 0, false, 5), 0);
    }
}
