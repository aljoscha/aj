//! Thinking-effort selector overlay (`/thinking`).
//!
//! Wraps an [`aj_tui::components::select_list::SelectList`] with a
//! shared-state outcome handle so the host can poll the result
//! after each input event. The overlay renders a one-line title
//! row above the list ("Thinking effort") and the list itself,
//! with the current level pre-selected so `Enter` re-applies the
//! same setting (a no-op confirm).
//!
//! See `docs/aj-next-plan.md` "Selectors and theming" step.

use std::sync::{Arc, Mutex};

use aj_models::ThinkingConfig;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};

use crate::config::commands::{THINKING_LEVELS, parse_thinking_level, thinking_level_name};

/// Outcome of a single overlay session.
///
/// `Confirmed(level)` carries the chosen [`ThinkingConfig`] (or
/// `None` for `"off"`); `Cancelled` is the user pressing `Esc`.
/// The host treats both as "close the overlay"; only the former
/// mutates agent state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingSelectorOutcome {
    Confirmed(Option<ThinkingConfig>),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<ThinkingSelectorOutcome>>>;

/// The overlay's top-level component. Owns the underlying
/// [`SelectList`] (`inner`) and a clone of the outcome slot
/// (`outcome`). The host keeps another clone of `outcome` and
/// polls it after each input event to decide whether to close
/// the overlay.
pub struct ThinkingSelectorComponent {
    inner: SelectList,
    outcome: OutcomeHandle,
}

impl ThinkingSelectorComponent {
    /// Build a fresh selector. `current` highlights the active
    /// level on open so `Enter` is a no-op confirm; `theme` styles
    /// the underlying [`SelectList`].
    pub fn new(theme: SelectListTheme, current: Option<ThinkingConfig>) -> Self {
        let active_name = thinking_level_name(&current);
        let items: Vec<SelectItem> = THINKING_LEVELS
            .iter()
            .map(|l| {
                let label = if l.name == active_name {
                    format!("{} (current)", l.name)
                } else {
                    l.name.to_string()
                };
                SelectItem::new(l.name, &label).with_description(l.description)
            })
            .collect();

        let initial_index = THINKING_LEVELS
            .iter()
            .position(|l| l.name == active_name)
            .unwrap_or(0);

        let mut inner = SelectList::new(
            items,
            THINKING_LEVELS.len(),
            theme,
            SelectListLayout::default(),
        );
        inner.set_selected_index(initial_index);

        let outcome: OutcomeHandle = Arc::new(Mutex::new(None));

        // Wire the SelectList's on_select / on_cancel callbacks to
        // the shared outcome slot. The closures clone an `Arc` of
        // the slot rather than borrowing — the SelectList stores
        // them as `Box<dyn FnMut>`, which can't capture `&self`.
        let confirm_outcome = Arc::clone(&outcome);
        inner.on_select = Some(Box::new(move |item| {
            let level = parse_thinking_level(&item.value).unwrap_or(None);
            *confirm_outcome.lock().expect("outcome mutex poisoned") =
                Some(ThinkingSelectorOutcome::Confirmed(level));
        }));
        let cancel_outcome = Arc::clone(&outcome);
        inner.on_cancel = Some(Box::new(move || {
            *cancel_outcome.lock().expect("outcome mutex poisoned") =
                Some(ThinkingSelectorOutcome::Cancelled);
        }));

        Self { inner, outcome }
    }

    /// Hand the host a clone of the outcome slot. After each
    /// input event the host calls `lock().take()` on this handle;
    /// on `Some(_)` it hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }
}

impl aj_tui::component::Component for ThinkingSelectorComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::with_capacity(THINKING_LEVELS.len() + 1);
        lines.extend(self.inner.render(width));
        lines
    }

    fn handle_input(&mut self, event: &aj_tui::keys::InputEvent) -> bool {
        self.inner.handle_input(event)
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.inner.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::component::Component;
    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::{InputEvent, Key};

    use super::*;

    /// Identity theme for tests — passes every closure through
    /// verbatim so renders show the structural text rather than
    /// ANSI escape sequences.
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

    fn enter_event() -> InputEvent {
        Key::enter()
    }
    fn escape_event() -> InputEvent {
        Key::escape()
    }
    fn down_event() -> InputEvent {
        Key::down()
    }

    #[test]
    fn highlights_current_level_on_open() {
        let mut sel =
            ThinkingSelectorComponent::new(identity_theme(), Some(ThinkingConfig::Medium));
        let lines = sel.render(60);
        // The first list row should be the medium row, marked
        // "(current)" so the user sees what's currently active.
        let body = lines.join("\n");
        assert!(body.contains("medium (current)"), "got: {body}");
    }

    #[test]
    fn enter_emits_confirmed_outcome_with_selected_level() {
        let mut sel =
            ThinkingSelectorComponent::new(identity_theme(), Some(ThinkingConfig::Medium));
        let outcome = sel.outcome_handle();
        // Simulate confirm on the highlighted (medium) row.
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take();
        assert_eq!(
            result,
            Some(ThinkingSelectorOutcome::Confirmed(Some(
                ThinkingConfig::Medium
            )))
        );
    }

    #[test]
    fn esc_emits_cancelled_outcome() {
        let mut sel = ThinkingSelectorComponent::new(identity_theme(), Some(ThinkingConfig::Low));
        let outcome = sel.outcome_handle();
        sel.handle_input(&escape_event());
        let result = outcome.lock().unwrap().take();
        assert_eq!(result, Some(ThinkingSelectorOutcome::Cancelled));
    }

    #[test]
    fn down_arrow_moves_to_next_level_then_enter_confirms_it() {
        let mut sel =
            ThinkingSelectorComponent::new(identity_theme(), Some(ThinkingConfig::Medium));
        let outcome = sel.outcome_handle();
        // medium is index 2; pressing down once lands on high.
        sel.handle_input(&down_event());
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take();
        assert_eq!(
            result,
            Some(ThinkingSelectorOutcome::Confirmed(Some(
                ThinkingConfig::High
            )))
        );
    }

    #[test]
    fn off_is_selectable_and_round_trips() {
        // Build with current=off so off starts highlighted.
        let mut sel = ThinkingSelectorComponent::new(identity_theme(), None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take();
        assert_eq!(result, Some(ThinkingSelectorOutcome::Confirmed(None)));
    }
}
