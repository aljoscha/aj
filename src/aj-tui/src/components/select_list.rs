//! Navigable selection list component.
//!
//! Supports filtering, per-item descriptions aligned into a shared column,
//! configurable primary-column bounds, and an optional custom primary
//! truncator.

use std::sync::Arc;

use crate::ansi::{truncate_to_width, visible_width};
use crate::component::Component;
use crate::keybindings;
use crate::keys::InputEvent;

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// An item in a selection list.
#[derive(Debug, Clone)]
pub struct SelectItem {
    /// The value returned on selection.
    pub value: String,
    /// The display label. Falls back to `value` if empty.
    pub label: String,
    /// Optional description shown to the right.
    pub description: Option<String>,
}

impl SelectItem {
    pub fn new(value: &str, label: &str) -> Self {
        Self {
            value: value.to_string(),
            label: label.to_string(),
            description: None,
        }
    }

    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = Some(desc.to_string());
        self
    }

    fn display_value(&self) -> &str {
        if self.label.is_empty() {
            self.value.as_str()
        } else {
            self.label.as_str()
        }
    }
}

/// Theme for the selection list.
///
/// Mirrors pi-tui's `SelectListTheme` interface
/// (`packages/tui/src/components/select-list.ts`). Pi-tui ships no
/// upstream default theme — the agent layer is expected to assemble
/// the closures from its central palette and pass the populated theme
/// in at construction time. We deliberately do not provide a `Default`
/// impl: the tui crate stays palette-agnostic, and tests build themes
/// via `tests/support/themes.rs` (mirroring pi's
/// `packages/tui/test/test-themes.ts`).
///
/// The closures use `Arc` rather than `Box` so a single theme can be
/// cheaply cloned into nested or sibling components (e.g. an
/// [`Editor`][crate::components::editor::Editor]'s autocomplete popup
/// reuses its parent editor's theme without rebuilding the closures).
#[derive(Clone)]
pub struct SelectListTheme {
    /// Style for the prefix (e.g. "→ ") placed before the selected item.
    pub selected_prefix: Arc<dyn Fn(&str) -> String>,
    /// Style for the selected item text.
    pub selected_text: Arc<dyn Fn(&str) -> String>,
    /// Style for description text.
    pub description: Arc<dyn Fn(&str) -> String>,
    /// Style for scroll indicator.
    pub scroll_info: Arc<dyn Fn(&str) -> String>,
    /// Style for "no matches" text.
    pub no_match: Arc<dyn Fn(&str) -> String>,
}

/// Context passed to a custom [`SelectListLayout::truncate_primary`]
/// callback.
pub struct TruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

/// Layout tuning knobs for the primary column and description alignment.
#[derive(Default)]
pub struct SelectListLayout {
    /// Minimum width (including the 2-char gap) of the primary column.
    pub min_primary_column_width: Option<usize>,
    /// Maximum width (including the 2-char gap) of the primary column.
    pub max_primary_column_width: Option<usize>,
    /// Override the default primary-column truncation. The default is
    /// hard truncation with no ellipsis. Return value must fit in
    /// `context.max_width`; if it overflows, it will be re-truncated.
    pub truncate_primary: Option<Box<dyn Fn(TruncatePrimaryContext<'_>) -> String>>,
}

/// A navigable selection list.
///
/// # Scroll model
///
/// The visible window is recomputed from the current selection on every
/// render, matching the original `pi` framework's `select-list.ts`.
/// Selection is the only piece of scroll-affecting state; there's no
/// persistent `scroll_offset` field.
///
/// The window starts at
/// `clamp(selected - max_visible / 2, 0, len - max_visible)` and runs for
/// `max_visible` rows, so the selection floats near the center of the
/// visible region whenever there's room and clamps to the top/bottom edge
/// near the ends of the list. Wraparound (Up at index `0`, Down at
/// `len - 1`) only changes `selected`; the next render naturally lands the
/// new selection at the appropriate end.
///
/// `set_filter` resets `selected` to `0`.
///
/// # Constructor shape
///
/// [`SelectList::new`] mirrors pi-tui's
/// `new SelectList(items, maxVisible, theme, layout?)` byte-for-byte: the
/// theme and layout are taken at construction time and are immutable for
/// the life of the instance. Mutators that pi-tui does not expose
/// (`set_theme`, `set_layout`, `set_items`) are intentionally absent —
/// rebuild the list when those need to change. See PORTING.md F49 for
/// the audit and rationale.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_indices: Vec<usize>,
    selected: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayout,
    /// Called when an item is selected (Enter).
    pub on_select: Option<Box<dyn FnMut(&SelectItem)>>,
    /// Called when the list is cancelled (Escape).
    pub on_cancel: Option<Box<dyn FnMut()>>,
    /// Called when the selection changes.
    pub on_selection_change: Option<Box<dyn FnMut(&SelectItem)>>,
}

impl SelectList {
    /// Create a new select list.
    ///
    /// Mirrors pi-tui's
    /// `new SelectList(items, maxVisible, theme, layout?)` constructor
    /// byte-for-byte (`packages/tui/src/components/select-list.ts:52-58`).
    /// The theme is taken as a required argument so the tui crate stays
    /// palette-agnostic; build the theme from the agent's central
    /// palette (or a test fixture) and pass it in. Layout is also
    /// required — pi defaults it to `{}` (use built-in column bounds);
    /// the Rust side asks callers to pass [`SelectListLayout::default`]
    /// explicitly to make the value visible at the call site. Both are
    /// immutable for the life of the instance: rebuild the list to
    /// change them (see PORTING.md F49).
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayout,
    ) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filtered_indices,
            selected: 0,
            max_visible,
            theme,
            layout,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    /// Set the filter text. Items whose value starts with the filter
    /// (case-insensitive) are shown. Resets the selection to the first
    /// match. Mirrors pi-tui's `setFilter`
    /// (`packages/tui/src/components/select-list.ts:60-64`)
    /// byte-for-byte: a one-shot transform that rewrites
    /// `filtered_indices` from `items` and resets `selected = 0`. The
    /// filter string is *not* retained between calls — each invocation
    /// rebuilds from `items`, the same way pi rebuilds from `this.items`.
    pub fn set_filter(&mut self, filter: &str) {
        if filter.is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
        } else {
            let filter_lower = filter.to_lowercase();
            self.filtered_indices = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.value.to_lowercase().starts_with(&filter_lower))
                .map(|(i, _)| i)
                .collect();
        }
        self.selected = 0;
    }

    /// Get the currently selected item, if any. Mirrors pi's
    /// `getSelectedItem`.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// A read-only view over all items (pre-filter). Rust-side ergonomic
    /// addition; pi-tui has no equivalent (its callers retain their own
    /// item list). Used by callers that need to look up an item index
    /// without rebuilding the input slice.
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Set the highlighted index directly, clamped to the valid range.
    /// Mirrors pi's `setSelectedIndex`.
    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered_indices.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = index.min(self.filtered_indices.len() - 1);
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let len = self.filtered_indices.len();
        if delta < 0 {
            if self.selected == 0 {
                self.selected = len - 1;
            } else {
                self.selected -= 1;
            }
        } else {
            self.selected = (self.selected + 1) % len;
        }
    }

    /// Compute the index of the first visible row, given the current
    /// selection. Mirrors the formula in pi's `select-list.ts`:
    /// `clamp(selected - max_visible / 2, 0, len - max_visible)`.
    fn visible_window_start(&self) -> usize {
        let len = self.filtered_indices.len();
        if len <= self.max_visible {
            return 0;
        }
        let half = self.max_visible / 2;
        let upper_bound = len - self.max_visible;
        self.selected.saturating_sub(half).min(upper_bound)
    }

    fn primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let lo = raw_min.min(raw_max).max(1);
        let hi = raw_min.max(raw_max).max(1);
        (lo, hi)
    }

    fn primary_column_width(&self) -> usize {
        let (min, max) = self.primary_column_bounds();
        let widest = self
            .filtered_indices
            .iter()
            .map(|&i| visible_width(self.items[i].display_value()) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        is_selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display = item.display_value();
        let candidate = match &self.layout.truncate_primary {
            Some(f) => f(TruncatePrimaryContext {
                text: display,
                max_width,
                column_width,
                item,
                is_selected,
            }),
            None => truncate_to_width(display, max_width, "", false),
        };
        // Guard against a bogus custom truncator that overshoots.
        truncate_to_width(&candidate, max_width, "", false)
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(desc) = description_single_line {
            if width > 40 {
                let effective_primary = primary_column_width
                    .min(width.saturating_sub(prefix_width + 4))
                    .max(1);
                let max_primary = effective_primary.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
                let truncated_value =
                    self.truncate_primary(item, is_selected, max_primary, effective_primary);
                let truncated_width = visible_width(&truncated_value);
                let spacing_len = effective_primary.saturating_sub(truncated_width).max(1);
                let spacing = " ".repeat(spacing_len);
                let description_start = prefix_width + truncated_width + spacing_len;
                let remaining = width.saturating_sub(description_start + 2);

                if remaining > MIN_DESCRIPTION_WIDTH {
                    let truncated_desc = truncate_to_width(desc, remaining, "", false);
                    return if is_selected {
                        let styled_prefix = (self.theme.selected_prefix)(prefix);
                        let styled_body = (self.theme.selected_text)(&format!(
                            "{}{}{}",
                            truncated_value, spacing, truncated_desc
                        ));
                        format!("{}{}", styled_prefix, styled_body)
                    } else {
                        let desc_text =
                            (self.theme.description)(&format!("{}{}", spacing, truncated_desc));
                        format!("{}{}{}", prefix, truncated_value, desc_text)
                    };
                }
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2).max(1);
        let truncated_value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            let styled_prefix = (self.theme.selected_prefix)(prefix);
            let styled_body = (self.theme.selected_text)(&truncated_value);
            format!("{}{}", styled_prefix, styled_body)
        } else {
            format!("{}{}", prefix, truncated_value)
        }
    }
}

/// Normalize a string to a single line: collapse runs of `\r`, `\n`, and
/// `\t` into single spaces and trim the result.
fn normalize_to_single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = ch == ' ';
        }
    }
    out.trim().to_string()
}

impl Component for SelectList {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if self.filtered_indices.is_empty() {
            lines.push((self.theme.no_match)("  No matching commands"));
            return lines;
        }

        let primary_column_width = self.primary_column_width();

        // Compute the visible window from the current selection. Matches
        // pi's `select-list.ts` formula exactly: window starts at
        // clamp(selected - max_visible / 2, 0, len - max_visible).
        let len = self.filtered_indices.len();
        let start_index = self.visible_window_start();
        let end_index = (start_index + self.max_visible).min(len);

        for idx in start_index..end_index {
            let item = &self.items[self.filtered_indices[idx]];
            let is_selected = idx == self.selected;
            let single_line = item.description.as_deref().map(normalize_to_single_line);
            lines.push(self.render_item(
                item,
                is_selected,
                width,
                single_line.as_deref(),
                primary_column_width,
            ));
        }

        // Scroll indicator. `(N/TOTAL)` shows the 1-based index of the
        // currently-selected item within the filtered list. Pi's render
        // gates on `start_index > 0 || end_index < len` and truncates the
        // formatted string to `width - 2` (F31 in PORTING.md).
        if start_index > 0 || end_index < len {
            let info = format!("  ({}/{})", self.selected + 1, len);
            let clamped = truncate_to_width(&info, width.saturating_sub(2), "", false);
            lines.push((self.theme.scroll_info)(&clamped));
        }

        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.up") {
            self.move_selection(-1);
            return true;
        }
        if kb.matches(event, "tui.select.down") {
            self.move_selection(1);
            return true;
        }
        if kb.matches(event, "tui.select.confirm") {
            if let Some(item) = self.selected_item().cloned() {
                if let Some(ref mut on_select) = self.on_select {
                    on_select(&item);
                }
            }
            return true;
        }
        if kb.matches(event, "tui.select.cancel") {
            if let Some(ref mut on_cancel) = self.on_cancel {
                on_cancel();
            }
            return true;
        }
        if kb.matches(event, "tui.select.pageUp") {
            for _ in 0..self.max_visible {
                self.move_selection(-1);
            }
            return true;
        }
        if kb.matches(event, "tui.select.pageDown") {
            for _ in 0..self.max_visible {
                self.move_selection(1);
            }
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity theme used by in-module tests — every closure passes its
    /// input through verbatim. Lets these tests assert on layout/structure
    /// without stripping ANSI escapes from the output. Mirrors
    /// `tests/support/themes.rs::identity_select_list_theme` so integration
    /// tests and unit tests share the same convention.
    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
        }
    }

    #[test]
    fn normalize_collapses_whitespace_runs_and_trims() {
        assert_eq!(normalize_to_single_line("hello\nworld"), "hello world");
        assert_eq!(normalize_to_single_line("\n\n hello \n\n"), "hello");
        assert_eq!(normalize_to_single_line("a\r\nb\tc\nd"), "a b c d");
    }

    #[test]
    fn primary_column_bounds_clamp_and_reorder() {
        let layout = SelectListLayout {
            min_primary_column_width: Some(20),
            max_primary_column_width: Some(12),
            truncate_primary: None,
        };
        let list = SelectList::new(vec![], 5, identity_theme(), layout);
        let (min, max) = list.primary_column_bounds();
        assert_eq!(min, 12);
        assert_eq!(max, 20);
    }

    /// `SelectList::new` (F33 follow-up; F49 finalized the constructor
    /// shape with required theme + layout) takes the theme as a required
    /// argument and applies it directly. The render path picks up the
    /// supplied `selected_prefix` / `selected_text` immediately, no
    /// `set_theme` call required.
    #[test]
    fn new_applies_supplied_theme_to_render_output() {
        // Sentinel theme: wrap the selected-prefix in `<<...>>` so the
        // render output contains an unmistakable marker.
        let theme = SelectListTheme {
            selected_prefix: Arc::new(|s| format!("<<{}>>", s)),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
        };
        let items = vec![SelectItem::new("a", "alpha"), SelectItem::new("b", "bravo")];
        let mut list = SelectList::new(items, 5, theme, SelectListLayout::default());
        let lines = list.render(40);
        // The first row is the selected item; with our sentinel
        // prefix wrapper it must contain `<<` and `>>`.
        let first = &lines[0];
        assert!(
            first.contains("<<") && first.contains(">>"),
            "new() theme selected_prefix must be applied: got {first:?}",
        );
    }
}
