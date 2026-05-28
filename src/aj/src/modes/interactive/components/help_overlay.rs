//! Read-only help overlay listing every builtin command.
//!
//! A non-interactive [`SelectList`] (selection indicator suppressed)
//! with one row per command: the dim `category` prefix column, the
//! friendly `title` as the primary label, and the description in the
//! right column. A command's keyboard shortcut, when bound, is folded
//! into the description so the single-row layout carries everything.
//!
//! The surrounding [`OverlayWindow`] provides the title bar and border
//! chrome.
//!
//! [`OverlayWindow`]: aj_tui::components::overlay_window::OverlayWindow
//! [`SelectList`]: aj_tui::components::select_list::SelectList

use std::sync::{Arc, Mutex};

use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::config::slash_commands::BUILTIN_COMMANDS;

/// Outcome of a single help-overlay session.
///
/// The view is read-only, so there is exactly one terminal state:
/// `Closed`. Both Esc and Enter map to it.
#[derive(Clone, Debug)]
pub enum HelpOverlayOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the overlay's outcome slot.
#[derive(Clone)]
pub struct HelpOverlayOutcomeHandle(Arc<Mutex<Option<HelpOverlayOutcome>>>);

impl HelpOverlayOutcomeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<HelpOverlayOutcome> {
        self.0
            .lock()
            .expect("help overlay outcome mutex poisoned")
            .take()
    }

    fn set(&self, value: HelpOverlayOutcome) {
        *self.0.lock().expect("help overlay outcome mutex poisoned") = Some(value);
    }
}

/// Help overlay component: a read-only [`SelectList`] of
/// [`BUILTIN_COMMANDS`].
pub struct HelpOverlayComponent {
    list: SelectList,
    outcome: HelpOverlayOutcomeHandle,
    focused: bool,
}

impl HelpOverlayComponent {
    /// Build the help view seeded from [`BUILTIN_COMMANDS`].
    pub fn new(list_theme: SelectListTheme) -> Self {
        let layout = SelectListLayout {
            // Read-only: no selection to highlight.
            show_selection_indicator: false,
            ..Default::default()
        };
        let list = SelectList::new(
            build_items(),
            BUILTIN_COMMANDS.len().max(1),
            list_theme,
            layout,
        );
        Self {
            list,
            outcome: HelpOverlayOutcomeHandle::new(),
            focused: true,
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> HelpOverlayOutcomeHandle {
        HelpOverlayOutcomeHandle(Arc::clone(&self.outcome.0))
    }
}

/// Build one [`SelectItem`] per command: `category` prefix, `title`
/// label, and `description` (with the bound shortcut folded in) in
/// the right column.
fn build_items() -> Vec<SelectItem> {
    BUILTIN_COMMANDS
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

impl Component for HelpOverlayComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Chrome (title + border) is supplied by the surrounding
        // `OverlayWindow` mount.
        self.list.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(HelpOverlayOutcome::Closed);
            return true;
        }
        // Swallow every other key so printable characters don't leak
        // through to background components. The list is read-only, so
        // there is nothing to navigate.
        true
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
    use std::sync::Arc;

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
        let mut h = HelpOverlayComponent::new(identity_theme());
        // Render wide so descriptions aren't column-truncated.
        let body = h.render(200).join("\n");
        for cmd in BUILTIN_COMMANDS {
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
        let mut h = HelpOverlayComponent::new(identity_theme());
        let body = h.render(200).join("\n");
        assert!(
            !body.contains('→'),
            "read-only help should not show an arrow: {body}"
        );
    }

    #[test]
    fn esc_writes_closed_outcome() {
        let mut h = HelpOverlayComponent::new(identity_theme());
        let handle = h.outcome_handle();
        h.handle_input(&Key::escape());
        assert!(matches!(
            handle.take().expect("outcome set"),
            HelpOverlayOutcome::Closed
        ));
    }

    #[test]
    fn enter_also_closes() {
        let mut h = HelpOverlayComponent::new(identity_theme());
        let handle = h.outcome_handle();
        h.handle_input(&Key::enter());
        assert!(matches!(
            handle.take().expect("outcome set"),
            HelpOverlayOutcome::Closed
        ));
    }
}
