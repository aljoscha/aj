//! Shared scroll policy for the vaxis frontend's line-scrolling widgets.
//!
//! The page-turn overlap, the half-page step, and the pre-first-draw fallback
//! are UX policy, so they live here rather than in vaxis's `ListView`, which
//! only supplies the raw `viewport_height()` mechanism. The read-only content
//! overlay turns a full page ([`page_scroll_lines`]); the transcript takes a
//! gentler half-page step ([`half_page_scroll_lines`]).

use vaxis::cell::{Cell, Character, Style};
use vaxis::vxfw::{ListView, ScrollBars};

/// Rows kept in common between two page-scroll steps, so a reader keeps a
/// little context across a page turn rather than jumping a full viewport.
const PAGE_OVERLAP: u16 = 2;

/// Page size (in lines) used before the first draw has measured the real
/// viewport height. A page-scroll issued that early is rare, so a sane
/// constant is enough until the next draw records the true height.
const DEFAULT_PAGE_LINES: i32 = 20;

/// Line delta for a one-viewport page scroll.
///
/// Returns the given viewport height minus a small overlap, so a page turn
/// keeps a couple of context rows rather than jumping a full viewport. A
/// viewport too short to overlap still pages by at least one row, so the delta
/// is never zero. Falls back to [`DEFAULT_PAGE_LINES`] when no viewport has
/// been measured yet (`None`, before the first draw), and likewise for a
/// degenerate zero-height viewport.
pub(crate) fn page_scroll_lines(viewport_height: Option<u16>) -> i32 {
    match viewport_height {
        Some(h) if h > PAGE_OVERLAP => i32::from(h - PAGE_OVERLAP),
        Some(h) if h > 0 => i32::from(h),
        _ => DEFAULT_PAGE_LINES,
    }
}

/// Line delta for a half-viewport scroll, the transcript's page-key step.
///
/// Half the viewport height keeps the jump gentle enough to stay oriented
/// without a page-turn overlap (half the screen is retained anyway). Never
/// zero: a viewport too short to halve still steps by one row. Falls back to
/// half [`DEFAULT_PAGE_LINES`] before the first draw and for a degenerate
/// zero-height viewport, matching [`page_scroll_lines`].
pub(crate) fn half_page_scroll_lines(viewport_height: Option<u16>) -> i32 {
    match viewport_height {
        Some(h) if h > 0 => i32::from(h / 2).max(1),
        _ => DEFAULT_PAGE_LINES / 2,
    }
}

/// Tint the vertical scroll-bar thumb cells from `style`.
///
/// Applied on each draw so a runtime restyle (theme swap) is reflected without
/// rebuilding the bars. The hover and drag cells are tinted to match: the bars
/// self-stamp their surface, so they receive mouse events via bus routing and
/// the hover and drag cells are used while the thumb is hovered or dragged.
pub(crate) fn apply_thumb_style(bars: &mut ScrollBars<ListView>, style: Style) {
    let cell = |grapheme: &str| Cell {
        char: Character::new(grapheme, 1),
        style,
        ..Cell::default()
    };
    bars.vertical_scrollbar_thumb = cell("\u{2590}");
    bars.vertical_scrollbar_hover_thumb = cell("\u{2588}");
    bars.vertical_scrollbar_drag_thumb = cell("\u{2588}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtracts_the_overlap_when_the_viewport_is_tall_enough() {
        assert_eq!(page_scroll_lines(Some(10)), 10 - i32::from(PAGE_OVERLAP));
    }

    #[test]
    fn short_viewport_pages_by_at_least_one_row() {
        // Exactly the overlap, and a single row, both page by the full height
        // rather than collapsing to a zero delta.
        assert_eq!(
            page_scroll_lines(Some(PAGE_OVERLAP)),
            i32::from(PAGE_OVERLAP)
        );
        assert_eq!(page_scroll_lines(Some(1)), 1);
    }

    #[test]
    fn falls_back_before_the_first_draw() {
        assert_eq!(page_scroll_lines(None), DEFAULT_PAGE_LINES);
    }

    #[test]
    fn zero_height_viewport_falls_back() {
        // A degenerate zero-height viewport can't page by its own height, so it
        // takes the same fallback as an unmeasured one rather than a zero delta.
        assert_eq!(page_scroll_lines(Some(0)), DEFAULT_PAGE_LINES);
    }

    #[test]
    fn half_page_is_half_the_viewport() {
        assert_eq!(half_page_scroll_lines(Some(10)), 5);
        assert_eq!(half_page_scroll_lines(Some(11)), 5);
    }

    #[test]
    fn half_page_short_viewport_steps_by_at_least_one_row() {
        assert_eq!(half_page_scroll_lines(Some(1)), 1);
    }

    #[test]
    fn half_page_falls_back_before_the_first_draw() {
        assert_eq!(half_page_scroll_lines(None), DEFAULT_PAGE_LINES / 2);
        assert_eq!(half_page_scroll_lines(Some(0)), DEFAULT_PAGE_LINES / 2);
    }
}
