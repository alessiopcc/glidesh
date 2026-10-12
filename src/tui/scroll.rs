//! Lists taller than their panel: which rows are on screen, and the scrollbar beside them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

/// The first of `len` rows to show in `height` lines so that `selected` is on screen. It
/// moves `offset` only as far as needed, so the view does not jump while the cursor stays
/// inside it.
pub fn follow(offset: usize, selected: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let offset = offset.min(len - height);
    if selected < offset {
        selected
    } else if selected >= offset + height {
        (selected + 1 - height).min(len - height)
    } else {
        offset
    }
}

/// A scrollbar on the right border of the bordered `area`, when `total` rows do not fit in
/// `visible` lines.
pub fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    visible: usize,
    position: usize,
) {
    if total <= visible || area.width == 0 {
        return;
    }
    let track = Rect {
        x: area.x + area.width - 1,
        y: area.y + 1,
        width: 1,
        height: area.height.saturating_sub(2),
    };
    let mut state = ScrollbarState::new(total - visible).position(position);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"))
        .track_symbol(Some("░"))
        .thumb_symbol("█");
    frame.render_stateful_widget(scrollbar, track, &mut state);
}

#[cfg(test)]
mod tests {
    use super::follow;

    #[test]
    fn rows_that_fit_never_scroll() {
        assert_eq!(follow(0, 4, 5, 10), 0);
        assert_eq!(follow(3, 4, 5, 5), 0);
    }

    #[test]
    fn the_view_stays_put_while_the_cursor_is_inside_it() {
        assert_eq!(follow(5, 5, 30, 10), 5);
        assert_eq!(follow(5, 14, 30, 10), 5);
    }

    #[test]
    fn moving_past_the_bottom_scrolls_one_row() {
        assert_eq!(follow(0, 10, 30, 10), 1);
        assert_eq!(follow(5, 15, 30, 10), 6);
    }

    #[test]
    fn moving_above_the_top_scrolls_to_the_cursor() {
        assert_eq!(follow(5, 4, 30, 10), 4);
    }

    #[test]
    fn wrapping_around_jumps_the_view_to_the_other_end() {
        assert_eq!(follow(0, 29, 30, 10), 20);
        assert_eq!(follow(20, 0, 30, 10), 0);
    }

    /// Rows removed under the view, by collapsing a group or killing a tunnel, must not
    /// leave blank lines below the last one.
    #[test]
    fn a_shrunk_list_pulls_the_view_back() {
        assert_eq!(follow(20, 12, 15, 10), 5);
    }

    #[test]
    fn no_room_shows_nothing_to_scroll() {
        assert_eq!(follow(7, 9, 30, 0), 0);
    }
}
