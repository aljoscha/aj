//! Scrollable list of named settings with per-item value cycling and
//! optional fuzzy-search filtering.
//!
//! Each [`SettingItem`] declares a label, the current value, and
//! optionally a list of cycleable values or a submenu factory. Up/Down
//! navigate; Enter or Space advances the highlighted item to its next
//! value, or opens its submenu if one is provided. Escape fires the
//! on-cancel callback.
//!
//! # Submenus
//!
//! Setting [`SettingItem::submenu`] to a factory closure makes Enter /
//! Space open a nested picker instead of cycling a value. The factory
//! is handed the item's current value (so it can pre-select it) and a
//! `done` callback; when the submenu calls `done(Some(value))` the
//! parent item's current value updates and the parent list is
//! redisplayed, while `done(None)` closes the submenu without making
//! any change. While the submenu is open, the list delegates every
//! render and input event to the submenu component.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::component::Component;
use crate::components::text_input::Input;
use crate::fuzzy::fuzzy_filter;
use crate::keybindings;
use crate::keys::InputEvent;

/// Callback passed to a [`SettingItem::submenu`] factory. The submenu
/// calls this with `Some(value)` to commit a selection (which updates
/// the parent item's current value and fires the parent's on-change
/// callback) or with `None` to cancel without making any change. In
/// either case the parent list closes the submenu on the next input
/// event.
pub type SubmenuDoneCallback = Box<dyn Fn(Option<String>)>;

/// Factory for a submenu component. Receives the parent item's current
/// value (so the submenu can pre-select it) and a `done` callback that
/// the submenu must eventually call exactly once.
pub type SubmenuFactory = Box<dyn Fn(&str, SubmenuDoneCallback) -> Box<dyn Component>>;

/// One entry in a [`SettingsList`].
pub struct SettingItem {
    /// Unique identifier, surfaced in the on-change callback.
    pub id: String,
    /// Display label on the left.
    pub label: String,
    /// Optional longer description shown when this item is selected.
    pub description: Option<String>,
    /// Current value shown on the right.
    pub current_value: String,
    /// If provided, Enter/Space cycles through these values.
    pub values: Option<Vec<String>>,
    /// If provided, Enter/Space opens a submenu built by this factory
    /// instead of cycling `values`. When both `values` and `submenu`
    /// are set, `submenu` wins.
    pub submenu: Option<SubmenuFactory>,
}

impl SettingItem {
    /// Shorthand constructor for a cycleable boolean-style item.
    pub fn cycleable(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
        values: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: Some(values),
            submenu: None,
        }
    }

    /// Shorthand constructor for a submenu-backed item.
    pub fn with_submenu(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
        submenu: SubmenuFactory,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: None,
            submenu: Some(submenu),
        }
    }
}

/// Styling hooks for [`SettingsList`]. Each closure returns the text
/// wrapped in ANSI escapes (or passes through unchanged, for identity
/// themes used in tests).
///
/// Mirrors pi-tui's `SettingsListTheme` interface
/// (`packages/tui/src/components/settings-list.ts`). Pi-tui ships no
/// upstream default theme — the agent layer builds one from its central
/// palette and passes it to [`SettingsList::new`]. We deliberately do
/// not provide a `Default` impl: the tui crate stays palette-agnostic,
/// and tests build themes via `tests/support/themes.rs` (mirroring
/// pi's `packages/tui/test/test-themes.ts`).
///
/// The closures use `Arc` rather than `Box` so a single theme can be
/// cheaply cloned (e.g. for snapshot purposes or sharing with a sibling
/// component).
#[derive(Clone)]
pub struct SettingsListTheme {
    pub label: Arc<dyn Fn(&str, bool) -> String>,
    pub value: Arc<dyn Fn(&str, bool) -> String>,
    pub description: Arc<dyn Fn(&str) -> String>,
    pub hint: Arc<dyn Fn(&str) -> String>,
    pub cursor: String,
}

/// Construction options for [`SettingsList`].
#[derive(Default)]
pub struct SettingsListOptions {
    /// When true, a single-line search input filters items by label.
    pub enable_search: bool,
}

type ChangeCallback = Box<dyn FnMut(&str, &str)>;
type CancelCallback = Box<dyn FnMut()>;

/// Submenu result the [`SubmenuDoneCallback`] records. On the next
/// input event the [`SettingsList`] consumes the slot, applies the
/// choice if `Selected`, and closes the submenu either way.
enum SubmenuResult {
    /// Submenu selected a value. Parent item's `current_value` updates
    /// and the on-change callback fires.
    Selected(String),
    /// Submenu closed without a selection. Parent item unchanged.
    Cancelled,
}

/// State held while a submenu is active. Owned by the [`SettingsList`];
/// the `result` slot is shared with the [`SubmenuDoneCallback`] we
/// handed the submenu factory.
struct ActiveSubmenu {
    component: Box<dyn Component>,
    /// Index into `self.items` (not `self.filtered`) of the parent
    /// item that opened the submenu. Storing into `items` directly
    /// keeps the association stable across intervening filter
    /// changes.
    parent_item_index: usize,
    /// Filter-space index of the parent item when the submenu opened,
    /// restored on close so the highlight returns to the same row.
    parent_selected: usize,
    result: Rc<RefCell<Option<SubmenuResult>>>,
}

/// Scrollable list of settings with per-item value cycling.
pub struct SettingsList {
    items: Vec<SettingItem>,
    /// Indices into `items` that currently pass the search filter.
    /// When search is disabled, this mirrors `0..items.len()`.
    filtered: Vec<usize>,
    theme: SettingsListTheme,
    selected: usize,
    max_visible: usize,
    search: Option<Input>,
    on_change: ChangeCallback,
    on_cancel: CancelCallback,
    focused: bool,
    active_submenu: Option<ActiveSubmenu>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: impl FnMut(&str, &str) + 'static,
        on_cancel: impl FnMut() + 'static,
        options: SettingsListOptions,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        // Mirror the item row's 2-column cursor/gutter (`"→ "` / `"  "`)
        // with a `"> "` prompt on the search input so the search text
        // lines up visually with the rows below it.
        let search = options.enable_search.then(|| Input::new("> "));
        Self {
            items,
            filtered,
            theme,
            selected: 0,
            max_visible: max_visible.max(1),
            search,
            on_change: Box::new(on_change),
            on_cancel: Box::new(on_cancel),
            focused: false,
            active_submenu: None,
        }
    }

    /// Update an item's `current_value` by id. No-op if the id is
    /// unknown. Useful when the on-change callback persists the new
    /// value somewhere and a later reload needs to sync the display.
    pub fn update_value(&mut self, id: &str, new_value: impl Into<String>) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value.into();
        }
    }

    /// Index of the currently-selected visible item, or `None` if
    /// there are no visible items.
    pub fn selected_index(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// Id of the currently-selected visible item, or `None` if there
    /// are no visible items.
    pub fn selected_id(&self) -> Option<&str> {
        self.filtered
            .get(self.selected)
            .and_then(|idx| self.items.get(*idx))
            .map(|item| item.id.as_str())
    }

    /// Current value of the item with the given id.
    pub fn value_of(&self, id: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.current_value.as_str())
    }

    /// Activate the selected item: either advance its cycleable values,
    /// or open its submenu if one is configured. No-op otherwise.
    fn activate_selected(&mut self) {
        let Some(&item_idx) = self.filtered.get(self.selected) else {
            return;
        };

        // Submenu takes precedence over values. A submenu-backed item
        // ignores its `values` list (typically empty anyway).
        if self.items[item_idx].submenu.is_some() {
            self.open_submenu(item_idx);
            return;
        }

        let item = &mut self.items[item_idx];
        let Some(values) = item.values.as_ref() else {
            return;
        };
        if values.is_empty() {
            return;
        }
        let current_pos = values.iter().position(|v| v == &item.current_value);
        let next = match current_pos {
            Some(i) => (i + 1) % values.len(),
            None => 0,
        };
        item.current_value = values[next].clone();
        let id = item.id.clone();
        let new_value = item.current_value.clone();
        (self.on_change)(&id, &new_value);
    }

    /// Build the submenu component, hand it the `done` callback, and
    /// stash the resulting state so subsequent render / input calls
    /// delegate to the submenu.
    fn open_submenu(&mut self, item_idx: usize) {
        let result = Rc::new(RefCell::new(None::<SubmenuResult>));
        let result_for_cb = Rc::clone(&result);
        let done: SubmenuDoneCallback = Box::new(move |choice| {
            let mut slot = result_for_cb.borrow_mut();
            // Ignore second-and-later calls; the first one wins.
            if slot.is_none() {
                *slot = Some(match choice {
                    Some(v) => SubmenuResult::Selected(v),
                    None => SubmenuResult::Cancelled,
                });
            }
        });

        let factory = self.items[item_idx]
            .submenu
            .as_ref()
            .expect("open_submenu called on item without submenu");
        let current = self.items[item_idx].current_value.clone();
        let component = factory(&current, done);

        self.active_submenu = Some(ActiveSubmenu {
            component,
            parent_item_index: item_idx,
            parent_selected: self.selected,
            result,
        });
    }

    /// Close the active submenu, applying the result if any. Called
    /// after delegating an input event to the submenu; idempotent if
    /// no submenu is active.
    fn close_active_submenu(&mut self, outcome: SubmenuResult) {
        let Some(active) = self.active_submenu.take() else {
            return;
        };

        if let SubmenuResult::Selected(new_value) = outcome {
            if let Some(item) = self.items.get_mut(active.parent_item_index) {
                item.current_value = new_value.clone();
                let id = item.id.clone();
                (self.on_change)(&id, &new_value);
            }
        }

        // Restore the parent selection so the highlight lands on the
        // item that opened the submenu. Clamp to the current filtered
        // length in case the filter changed (it can't while the
        // submenu is open, but be defensive).
        if !self.filtered.is_empty() {
            self.selected = active
                .parent_selected
                .min(self.filtered.len().saturating_sub(1));
        }
    }

    /// Whether a submenu is currently open. Exposed so callers (tests,
    /// host apps that want to decorate the frame differently) can
    /// branch on submenu state.
    pub fn has_active_submenu(&self) -> bool {
        self.active_submenu.is_some()
    }

    fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected + 1 >= self.filtered.len() {
            0
        } else {
            self.selected + 1
        };
    }

    fn apply_filter(&mut self) {
        let Some(ref search) = self.search else {
            return;
        };
        let query = search.value().to_string();
        if query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            // Collect (index, item) pairs so we can run fuzzy_filter
            // by label then map back to indices.
            let indexed: Vec<(usize, String)> = self
                .items
                .iter()
                .enumerate()
                .map(|(idx, item)| (idx, item.label.clone()))
                .collect();
            let ranked = fuzzy_filter(indexed, &query, |(_, label)| label);
            self.filtered = ranked.into_iter().map(|(idx, _)| idx).collect();
        }
        self.selected = 0;
    }

    fn hint_text(&self) -> &'static str {
        if self.search.is_some() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        }
    }

    fn compute_label_width(&self) -> usize {
        self.items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(30)
    }
}

impl Component for SettingsList {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // While a submenu is open, it owns the entire frame. Keeps the
        // parent list from bleeding render state into the submenu's
        // layout.
        if let Some(active) = &mut self.active_submenu {
            return active.component.render(width);
        }

        let mut lines: Vec<String> = Vec::new();

        if let Some(search) = self.search.as_mut() {
            lines.extend(search.render(width));
            lines.push(String::new());
        }

        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            if self.search.is_some() {
                lines.push(String::new());
                lines.push(truncate_to_width(
                    &(self.theme.hint)(self.hint_text()),
                    width,
                    "",
                    false,
                ));
            }
            return lines;
        }

        if self.filtered.is_empty() {
            lines.push(truncate_to_width(
                &(self.theme.hint)("  No matching settings"),
                width,
                "",
                false,
            ));
            lines.push(String::new());
            lines.push(truncate_to_width(
                &(self.theme.hint)(self.hint_text()),
                width,
                "",
                false,
            ));
            return lines;
        }

        // Scroll window.
        let half = self.max_visible / 2;
        let max_start = self.filtered.len().saturating_sub(self.max_visible);
        let start = self.selected.saturating_sub(half).min(max_start);
        let end = (start + self.max_visible).min(self.filtered.len());

        let label_width = self.compute_label_width();

        for i in start..end {
            let item_idx = self.filtered[i];
            let item = &self.items[item_idx];
            let is_selected = i == self.selected;
            let prefix = if is_selected {
                self.theme.cursor.clone()
            } else {
                "  ".to_string()
            };
            let prefix_width = visible_width(&prefix);
            let label_w = visible_width(&item.label);
            let pad = label_width.saturating_sub(label_w);
            let label_padded = format!("{}{}", item.label, " ".repeat(pad));
            let label_text = (self.theme.label)(&label_padded, is_selected);

            let separator = "  ";
            let used = prefix_width + label_width + visible_width(separator);
            let value_max = width.saturating_sub(used).saturating_sub(2);
            let value_trunc = truncate_to_width(&item.current_value, value_max, "", false);
            let value_text = (self.theme.value)(&value_trunc, is_selected);

            lines.push(truncate_to_width(
                &format!("{}{}{}{}", prefix, label_text, separator, value_text),
                width,
                "",
                false,
            ));
        }

        // Scroll indicator when truncated.
        if start > 0 || end < self.filtered.len() {
            let scroll_text = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        // Description for the selected item.
        if let Some(item) = self
            .filtered
            .get(self.selected)
            .and_then(|idx| self.items.get(*idx))
        {
            if let Some(desc) = &item.description {
                lines.push(String::new());
                let wrapped = wrap_text_with_ansi(desc, width.saturating_sub(4));
                for row in wrapped {
                    lines.push((self.theme.description)(&format!("  {}", row)));
                }
            }
        }

        // Trailing hint.
        lines.push(String::new());
        lines.push(truncate_to_width(
            &(self.theme.hint)(self.hint_text()),
            width,
            "",
            false,
        ));

        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        // When a submenu is active, every input event goes to it. The
        // submenu's own Escape handler (or an explicit selection) is
        // how the user closes it: those paths invoke the `done`
        // callback, which writes to the shared result slot, which we
        // then consume here on the next tick.
        if self.active_submenu.is_some() {
            let handled = {
                let active = self.active_submenu.as_mut().unwrap();
                active.component.handle_input(event)
            };
            // Check whether the submenu signaled completion via the
            // shared slot. Take() drops the borrow before we call
            // close_active_submenu, which borrows self mutably.
            let outcome = self
                .active_submenu
                .as_ref()
                .and_then(|a| a.result.borrow_mut().take());
            if let Some(result) = outcome {
                self.close_active_submenu(result);
            }
            return handled;
        }

        let kb = keybindings::get();

        if kb.matches(event, "tui.select.up") {
            self.move_up();
            return true;
        }
        if kb.matches(event, "tui.select.down") {
            self.move_down();
            return true;
        }
        // Confirm activates; Space is also a hardcoded alias for
        // activation regardless of the registry, matching the
        // original framework's `kb.matches(data, "tui.select.confirm")
        // || data === " "` shape. Space is therefore reserved and
        // never reaches the search input even when search is enabled.
        if kb.matches(event, "tui.select.confirm") || is_plain_space(event) {
            self.activate_selected();
            return true;
        }
        if kb.matches(event, "tui.select.cancel") {
            (self.on_cancel)();
            return true;
        }

        // Anything else falls through to the search input when search
        // is enabled. Plain space was already consumed above, so the
        // search input never sees it.
        //
        // Filter recompute fires unconditionally after forwarding,
        // mirroring pi-tui's `settings-list.ts:194-195` shape:
        //
        // ```ts
        // this.searchInput.handleInput(sanitized);
        // this.applyFilter(this.searchInput.getValue());
        // ```
        //
        // Pi-tui's `Input.handleInput` returns `void` so pi has no
        // gate to elide the recompute. Our [`Input::handle_input`]
        // returns a bool, but auditing it (PORTING.md F37): every
        // branch that mutates `value` returns `true`, and every
        // branch that returns `false` leaves `value` untouched
        // (Ctrl/Alt-modified Key events that didn't match any
        // registry binding and aren't plain printables — e.g. a
        // `Ctrl+Z` under default bindings). So a `if handled` gate
        // would be functionally correct today — but it's
        // structurally fragile: a future Input change that adds a
        // return-false-but-mutate path would silently break filter
        // updates. Dropping the gate eliminates that drift surface
        // and aligns byte-for-byte with pi.
        //
        // Note: only Key and Paste events reach this point. The
        // [`InputEvent::Resize`] variant is intercepted at
        // [`Tui::handle_input_after_listeners`] before any component
        // sees it, and Mouse / Focus events aren't represented in
        // our [`InputEvent`] enum at all (filtered out at the
        // crossterm `TryFrom` boundary).
        //
        // The unconditional call also means cursor moves inside the
        // search input (Ctrl+A / Ctrl+E etc.) reset `selected` to 0
        // via `apply_filter`, matching pi's behavior.
        if let Some(ref mut search) = self.search {
            let handled = search.handle_input(event);
            self.apply_filter();
            return handled;
        }
        false
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(ref mut search) = self.search {
            search.set_focused(focused);
        }
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

/// Predicate for "is this a literal space keypress with no modifiers"
/// (or only Shift, which terminals fold into the character anyway).
///
/// Used by [`SettingsList::handle_input`] so Space can act as a
/// hardcoded confirm alias regardless of how the registry binds
/// `tui.select.confirm`. Mirrors the original framework's `data ===
/// " "` shortcut.
fn is_plain_space(event: &InputEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let InputEvent::Key(key) = event else {
        return false;
    };
    if !matches!(key.code, KeyCode::Char(' ')) {
        return false;
    }
    (key.modifiers - KeyModifiers::SHIFT).is_empty()
}
