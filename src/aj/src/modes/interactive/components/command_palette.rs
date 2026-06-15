//! Command palette overlay.
//!
//! A grouped, fuzzy-searchable list of every entry in [`COMMANDS`].
//! The user types to filter, navigates with arrows, presses `Enter`
//! to confirm or `Esc` to cancel. The palette's outcome is the chosen
//! command's [`CommandAction`], which the host applies — the palette
//! is a thin discoverability layer over the same actions the keyboard
//! shortcuts trigger.
//!
//! Visual layout per row: `<category>  <name>  …  <shortcut-or-hint>`,
//! supplied to [`SelectList`] via the `prefix` / primary label /
//! `shortcut` / `description` columns.

use std::sync::{Arc, Mutex};

use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::config::commands::{COMMANDS, CommandAction};

/// Outcome of a single palette session.
///
/// `Confirmed.action` is the chosen command's [`CommandAction`] for
/// the host to apply. `Cancelled` is `Esc`.
#[derive(Clone, Debug)]
pub enum CommandPaletteOutcome {
    Confirmed { action: CommandAction },
    Cancelled,
}

/// Cheap-to-clone handle pointing at the palette's outcome slot.
#[derive(Clone)]
pub struct CommandPaletteOutcomeHandle(Arc<Mutex<Option<CommandPaletteOutcome>>>);

impl CommandPaletteOutcomeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<CommandPaletteOutcome> {
        self.0
            .lock()
            .expect("palette outcome mutex poisoned")
            .take()
    }

    fn set(&self, value: CommandPaletteOutcome) {
        *self.0.lock().expect("palette outcome mutex poisoned") = Some(value);
    }
}

/// Palette component: search input over a fuzzy-filtered
/// [`SelectList`] of builtin commands.
///
/// The list is built **once** from [`COMMANDS`]; each
/// keystroke calls [`SelectList::set_filter`] rather than rebuilding
/// the list, so the prefix/label columns stay anchored and no
/// per-keystroke allocation churn occurs.
pub struct CommandPaletteComponent {
    search: TextInput,
    list: SelectList,
    outcome: CommandPaletteOutcomeHandle,
}

impl CommandPaletteComponent {
    /// Build a palette seeded from [`COMMANDS`].
    ///
    /// `max_visible_rows` is the initial [`SelectList`] window cap. Once
    /// mounted in an `OverlayWindow` the surrounding frame drives this
    /// via [`Component::set_available_height`] each frame, so the list
    /// grows to fill the overlay's inner-row budget; the constructor
    /// value only governs the first render (and direct-render tests).
    pub fn new(list_theme: SelectListTheme, max_visible_rows: usize) -> Self {
        let mut search = TextInput::new("search: ");
        search.set_focused(true);

        let mut list = SelectList::new(
            build_items(),
            max_visible_rows,
            list_theme,
            SelectListLayout::default(),
        );
        list.set_focused(true);

        Self {
            search,
            list,
            outcome: CommandPaletteOutcomeHandle::new(),
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> CommandPaletteOutcomeHandle {
        CommandPaletteOutcomeHandle(Arc::clone(&self.outcome.0))
    }

    fn commit_selection(&self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        // The list item's `value` is the command `name`; map it back
        // to the catalog entry to recover the action to dispatch.
        let Some(cmd) = COMMANDS.iter().find(|c| c.name == item.value) else {
            return;
        };
        self.outcome
            .set(CommandPaletteOutcome::Confirmed { action: cmd.action });
    }

    fn commit_cancel(&self) {
        self.outcome.set(CommandPaletteOutcome::Cancelled);
    }
}

/// Build one [`SelectItem`] per command.
///
/// - `value` is the command `name`; `commit_selection` maps it back
///   to the catalog entry to recover the [`CommandAction`].
/// - `label` is the friendly `title` shown in the primary column.
/// - `prefix` is the dim `category` column.
/// - `filter_key` is `"{category} {title}"` so typing a category
///   surfaces its whole group and typing a title narrows to the row.
/// - `shortcut` (when the command has a bound action) populates the
///   accent right column, resolved at render time from the
///   keybindings manager so user rebindings flow through.
fn build_items() -> Vec<SelectItem> {
    COMMANDS
        .iter()
        .map(|cmd| {
            let mut item = SelectItem::new(cmd.name, cmd.title)
                .with_prefix(cmd.category)
                .with_filter_key(&format!("{} {}", cmd.category, cmd.title));
            if let Some(short) = cmd
                .action_id
                .and_then(aj_tui::keybindings::format_action_shortcut)
            {
                item = item.with_shortcut(&short);
            }
            item
        })
        .collect()
}

impl Component for CommandPaletteComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Chrome (title + border) is supplied by the surrounding
        // `OverlayWindow` mount; render only the search input and
        // the filtered list here.
        let mut lines = Vec::new();
        lines.extend(self.search.render(width));
        lines.push(String::new());
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();

        if kb.matches(event, "tui.select.cancel") {
            self.commit_cancel();
            return true;
        }

        if kb.matches(event, "tui.input.submit") {
            self.commit_selection();
            return true;
        }

        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            return self.list.handle_input(event);
        }

        drop(kb);

        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            self.list.set_filter(self.search.value());
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        // Grow the list to fill the overlay's inner-row budget. Chrome
        // above the list (mirrored in `render`): search input + blank
        // separator + the list's own scroll-info line.
        self.list.set_max_visible(rows.saturating_sub(3).max(1));
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::Key;

    use super::*;

    /// Identity theme — pass-through closures so renders show
    /// structural text rather than ANSI escapes.
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
    fn renders_all_builtin_commands_when_query_empty() {
        // Sized to the catalog: this test needs every row visible,
        // unlike the host, which gives the list a fixed height and
        // lets it scroll.
        let mut p = CommandPaletteComponent::new(identity_theme(), COMMANDS.len());
        let body = p.render(80).join("\n");
        for cmd in COMMANDS {
            assert!(
                body.contains(cmd.title),
                "missing title {}: {body}",
                cmd.title
            );
            assert!(
                body.contains(cmd.category),
                "missing category {}: {body}",
                cmd.category
            );
        }
    }

    #[test]
    fn set_available_height_grows_the_visible_list() {
        // Seed with a small initial cap, then report a tall overlay: the
        // list must fill it (minus the search box, blank separator, and
        // scroll-info chrome) rather than stay pinned at the seed value.
        // Guard against a catalog too small to exercise the growth.
        assert!(
            COMMANDS.len() > 8,
            "test needs a catalog larger than the tall budget"
        );
        let mut p = CommandPaletteComponent::new(identity_theme(), 3);
        let seeded_rows = p.render(80).len();
        p.set_available_height(20);
        let tall_rows = p.render(80).len();
        assert!(
            tall_rows > seeded_rows,
            "expected the list to grow with available height: {seeded_rows} -> {tall_rows}"
        );
    }

    #[test]
    fn palette_shows_resolved_open_shortcut() {
        // The shortcut is now resolved at render time from the
        // process-wide keybindings manager, so installing the
        // `aj.*` defaults is required for the action to be known.
        crate::config::keybindings::install_global_manager_defaults();
        let mut p = CommandPaletteComponent::new(identity_theme(), 14);
        let body = p.render(80).join("\n");
        assert!(body.contains("Ctrl+O"), "expected Ctrl+O in: {body}");
    }

    #[test]
    fn fuzzy_filter_narrows_list() {
        let mut p = CommandPaletteComponent::new(identity_theme(), 14);
        for c in "mod".chars() {
            p.handle_input(&Key::char(c));
        }
        let body = p.render(80).join("\n");
        // The `model` category rows survive.
        assert!(body.contains("model"), "got: {body}");
        // Rows in other categories don't fuzzy-match "mod".
        assert!(!body.contains("session"), "got: {body}");
        assert!(!body.contains("quit"), "got: {body}");
    }

    #[test]
    fn fuzzy_filter_matches_category() {
        let mut p = CommandPaletteComponent::new(identity_theme(), 14);
        for c in "mod".chars() {
            p.handle_input(&Key::char(c));
        }
        let body = p.render(80).join("\n");
        // Both `model` rows (`thinking` and `use`) should be on
        // screen — querying the category surfaces every entry in that
        // group via the `"<category> <title>"` filter key.
        assert!(body.contains("thinking"), "got: {body}");
        assert!(body.contains("use"), "got: {body}");
    }

    #[test]
    fn confirm_writes_outcome() {
        let mut p = CommandPaletteComponent::new(identity_theme(), 14);
        let handle = p.outcome_handle();
        let expected_action = COMMANDS[0].action;
        p.handle_input(&Key::enter());
        match handle.take().expect("outcome set") {
            CommandPaletteOutcome::Confirmed { action } => {
                assert_eq!(action, expected_action);
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn cancel_writes_outcome() {
        let mut p = CommandPaletteComponent::new(identity_theme(), 14);
        let handle = p.outcome_handle();
        p.handle_input(&Key::escape());
        assert!(matches!(
            handle.take().expect("outcome set"),
            CommandPaletteOutcome::Cancelled
        ));
    }

    /// Regression: filtering the long-lived list down to a single
    /// match must not shift the primary-label column. The prefix
    /// column is sized over the full catalog (the widest category),
    /// so the label position is invariant under filtering.
    #[test]
    fn label_column_stable_across_filter() {
        // Pick a query that matches exactly one command. `quit` is a
        // single-row hit in the current catalog. Sized to the catalog
        // so the quit row is visible before filtering.
        let mut p_unfiltered = CommandPaletteComponent::new(identity_theme(), COMMANDS.len());
        let unfiltered = p_unfiltered.render(80);
        let unfiltered_row =
            list_row_containing(&unfiltered, "quit").expect("unfiltered list contains quit row");

        let mut p_filtered = CommandPaletteComponent::new(identity_theme(), 14);
        for c in "quit".chars() {
            p_filtered.handle_input(&Key::char(c));
        }
        let filtered = p_filtered.render(80);
        let filtered_row =
            list_row_containing(&filtered, "quit").expect("filtered list contains quit row");

        // Compare the label position *after* the selection gutter so
        // the test isolates the prefix-column width from the unrelated
        // selected-vs-unselected arrow difference.
        let unfiltered_offset = label_offset_after_gutter(&unfiltered_row, "quit");
        let filtered_offset = label_offset_after_gutter(&filtered_row, "quit");
        assert_eq!(
            unfiltered_offset, filtered_offset,
            "quit label column shifted between unfiltered ({unfiltered_row:?}) \
             and filtered ({filtered_row:?})",
        );
    }

    /// Find a list-area row containing `needle`. The palette renders
    /// `[search-input, blank, ...list rows]`; this helper skips the
    /// search input (which echoes the user's query and would otherwise
    /// match the needle when the query itself is the command name).
    fn list_row_containing(lines: &[String], needle: &str) -> Option<String> {
        lines
            .iter()
            .skip(1)
            .find(|line| line.contains(needle))
            .cloned()
    }

    /// Strip the 2-cell `SelectList` selection-arrow gutter (`"→ "`
    /// when selected, `"  "` otherwise) and return the visible-column
    /// position of `needle` within the remainder. Counts characters
    /// (not bytes) so the multi-byte arrow doesn't skew the result.
    fn label_offset_after_gutter(row: &str, needle: &str) -> usize {
        let rest = row
            .strip_prefix("→ ")
            .or_else(|| row.strip_prefix("  "))
            .unwrap_or(row);
        rest.char_indices()
            .position(|(byte_idx, _)| rest[byte_idx..].starts_with(needle))
            .unwrap_or_else(|| panic!("needle {needle:?} not in row {row:?}"))
    }
}
