//! Read-only help overlay listing every builtin command.
//!
//! A non-interactive [`SelectList`] (selection indicator suppressed)
//! with one row per command: the dim `category` prefix column, the
//! friendly `title` as the primary label, and the description in the
//! right column. A command's keyboard shortcut, when bound, is folded
//! into the description so the single-row layout carries everything.
//!
//! The list/close-key mechanics are the shared [`ReadOnlyListOverlay`];
//! this module only builds the rows. The surrounding [`OverlayWindow`]
//! provides the title bar and border chrome.
//!
//! [`OverlayWindow`]: aj_tui::components::overlay_window::OverlayWindow
//! [`SelectList`]: aj_tui::components::select_list::SelectList

use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};

use crate::config::commands::COMMANDS;
use crate::modes::interactive::components::read_only_list::{
    ReadOnlyCloseHandle, ReadOnlyListOverlay,
};

/// Cheap-to-clone handle the host polls to learn the overlay was closed.
pub type HelpOverlayOutcomeHandle = ReadOnlyCloseHandle;

/// Build a read-only help overlay seeded from [`COMMANDS`].
pub fn build_overlay(list_theme: SelectListTheme) -> ReadOnlyListOverlay {
    let layout = SelectListLayout {
        // Read-only: no selection to highlight.
        show_selection_indicator: false,
        ..Default::default()
    };
    let list = SelectList::new(build_items(), COMMANDS.len().max(1), list_theme, layout);
    ReadOnlyListOverlay::new(list)
}

/// Build one [`SelectItem`] per command: `category` prefix, `title`
/// label, and `description` (with the bound shortcut folded in) in
/// the right column.
fn build_items() -> Vec<SelectItem> {
    COMMANDS
        .iter()
        .map(|cmd| {
            let description = match cmd
                .action_id
                .and_then(aj_tui::keybindings::format_action_shortcut)
            {
                Some(short) => format!("{}  ({short})", cmd.description),
                None => cmd.description.to_string(),
            };
            SelectItem::new(cmd.name, cmd.title)
                .with_prefix(cmd.category)
                .with_description(&description)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::component::Component;
    use aj_tui::components::select_list::SelectListTheme;
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

    #[test]
    fn renders_all_builtin_commands() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut h = build_overlay(identity_theme());
        // Render wide so descriptions aren't column-truncated.
        let body = h.render(200).join("\n");
        for cmd in COMMANDS {
            assert!(
                body.contains(cmd.title),
                "missing title {}: {body}",
                cmd.title
            );
            assert!(
                body.contains(cmd.description),
                "missing description for {}: {body}",
                cmd.name,
            );
            if let Some(short) = cmd
                .action_id
                .and_then(aj_tui::keybindings::format_action_shortcut)
            {
                assert!(
                    body.contains(&short),
                    "missing shortcut for {}: {body}",
                    cmd.name,
                );
            }
        }
    }

    #[test]
    fn no_selection_indicator_in_read_only_view() {
        let mut h = build_overlay(identity_theme());
        let body = h.render(200).join("\n");
        assert!(
            !body.contains('→'),
            "read-only help should not show an arrow: {body}"
        );
    }

    #[test]
    fn esc_writes_closed_outcome() {
        let mut h = build_overlay(identity_theme());
        let handle = h.outcome_handle();
        h.handle_input(&Key::escape());
        assert!(handle.take().is_some(), "Esc should close the overlay");
    }

    #[test]
    fn enter_also_closes() {
        let mut h = build_overlay(identity_theme());
        let handle = h.outcome_handle();
        h.handle_input(&Key::enter());
        assert!(handle.take().is_some(), "Enter should close the overlay");
    }
}
