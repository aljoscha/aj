//! Navigable selection list component.
//!
//! Supports filtering, per-item descriptions aligned into a shared column,
//! configurable primary-column bounds, and an optional custom primary
//! truncator.

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
pub struct SelectListTheme {
    /// Style for the prefix (e.g. "→ ") placed before the selected item.
    pub selected_prefix: Box<dyn Fn(&str) -> String>,
    /// Style for the selected item text.
    pub selected_text: Box<dyn Fn(&str) -> String>,
    /// Style for description text.
    pub description: Box<dyn Fn(&str) -> String>,
    /// Style for scroll indicator.
    pub scroll_info: Box<dyn Fn(&str) -> String>,
    /// Style for "no matches" text.
    pub no_match: Box<dyn Fn(&str) -> String>,
}

impl Default for SelectListTheme {
    fn default() -> Self {
        Self {
            selected_prefix: Box::new(|s| format!("\x1b[36m{}\x1b[0m", s)),
            selected_text: Box::new(|s| format!("\x1b[1;36m{}\x1b[0m", s)),
            description: Box::new(|s| format!("\x1b[90m{}\x1b[0m", s)),
            scroll_info: Box::new(|s| format!("\x1b[90m{}\x1b[0m", s)),
            no_match: Box::new(|s| format!("\x1b[90m{}\x1b[0m", s)),
        }
    }
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
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_indices: Vec<usize>,
    selected: usize,
    scroll_offset: usize,
    max_visible: usize,
    filter: String,
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
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filtered_indices,
            selected: 0,
            scroll_offset: 0,
            max_visible,
            filter: String::new(),
            theme: SelectListTheme::default(),
            layout: SelectListLayout::default(),
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    /// Set the theme.
    pub fn set_theme(&mut self, theme: SelectListTheme) {
        self.theme = theme;
    }

    /// Replace the layout options.
    pub fn set_layout(&mut self, layout: SelectListLayout) {
        self.layout = layout;
    }

    /// Set the filter text. Items whose value starts with the filter
    /// (case-insensitive) are shown. Resets the selection to the first
    /// match.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.apply_filter();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected item, if any.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// A read-only view over all items (pre-filter).
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Set the items list.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.apply_filter();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Set the highlighted index directly, clamped to the valid range.
    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered_indices.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = index.min(self.filtered_indices.len() - 1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        if self.selected >= self.scroll_offset + self.max_visible {
            self.scroll_offset = self.selected + 1 - self.max_visible;
        }
    }

    fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
        } else {
            let filter_lower = self.filter.to_lowercase();
            self.filtered_indices = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.value.to_lowercase().starts_with(&filter_lower))
                .map(|(i, _)| i)
                .collect();
        }
        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len().saturating_sub(1);
        }
        if self.scroll_offset > self.selected {
            self.scroll_offset = self.selected;
        }
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
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        if self.selected >= self.scroll_offset + self.max_visible {
            self.scroll_offset = self.selected + 1 - self.max_visible;
        }
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
        let visible_count = self.filtered_indices.len().min(self.max_visible);

        for i in 0..visible_count {
            let idx = self.scroll_offset + i;
            if idx >= self.filtered_indices.len() {
                break;
            }
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

        // Scroll indicator. `(N/TOTAL)` shows the 1-based index of
        // the currently-selected item within the filtered list, so the
        // format matches `SettingsList` and stays compact in narrow
        // overlays.
        let total = self.filtered_indices.len();
        if total > self.max_visible {
            let info = format!("  ({}/{})", self.selected + 1, total);
            lines.push((self.theme.scroll_info)(&info));
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

    #[test]
    fn normalize_collapses_whitespace_runs_and_trims() {
        assert_eq!(normalize_to_single_line("hello\nworld"), "hello world");
        assert_eq!(normalize_to_single_line("\n\n hello \n\n"), "hello");
        assert_eq!(normalize_to_single_line("a\r\nb\tc\nd"), "a b c d");
    }

    #[test]
    fn primary_column_bounds_clamp_and_reorder() {
        let mut list = SelectList::new(vec![], 5);
        list.layout = SelectListLayout {
            min_primary_column_width: Some(20),
            max_primary_column_width: Some(12),
            truncate_primary: None,
        };
        let (min, max) = list.primary_column_bounds();
        assert_eq!(min, 12);
        assert_eq!(max, 20);
    }
}
