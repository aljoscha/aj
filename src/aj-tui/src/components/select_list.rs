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
const PREFIX_COLUMN_GAP: usize = 2;
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
    /// Optional left-side category label rendered in the dim prefix style.
    /// When any item in the list sets this, a right-aligned prefix column
    /// is reserved to the left of the primary column.
    pub prefix: Option<String>,
    /// Optional right-side accent label (e.g. a key combo). Takes the
    /// right column slot in preference to `description`; the two never
    /// coexist on the same row.
    pub shortcut: Option<String>,
    /// Optional text the fuzzy filter ([`SelectList::set_filter`])
    /// matches against. When unset the filter matches the displayed
    /// label. Set this when the searchable text differs from what's
    /// shown — e.g. the command palette filters on `"<category>
    /// <name>"` so typing a category surfaces the whole group.
    pub filter_key: Option<String>,
}

impl SelectItem {
    pub fn new(value: &str, label: &str) -> Self {
        Self {
            value: value.to_string(),
            label: label.to_string(),
            description: None,
            prefix: None,
            shortcut: None,
            filter_key: None,
        }
    }

    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = Some(desc.to_string());
        self
    }

    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.prefix = Some(prefix.to_string());
        self
    }

    pub fn with_shortcut(mut self, shortcut: &str) -> Self {
        self.shortcut = Some(shortcut.to_string());
        self
    }

    pub fn with_filter_key(mut self, key: &str) -> Self {
        self.filter_key = Some(key.to_string());
        self
    }

    fn display_value(&self) -> &str {
        if self.label.is_empty() {
            self.value.as_str()
        } else {
            self.label.as_str()
        }
    }

    /// Text the fuzzy filter matches against: the explicit
    /// [`Self::filter_key`] when set, else the displayed label.
    fn filter_text(&self) -> &str {
        self.filter_key
            .as_deref()
            .unwrap_or_else(|| self.display_value())
    }
}

/// Theme for the selection list.
///
/// The agent layer is expected to assemble the closures from its
/// central palette and pass the populated theme in at construction
/// time. We deliberately do not provide a `Default` impl: the tui
/// crate stays palette-agnostic, and tests build themes via
/// `tests/support/themes.rs`.
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
    /// Style for the left-side category prefix column (typically dim).
    /// Stays in this style even when the row is selected — the selection
    /// highlight applies to the primary label, not the metadata columns.
    pub prefix: Arc<dyn Fn(&str) -> String>,
    /// Style for the right-side shortcut/key-combo label (typically accent).
    pub shortcut: Arc<dyn Fn(&str) -> String>,
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

/// How a query is matched against items to produce the visible subset.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FilterMode {
    /// Fuzzy subsequence ranking via nucleo. Each whitespace-separated
    /// query token must subsequence-match the item text; results are
    /// reordered best-match-first. Suited to short labels (command
    /// palette, session list) where typing a few characters should
    /// surface the closest entries.
    #[default]
    Fuzzy,
    /// Case-insensitive "contains all tokens": each whitespace-separated
    /// query token must appear as a literal substring of the item text.
    /// Matching items keep their original order (no re-ranking). Suited
    /// to searching long bodies (prompt history) where subsequence
    /// matching is too permissive and the user expects "the entries
    /// that contain these words".
    SubstringAllTokens,
}

/// Layout tuning knobs for the primary column and description alignment.
pub struct SelectListLayout {
    /// Minimum width (including the 2-char gap) of the primary column.
    pub min_primary_column_width: Option<usize>,
    /// Maximum width (including the 2-char gap) of the primary column.
    pub max_primary_column_width: Option<usize>,
    /// Override the default primary-column truncation. The default is
    /// hard truncation with no ellipsis. Return value must fit in
    /// `context.max_width`; if it overflows, it will be re-truncated.
    pub truncate_primary: Option<Box<dyn Fn(TruncatePrimaryContext<'_>) -> String>>,
    /// Cap on the prefix column width. Default unbounded — the column is
    /// sized to the widest prefix across all items.
    pub max_prefix_column_width: Option<usize>,
    /// When `true` (the default), the selected row is prefixed with `→ `
    /// and styled via `selected_text`. When `false`, every row gets a
    /// uniform `  ` prefix and the `selected_text` style is suppressed
    /// — used by the read-only help overlay where there is no
    /// interactive selection to highlight.
    pub show_selection_indicator: bool,
    /// When `true` (default), Up at the first row wraps to the last row
    /// and Down at the last row wraps to the first. When `false`,
    /// navigation clamps at the ends so holding a key settles on the
    /// top/bottom row.
    pub wrap_selection: bool,
    /// Strategy used by [`SelectList::set_filter`]. Defaults to
    /// [`FilterMode::Fuzzy`].
    pub filter_mode: FilterMode,
    /// Message shown (in the `no_match` style) when the filter excludes
    /// every item. Defaults to `"No matching commands"`.
    pub empty_message: String,
}

impl Default for SelectListLayout {
    fn default() -> Self {
        Self {
            min_primary_column_width: None,
            max_primary_column_width: None,
            truncate_primary: None,
            max_prefix_column_width: None,
            show_selection_indicator: true,
            wrap_selection: true,
            filter_mode: FilterMode::Fuzzy,
            empty_message: "No matching commands".to_string(),
        }
    }
}

/// A navigable selection list.
///
/// # Scroll model
///
/// The visible window is recomputed from the current selection on every
/// render. Selection is the only piece of scroll-affecting state;
/// there's no persistent `scroll_offset` field.
///
/// The window starts at
/// `clamp(selected - max_visible / 2, 0, len - max_visible)` and runs for
/// `max_visible` rows, so the selection floats near the center of the
/// visible region whenever there's room and clamps to the top/bottom edge
/// near the ends of the list. Wraparound (Up at index `0`, Down at
/// `len - 1`, enabled by [`SelectListLayout::wrap_selection`]) only
/// changes `selected`; the next render naturally lands the new selection
/// at the appropriate end.
///
/// `set_filter` resets `selected` to `0`.
///
/// # Constructor shape
///
/// [`SelectList::new`] takes the theme and layout at construction time
/// and they are immutable for the life of the instance — there are no
/// `set_theme` / `set_layout` mutators. Rebuild the list to change
/// those or to replace the item set wholesale. Items may be *appended*
/// in place with [`SelectList::extend_items`] (for lists that load
/// incrementally); appending keeps the active filter and selection.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_indices: Vec<usize>,
    selected: usize,
    /// The active (trimmed) filter query, retained so the visible set
    /// can be recomputed when items are appended via
    /// [`SelectList::extend_items`]. Empty means "show everything".
    filter: String,
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
    /// The theme is taken as a required argument so the tui crate stays
    /// palette-agnostic; build the theme from the agent's central
    /// palette (or a test fixture) and pass it in. Layout is also
    /// required — pass [`SelectListLayout::default`] to use the
    /// built-in column bounds. Both are immutable for the life of the
    /// instance: rebuild the list to change them.
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
            filter: String::new(),
            max_visible,
            theme,
            layout,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    /// Filter the visible rows by fuzzy-matching `filter` against each
    /// item's [`SelectItem::filter_text`] (the explicit `filter_key`
    /// when set, else the displayed label). Matching rows are reordered
    /// best-match-first; the selection resets to the top match.
    ///
    /// An empty (or whitespace-only) `filter` restores the full list in
    /// its original order. The trimmed query is retained internally so
    /// [`SelectList::extend_items`] can keep newly-appended rows
    /// consistent with it; each call still recomputes `filtered_indices`
    /// from scratch.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.trim().to_string();
        self.apply_filter();
        self.selected = 0;
    }

    /// Recompute `filtered_indices` from the current items and the
    /// retained [`Self::filter`], per the configured
    /// [`SelectListLayout::filter_mode`]. Leaves `selected` untouched
    /// (callers reset or restore it as appropriate).
    fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
            return;
        }
        match self.layout.filter_mode {
            FilterMode::Fuzzy => {
                let candidates: Vec<(usize, &str)> = self
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| (i, item.filter_text()))
                    .collect();
                let ranked =
                    crate::fuzzy::fuzzy_filter(candidates, &self.filter, |(_, text)| *text);
                self.filtered_indices = ranked.into_iter().map(|(i, _)| i).collect();
            }
            FilterMode::SubstringAllTokens => {
                // Keep items in their original order; an entry is visible
                // when every whitespace-separated query token appears as a
                // case-insensitive substring of its text.
                let needle = self.filter.to_lowercase();
                let tokens: Vec<&str> = needle.split_whitespace().collect();
                self.filtered_indices = self
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        let hay = item.filter_text().to_lowercase();
                        tokens.iter().all(|token| hay.contains(token))
                    })
                    .map(|(i, _)| i)
                    .collect();
            }
        }
    }

    /// Append `new_items` to the list, keeping it consistent with the
    /// active filter and preserving the highlighted row. Intended for
    /// lists that load incrementally.
    ///
    /// With no active filter this is O(`new_items`): the rows are
    /// appended in order, revealed at the bottom, and the selection is
    /// untouched. With an active filter the visible set is re-ranked
    /// (new rows may interleave with existing matches) and the
    /// previously-highlighted row is restored when it still matches.
    pub fn extend_items(&mut self, new_items: impl IntoIterator<Item = SelectItem>) {
        let first_new = self.items.len();
        self.items.extend(new_items);
        if first_new == self.items.len() {
            return;
        }

        if self.filter.is_empty() {
            // Order is stable and the new rows go at the end, so the
            // current selection index still points at the same item.
            self.filtered_indices.extend(first_new..self.items.len());
        } else {
            let selected_value = self.selected_item().map(|item| item.value.clone());
            self.apply_filter();
            self.selected = 0;
            if let Some(value) = selected_value {
                self.select_by_value(&value);
            }
        }
    }

    /// Get the currently selected item, if any.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// A read-only view over all items (pre-filter). Used by callers
    /// that need to look up an item index without rebuilding the input
    /// slice.
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Update the maximum number of rows shown at once. The next render
    /// uses the new window size; the selection is re-clamped on the next
    /// render. Floored at 1 so the list always shows at least one row.
    pub fn set_max_visible(&mut self, max_visible: usize) {
        self.max_visible = max_visible.max(1);
    }

    /// Set the highlighted index directly, clamped to the valid range.
    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered_indices.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = index.min(self.filtered_indices.len() - 1);
    }

    /// Move the selection to the first visible row whose
    /// [`SelectItem::value`] equals `value`, returning whether a match
    /// was found. Used to preserve the user's selection across a
    /// rebuild (e.g. when a list is repopulated incrementally).
    pub fn select_by_value(&mut self, value: &str) -> bool {
        match self
            .filtered_indices
            .iter()
            .position(|&i| self.items[i].value == value)
        {
            Some(pos) => {
                self.selected = pos;
                true
            }
            None => false,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let len = self.filtered_indices.len();
        let previous = self.selected;
        if delta < 0 {
            if self.selected == 0 {
                self.selected = if self.layout.wrap_selection {
                    len - 1
                } else {
                    0
                };
            } else {
                self.selected -= 1;
            }
        } else if self.selected + 1 < len {
            self.selected += 1;
        } else {
            self.selected = if self.layout.wrap_selection {
                0
            } else {
                len - 1
            };
        }
        // When clamping leaves the selection unchanged, there is no
        // change to notify.
        if self.selected != previous {
            self.notify_selection_change();
        }
    }

    fn notify_selection_change(&mut self) {
        // Take the callback out so we can hand `&self` to it without
        // overlapping borrows; restore it afterwards. The callback
        // doesn't re-enter `SelectList`, so a one-shot swap is safe.
        if let Some(item) = self.selected_item().cloned()
            && let Some(mut cb) = self.on_selection_change.take()
        {
            cb(&item);
            self.on_selection_change = Some(cb);
        }
    }

    /// Compute the index of the first visible row, given the current
    /// selection: `clamp(selected - max_visible / 2, 0, len - max_visible)`.
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

    /// Width of the right-aligned prefix column. Returns `0` when no
    /// item carries a `prefix` — callers must then skip both the column
    /// and its gap.
    ///
    /// Width is sized to the widest prefix across **all items**, not
    /// just the visible/filtered subset, so the label column stays in a
    /// stable horizontal position as the user filters.
    fn prefix_column_width(&self) -> usize {
        let widest = self
            .items
            .iter()
            .filter_map(|item| item.prefix.as_deref())
            .map(visible_width)
            .max()
            .unwrap_or(0);
        match self.layout.max_prefix_column_width {
            Some(cap) => widest.min(cap),
            None => widest,
        }
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

    /// Render the prefix column block (right-aligned prefix text + 2-space
    /// gap) and return `(rendered, consumed_width)`. Returns empty when
    /// `prefix_column_width == 0`.
    fn render_prefix_block(
        &self,
        item: &SelectItem,
        prefix_column_width: usize,
    ) -> (String, usize) {
        if prefix_column_width == 0 {
            return (String::new(), 0);
        }
        let raw = item.prefix.as_deref().unwrap_or("");
        let truncated = truncate_to_width(raw, prefix_column_width, "", false);
        let pad = prefix_column_width.saturating_sub(visible_width(&truncated));
        let padded = format!("{}{}", " ".repeat(pad), truncated);
        let styled = (self.theme.prefix)(&padded);
        let consumed = prefix_column_width + PREFIX_COLUMN_GAP;
        (
            format!("{}{}", styled, " ".repeat(PREFIX_COLUMN_GAP)),
            consumed,
        )
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
        prefix_column_width: usize,
    ) -> String {
        let show_indicator = self.layout.show_selection_indicator;
        let arrow = if show_indicator && is_selected {
            "→ "
        } else {
            "  "
        };
        let arrow_width = visible_width(arrow);

        let (prefix_block, prefix_consumed) = self.render_prefix_block(item, prefix_column_width);
        let leading_width = arrow_width + prefix_consumed;
        let arrow_styled = if show_indicator && is_selected {
            (self.theme.selected_prefix)(arrow)
        } else {
            arrow.to_string()
        };
        // Suppress the selected-row styling when the indicator is
        // hidden — there is no "focus" cue to communicate.
        let style_as_selected = show_indicator && is_selected;

        // Right-column content: shortcut takes precedence over description.
        // The two never coexist on the same row.
        enum Right<'a> {
            Shortcut(&'a str),
            Description(&'a str),
            None,
        }
        let right = if let Some(sc) = item.shortcut.as_deref() {
            Right::Shortcut(sc)
        } else if let Some(desc) = description_single_line {
            Right::Description(desc)
        } else {
            Right::None
        };

        if !matches!(right, Right::None) && width > 40 {
            let effective_primary = primary_column_width
                .min(width.saturating_sub(leading_width + 4))
                .max(1);
            let max_primary = effective_primary.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated_value =
                self.truncate_primary(item, style_as_selected, max_primary, effective_primary);
            let truncated_width = visible_width(&truncated_value);
            let spacing_len = effective_primary.saturating_sub(truncated_width).max(1);
            let spacing = " ".repeat(spacing_len);
            let right_start = leading_width + truncated_width + spacing_len;
            let remaining = width.saturating_sub(right_start + 2);

            if remaining > MIN_DESCRIPTION_WIDTH {
                // A truncated description gets an ellipsis so the cut
                // is visible; a shortcut never needs one (it's short
                // and shouldn't be elided mid-combo).
                let (right_text_raw, right_style, ellipsis) = match right {
                    Right::Shortcut(s) => (s, &self.theme.shortcut, ""),
                    Right::Description(s) => (s, &self.theme.description, "…"),
                    Right::None => unreachable!(),
                };
                let truncated_right = truncate_to_width(right_text_raw, remaining, ellipsis, false);
                let primary_text = if style_as_selected {
                    (self.theme.selected_text)(&truncated_value)
                } else {
                    truncated_value
                };
                let right_styled = right_style(&truncated_right);
                return format!(
                    "{}{}{}{}{}",
                    arrow_styled, prefix_block, primary_text, spacing, right_styled
                );
            }
        }

        let max_width = width.saturating_sub(leading_width + 2).max(1);
        let truncated_value = self.truncate_primary(item, style_as_selected, max_width, max_width);
        let primary_text = if style_as_selected {
            (self.theme.selected_text)(&truncated_value)
        } else {
            truncated_value
        };
        format!("{}{}{}", arrow_styled, prefix_block, primary_text)
    }
}

/// Normalize a string to a single line: collapse runs of `\r` and `\n`
/// into single spaces and trim the result.
fn normalize_to_single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_newline = false;
    for ch in text.chars() {
        if matches!(ch, '\n' | '\r') {
            if !last_was_newline {
                out.push(' ');
                last_was_newline = true;
            }
        } else {
            out.push(ch);
            last_was_newline = false;
        }
    }
    out.trim().to_string()
}

impl Component for SelectList {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if self.filtered_indices.is_empty() {
            lines.push((self.theme.no_match)(&format!(
                "  {}",
                self.layout.empty_message
            )));
            return lines;
        }

        let primary_column_width = self.primary_column_width();
        let prefix_column_width = self.prefix_column_width();

        // Compute the visible window from the current selection:
        // window starts at clamp(selected - max_visible / 2, 0, len - max_visible).
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
                prefix_column_width,
            ));
        }

        // Scroll indicator. `(N/TOTAL)` shows the 1-based index of the
        // currently-selected item within the filtered list. Only shown
        // when the visible window doesn't already cover the whole list,
        // and truncated to `width - 2` to keep narrow overlays readable.
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
    /// without stripping ANSI escapes from the output. Matches the
    /// integration-test fixture in `tests/support/themes.rs::identity_select_list_theme`.
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

    #[test]
    fn substring_all_tokens_requires_every_token_and_keeps_order() {
        let layout = SelectListLayout {
            filter_mode: FilterMode::SubstringAllTokens,
            ..Default::default()
        };
        let items = vec![
            SelectItem::new("0", "fix the parser bug"),
            SelectItem::new("1", "add a parser test"),
            SelectItem::new("2", "refactor the bug report"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), layout);

        // Every token must appear as a substring; matches keep their
        // input order (not re-ranked).
        list.set_filter("parser bug");
        let visible: Vec<&str> = list
            .filtered_indices
            .iter()
            .map(|&i| list.items[i].value.as_str())
            .collect();
        assert_eq!(visible, vec!["0"]);

        // Case-insensitive.
        list.set_filter("PARSER");
        let visible: Vec<&str> = list
            .filtered_indices
            .iter()
            .map(|&i| list.items[i].value.as_str())
            .collect();
        assert_eq!(visible, vec!["0", "1"]);

        // "prsr" is a subsequence of "parser" but not a substring, so it
        // matches nothing here (it would under fuzzy mode).
        list.set_filter("prsr");
        assert!(list.filtered_indices.is_empty());
    }

    #[test]
    fn extend_items_unfiltered_appends_and_keeps_selection() {
        let items = vec![SelectItem::new("a", "alpha"), SelectItem::new("b", "bravo")];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        // Highlight the second row, then stream more rows in.
        list.set_selected_index(1);

        list.extend_items(vec![
            SelectItem::new("c", "charlie"),
            SelectItem::new("d", "delta"),
        ]);

        // Appended at the end, in order, and the selection is untouched.
        assert_eq!(list.items().len(), 4);
        assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("b"));
        let visible: Vec<&str> = list
            .filtered_indices
            .iter()
            .map(|&i| list.items[i].value.as_str())
            .collect();
        assert_eq!(visible, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn extend_items_filtered_reranks_and_restores_selection() {
        let items = vec![
            SelectItem::new("apple", "apple"),
            SelectItem::new("apricot", "apricot"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        list.set_filter("ap");
        // Highlight "apricot" within the filtered set.
        assert!(list.select_by_value("apricot"));

        // Stream in a new matching row and a non-matching one.
        list.extend_items(vec![
            SelectItem::new("apex", "apex"),
            SelectItem::new("banana", "banana"),
        ]);

        // The filter still excludes the non-match, the new match is
        // visible, and the highlight stays on "apricot".
        let visible: Vec<&str> = list
            .filtered_indices
            .iter()
            .map(|&i| list.items[i].value.as_str())
            .collect();
        assert!(visible.contains(&"apex"), "new match visible: {visible:?}");
        assert!(
            !visible.contains(&"banana"),
            "non-match hidden: {visible:?}"
        );
        assert_eq!(
            list.selected_item().map(|i| i.value.as_str()),
            Some("apricot")
        );
    }

    #[test]
    fn select_by_value_moves_to_matching_visible_row() {
        let items = vec![
            SelectItem::new("a", "alpha"),
            SelectItem::new("b", "bravo"),
            SelectItem::new("c", "charlie"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());

        assert!(list.select_by_value("c"));
        assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("c"));

        // A value that isn't present leaves the selection untouched.
        assert!(!list.select_by_value("missing"));
        assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("c"));
    }

    #[test]
    fn normalize_collapses_newline_runs_and_trims() {
        assert_eq!(normalize_to_single_line("hello\nworld"), "hello world");
        assert_eq!(normalize_to_single_line("\n\n hello \n\n"), "hello");
        assert_eq!(normalize_to_single_line("a\r\nb\nc"), "a b c");
    }

    #[test]
    fn primary_column_bounds_clamp_and_reorder() {
        let layout = SelectListLayout {
            min_primary_column_width: Some(20),
            max_primary_column_width: Some(12),
            truncate_primary: None,
            max_prefix_column_width: None,
            show_selection_indicator: true,
            wrap_selection: true,
            ..Default::default()
        };
        let list = SelectList::new(vec![], 5, identity_theme(), layout);
        let (min, max) = list.primary_column_bounds();
        assert_eq!(min, 12);
        assert_eq!(max, 20);
    }

    /// `SelectList::new` takes the theme as a required argument and
    /// applies it directly. The render path picks up the supplied
    /// `selected_prefix` / `selected_text` immediately, no `set_theme`
    /// call required.
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
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
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

    /// When any item carries a `prefix`, the prefix column is rendered
    /// right-aligned within a column sized to the widest prefix. Shorter
    /// prefixes get leading spaces so the column edges line up.
    #[test]
    fn prefix_column_renders_when_set() {
        let items = vec![
            SelectItem::new("a", "alpha").with_prefix("model"),
            SelectItem::new("b", "bravo").with_prefix("session"),
            SelectItem::new("c", "charlie").with_prefix("aj"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        let lines = list.render(80);
        assert_eq!(lines.len(), 3);
        // Widest prefix is "session" (7). Each row must contain the
        // prefix right-aligned in a 7-wide column, followed by the
        // 2-space gap, followed by the label. Selected row uses "→ ",
        // others use "  ".
        assert!(lines[0].starts_with("→ "), "selected arrow: {:?}", lines[0]);
        assert!(lines[0].contains("  model  alpha"), "row 0: {:?}", lines[0]);
        assert!(lines[1].contains("session  bravo"), "row 1: {:?}", lines[1]);
        assert!(
            lines[2].contains("     aj  charlie"),
            "row 2: {:?}",
            lines[2]
        );
    }

    /// When no item carries a prefix, the prefix column (and its gap)
    /// disappear entirely — rendering is byte-identical to a list built
    /// without prefixes at all.
    #[test]
    fn no_prefix_column_when_all_empty() {
        let items = vec![SelectItem::new("a", "alpha"), SelectItem::new("b", "bravo")];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        let lines = list.render(80);
        // Selected row: arrow then label, no leading prefix gutter.
        assert!(lines[0].starts_with("→ alpha"));
        assert!(lines[1].starts_with("  bravo"));
    }

    /// When an item carries both `shortcut` and `description`, the
    /// shortcut wins the right column slot.
    #[test]
    fn shortcut_takes_right_column_over_description() {
        let items = vec![
            SelectItem::new("a", "alpha")
                .with_description("do alpha things")
                .with_shortcut("Ctrl+A"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        let lines = list.render(80);
        assert!(
            lines[0].contains("Ctrl+A"),
            "shortcut must render: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains("do alpha things"),
            "description must be suppressed when shortcut is set: {:?}",
            lines[0],
        );
    }

    /// On a selected row the primary label gets the `selected_text` style,
    /// but the prefix column stays in the dim `prefix` style. With
    /// identity themes we can verify structurally that the prefix text
    /// still appears verbatim on the selected line (i.e. the prefix
    /// closure ran, not some wrapping selected-text closure).
    #[test]
    fn selected_row_keeps_prefix_dim() {
        // Sentinel theme: tag selected-text with `[[...]]` and prefix
        // with `<<...>>`. If selection wrapping leaked into the prefix
        // column, we would see `model` inside `[[ ... ]]`.
        let theme = SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| format!("[[{}]]", s)),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| format!("<<{}>>", s)),
            shortcut: Arc::new(|s| s.to_string()),
        };
        let items = vec![
            SelectItem::new("a", "alpha").with_prefix("model"),
            SelectItem::new("b", "bravo").with_prefix("aj"),
        ];
        let mut list = SelectList::new(items, 5, theme, SelectListLayout::default());
        let lines = list.render(80);
        // Selected row: prefix wrapped by `prefix` closure, primary by
        // `selected_text` closure. They must not be nested.
        let first = &lines[0];
        assert!(
            first.contains("<<model>>"),
            "prefix must use prefix style: {first:?}"
        );
        assert!(
            first.contains("[[alpha]]"),
            "selected primary must use selected_text: {first:?}",
        );
        // The prefix text `model` must not appear inside the
        // selected_text wrapping.
        assert!(
            !first.contains("[[<<model>>"),
            "prefix must not be wrapped by selected_text: {first:?}",
        );
    }

    #[test]
    fn hides_selection_indicator_when_layout_says_so() {
        let items = vec![SelectItem::new("a", "alpha"), SelectItem::new("b", "bravo")];
        let layout = SelectListLayout {
            show_selection_indicator: false,
            ..Default::default()
        };
        let mut list = SelectList::new(items, 5, identity_theme(), layout);
        let lines = list.render(40);
        assert!(!lines[0].contains('→'), "row 0: {:?}", lines[0]);
        assert!(lines[0].starts_with("  "), "row 0: {:?}", lines[0]);
    }

    #[test]
    fn prefix_column_stays_stable_when_filter_narrows() {
        let items = vec![
            SelectItem::new("a", "alpha").with_prefix("model"),
            SelectItem::new("b", "bravo").with_prefix("session"), // widest prefix
            SelectItem::new("c", "charlie").with_prefix("aj"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());

        let unfiltered = list.render(80);
        // Filter (by `value`) down to only the "aj"-prefixed row ("c" /
        // "charlie"). `set_filter` matches item.value, not the
        // displayed label, so we filter on "c".
        list.set_filter("c");
        let filtered = list.render(80);

        // Both renders should place the label "alpha"/"charlie" at the
        // same column position because the prefix column is sized to
        // the widest prefix across ALL items (i.e. "session" = 7), not
        // just the visible ones.
        let unfiltered_first = unfiltered[0].clone();
        let filtered_first = filtered[0].clone();

        // Strip ANSI for the assertion if needed — identity_theme()
        // passes through verbatim, so structural matching works on the
        // raw strings.
        let alpha_pos = unfiltered_first
            .find("alpha")
            .expect("unfiltered has alpha");
        let charlie_pos = filtered_first
            .find("charlie")
            .expect("filtered has charlie");
        assert_eq!(
            alpha_pos, charlie_pos,
            "label column shifted between unfiltered ({:?}) and filtered ({:?})",
            unfiltered_first, filtered_first
        );
    }

    /// A description too wide for its column is truncated with a
    /// trailing ellipsis so the cut is visible; a shortcut is not.
    #[test]
    fn description_truncates_with_ellipsis() {
        let items =
            vec![SelectItem::new("a", "alpha").with_description(
                "a fairly long description that will not fit in a narrow column",
            )];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        let lines = list.render(60);
        assert!(lines[0].contains('…'), "expected ellipsis: {:?}", lines[0]);
    }

    /// Fuzzy `set_filter` narrows and reorders by match quality. A
    /// query that fuzzy-matches a single item's label leaves only that
    /// row; an empty query restores the full list.
    #[test]
    fn set_filter_fuzzy_narrows_and_restores() {
        let items = vec![
            SelectItem::new("a", "alpha"),
            SelectItem::new("b", "bravo"),
            SelectItem::new("c", "charlie"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());

        list.set_filter("char");
        assert_eq!(list.render(80).len(), 1);
        assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("c"));

        list.set_filter("");
        assert_eq!(list.render(80).len(), 3);
    }

    /// `set_filter` matches `filter_key` when set, not the displayed
    /// label — so a row can be found by hidden search text (e.g. the
    /// command palette's `"<category> <name>"` key).
    #[test]
    fn set_filter_matches_filter_key_over_label() {
        let items = vec![
            SelectItem::new("a", "list").with_filter_key("model list"),
            SelectItem::new("b", "switch").with_filter_key("session switch"),
        ];
        let mut list = SelectList::new(items, 5, identity_theme(), SelectListLayout::default());
        // "model" appears only in the first row's filter key, not its
        // label, so the row is still found.
        list.set_filter("model");
        assert_eq!(list.render(80).len(), 1);
        assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("a"));
    }

    fn three_item_list(wrap_selection: bool) -> SelectList {
        let items = vec![
            SelectItem::new("a", "alpha"),
            SelectItem::new("b", "bravo"),
            SelectItem::new("c", "charlie"),
        ];
        let layout = SelectListLayout {
            wrap_selection,
            ..Default::default()
        };
        SelectList::new(items, 5, identity_theme(), layout)
    }

    /// With `wrap_selection: false`, Up at the first row stays on the
    /// first row and Down on the last row stays on the last row.
    #[test]
    fn no_wrap_clamps_at_ends() {
        let mut list = three_item_list(false);

        // Up at index 0 stays at 0.
        list.move_selection(-1);
        assert_eq!(list.selected, 0);

        // Down to the last row, then Down again clamps there.
        list.move_selection(1);
        list.move_selection(1);
        assert_eq!(list.selected, 2);
        list.move_selection(1);
        assert_eq!(list.selected, 2);
    }

    /// The default (`wrap_selection: true`) still wraps around at both
    /// ends.
    #[test]
    fn default_wraps_at_ends() {
        let mut list = three_item_list(true);

        // Up at index 0 wraps to the last row.
        list.move_selection(-1);
        assert_eq!(list.selected, 2);

        // Down at the last row wraps back to the first.
        list.move_selection(1);
        assert_eq!(list.selected, 0);
    }
}
