//! Read-only list overlay.
//!
//! Some overlays (help, auth status, session info) present a
//! non-interactive [`SelectList`] purely to be read: there is nothing to
//! select, both Esc and Enter close the view, and every other key is
//! swallowed so it can't leak to the background. `ReadOnlyListOverlay` is
//! that shape, shared by the host's read-only overlays.
//!
//! The host builds the [`SelectList`] (with its selection indicator
//! suppressed) and wraps it here, then polls [`Self::outcome_handle`]:
//! a `Some(())` means "close me".
//!
//! Scrolling is document-style, not selection-driven. The overlay keeps
//! its own top-row offset and windows the list's rendered lines, so the
//! arrow keys scroll one line at a time, the offset is clamped at the top
//! and bottom (no wraparound), and a position indicator is shown while
//! there is more above or below. We deliberately do not reuse the list's
//! own selection-follows-cursor scrolling: with no visible cursor it
//! reads as a dead zone (the window only moves once the hidden selection
//! crosses the half-way point).

use std::sync::Arc;

use aj_tui::ansi::truncate_to_width;
use aj_tui::component::Component;
use aj_tui::components::select_list::SelectList;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::modes::interactive::components::outcome::OutcomeSlot;

/// Handle the host polls to learn the overlay was closed. The unit payload
/// carries no data: the only outcome of a read-only view is "closed".
pub type ReadOnlyCloseHandle = OutcomeSlot<()>;

/// A non-interactive [`SelectList`] that closes on Esc/Enter, scrolls
/// when its content overflows the frame, and swallows every other key.
pub struct ReadOnlyListOverlay {
    list: SelectList,
    outcome: ReadOnlyCloseHandle,
    focused: bool,
    /// Style for the position indicator, taken from the list's theme so
    /// the indicator matches the rest of the overlay.
    scroll_info_style: Arc<dyn Fn(&str) -> String>,
    /// Index of the first visible row (document scroll offset). Clamped
    /// against the live content/viewport on every render and key press.
    scroll: usize,
    /// Inner-row budget reported by the overlay frame. `usize::MAX` until
    /// the frame reports one, so a direct render (e.g. in tests) shows
    /// every row.
    viewport_rows: usize,
    /// Rows the wrapped list last produced, cached from `render` so key
    /// handling can clamp `scroll` without re-rendering.
    ///
    /// NOTE: this is filled by `render`, which the frame loop always runs
    /// before delivering input. A key arriving before the first render
    /// just no-ops (the cache reads as empty), and `render` re-clamps
    /// `scroll` before slicing, so a stale cache can never produce an
    /// out-of-range slice.
    content_rows: usize,
}

impl ReadOnlyListOverlay {
    /// Wrap a read-only `list`. The caller builds it with
    /// `show_selection_indicator: false` so no row reads as focused.
    /// `scroll_info_style` styles the position indicator and is typically
    /// the list theme's `scroll_info`.
    pub fn new(mut list: SelectList, scroll_info_style: Arc<dyn Fn(&str) -> String>) -> Self {
        // This overlay does its own windowing, so the wrapped list must
        // render every row and never draw its own scroll indicator. Sizing
        // the list's window to its item count guarantees that whatever
        // `max_visible` the caller passed.
        let all_rows = list.items().len().max(1);
        list.set_max_visible(all_rows);
        Self {
            list,
            outcome: ReadOnlyCloseHandle::new(),
            focused: true,
            scroll_info_style,
            scroll: 0,
            viewport_rows: usize::MAX,
            content_rows: 0,
        }
    }

    /// Hand the host a clone of the close slot.
    pub fn outcome_handle(&self) -> ReadOnlyCloseHandle {
        self.outcome.clone()
    }

    /// Resolve the visible window for the current content and viewport:
    /// `(rows_shown, max_scroll)`. When everything fits, the whole list is
    /// shown and `max_scroll` is `0`. Otherwise one row is reserved for the
    /// position indicator, so `rows_shown = viewport - 1`.
    fn window(&self) -> (usize, usize) {
        let total = self.content_rows;
        let viewport = self.viewport_rows.max(1);
        if total <= viewport {
            (total, 0)
        } else {
            let shown = viewport.saturating_sub(1).max(1);
            (shown, total - shown)
        }
    }
}

impl Component for ReadOnlyListOverlay {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        // The wrapped list renders every row, so we window the result here.
        let all = self.list.render(width);
        self.content_rows = all.len();

        let (shown, max_scroll) = self.window();
        if max_scroll == 0 {
            self.scroll = 0;
            return all;
        }

        self.scroll = self.scroll.min(max_scroll);
        let mut out: Vec<aj_tui::Line> = all[self.scroll..self.scroll + shown].to_vec();

        // Position indicator on the reserved last row: `(first-last/total)`,
        // clamped like the list's own indicator so narrow frames stay tidy.
        let text = format!(
            "  ({}-{}/{})",
            self.scroll + 1,
            self.scroll + shown,
            self.content_rows
        );
        let clamped = truncate_to_width(&text, width.saturating_sub(2), "", false);
        out.push((self.scroll_info_style)(&clamped).into());
        out
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(());
            return true;
        }

        // Document scroll, clamped at both ends (no wraparound).
        let (shown, max_scroll) = self.window();
        if kb.matches(event, "tui.select.up") {
            self.scroll = self.scroll.saturating_sub(1);
        } else if kb.matches(event, "tui.select.down") {
            self.scroll = (self.scroll + 1).min(max_scroll);
        } else if kb.matches(event, "tui.select.pageUp") {
            self.scroll = self.scroll.saturating_sub(shown);
        } else if kb.matches(event, "tui.select.pageDown") {
            self.scroll = (self.scroll + shown).min(max_scroll);
        }
        // Swallow every other key: the list is read-only, so nothing
        // should reach the components behind it.
        true
    }

    fn set_available_height(&mut self, rows: usize) {
        self.viewport_rows = rows;
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use aj_tui::components::select_list::{
        SelectItem, SelectList, SelectListLayout, SelectListTheme,
    };
    use aj_tui::keys::Key;

    use super::*;

    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
        }
    }

    fn overlay_with(rows: usize) -> ReadOnlyListOverlay {
        let items: Vec<SelectItem> = (0..rows)
            .map(|i| SelectItem::new("", &format!("row {i}")))
            .collect();
        let layout = SelectListLayout {
            show_selection_indicator: false,
            ..Default::default()
        };
        let theme = identity_theme();
        let scroll_info = Arc::clone(&theme.scroll_info);
        let list = SelectList::new(items, rows.max(1), theme, layout);
        ReadOnlyListOverlay::new(list, scroll_info)
    }

    fn body(overlay: &mut ReadOnlyListOverlay) -> Vec<aj_tui::Line> {
        overlay.render(80)
    }

    /// A single down-press scrolls by exactly one row (no centered-cursor
    /// dead zone): the top row leaves the view immediately.
    #[test]
    fn one_keypress_scrolls_one_row() {
        let mut c = overlay_with(40);
        c.set_available_height(10);
        let first = body(&mut c);
        assert!(first.iter().any(|l| l.contains("row 0")), "{first:?}");

        c.handle_input(&Key::down());
        let after = body(&mut c);
        assert!(!after.iter().any(|l| l.ends_with("row 0")), "{after:?}");
        assert!(after.iter().any(|l| l.contains("row 1")), "{after:?}");
    }

    /// An overflowing list fills exactly the budget (content rows plus the
    /// indicator) and the window reaches the last row when scrolled down.
    #[test]
    fn overflowing_list_fills_budget_and_reaches_bottom() {
        let mut c = overlay_with(40);
        c.set_available_height(10);
        let lines = body(&mut c);
        assert_eq!(lines.len(), 10, "{lines:?}");
        assert!(lines.last().unwrap().contains("/40"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("row 39")), "{lines:?}");

        for _ in 0..200 {
            c.handle_input(&Key::down());
        }
        let lines = body(&mut c);
        assert!(lines.iter().any(|l| l.contains("row 39")), "{lines:?}");
    }

    /// Scrolling is capped at both ends: down past the bottom stays on the
    /// last page, and up past the top returns to (and stays at) the first.
    #[test]
    fn scroll_is_capped_with_no_wraparound() {
        let mut c = overlay_with(40);
        c.set_available_height(10);
        // Render once so the overlay learns its content height, as the
        // frame loop does before delivering any input.
        body(&mut c);

        for _ in 0..200 {
            c.handle_input(&Key::down());
        }
        let bottom = body(&mut c);
        // At the bottom the first row is gone and the last row is visible.
        // Wraparound would have folded row 0 back into view.
        assert!(bottom.iter().any(|l| l.contains("row 39")), "{bottom:?}");
        assert!(!bottom.iter().any(|l| l.contains("row 0")), "{bottom:?}");

        for _ in 0..200 {
            c.handle_input(&Key::up());
        }
        let top = body(&mut c);
        assert!(top.iter().any(|l| l.contains("row 0")), "{top:?}");
        assert!(!top.iter().any(|l| l.contains("row 39")), "{top:?}");
    }

    /// A list that fits in the budget shows no indicator and ignores scroll.
    #[test]
    fn fitting_list_has_no_indicator() {
        let mut c = overlay_with(3);
        c.set_available_height(20);
        c.handle_input(&Key::down());
        let lines = body(&mut c);
        assert!(!lines.iter().any(|l| l.contains("/3")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("row 0")), "{lines:?}");
    }

    #[test]
    fn esc_and_enter_close() {
        let mut c = overlay_with(3);
        let h = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(h.take().is_some(), "Esc should close");

        let mut c = overlay_with(3);
        let h = c.outcome_handle();
        c.handle_input(&Key::enter());
        assert!(h.take().is_some(), "Enter should close");
    }
}
