//! Provider picker overlay shared by `/login` and `/logout`.
//!
//! A thin wrapper over [`SelectList`] following the same
//! shared-outcome-slot pattern as the other selectors (see
//! [`crate::modes::interactive::components::thinking_selector`]). The
//! rows are pre-computed by the host (provider id, display name, and a
//! status summary) so the component stays free of any
//! [`aj_models::auth`] dependency; it just reports which provider id
//! the user confirmed.
//!
//! The login-vs-logout distinction is the host's concern: it picks
//! which providers to list and what to do on confirm. This component
//! only surfaces the choice.

use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};

use crate::modes::interactive::components::outcome::OutcomeSlot;

/// Outcome of a single picker session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPickerOutcome {
    /// The user confirmed a provider; carries its id.
    Confirmed(String),
    /// The user pressed `Esc`.
    Cancelled,
}

/// Cheap-to-clone handle pointing at the picker's outcome slot.
pub type OutcomeHandle = OutcomeSlot<AuthPickerOutcome>;

/// One selectable provider row supplied by the host.
pub struct AuthProviderItem {
    /// Provider id returned on confirm (e.g. `"anthropic"`).
    pub provider_id: String,
    /// Friendly name shown as the primary column.
    pub label: String,
    /// Current auth status, shown in the right column.
    pub description: String,
}

/// Provider picker component.
pub struct AuthPickerComponent {
    inner: SelectList,
    outcome: OutcomeHandle,
}

impl AuthPickerComponent {
    /// Build a picker from pre-computed provider rows.
    pub fn new(theme: SelectListTheme, items: Vec<AuthProviderItem>) -> Self {
        let select_items: Vec<SelectItem> = items
            .iter()
            .map(|it| {
                SelectItem::new(&it.provider_id, &it.label)
                    .with_description(&it.description)
                    .with_filter_key(&format!("{} {}", it.provider_id, it.label))
            })
            .collect();
        let visible = select_items.len().max(1);
        let mut inner = SelectList::new(select_items, visible, theme, SelectListLayout::default());

        let outcome = OutcomeHandle::new();
        let confirm = outcome.clone();
        inner.on_select = Some(Box::new(move |item| {
            confirm.set(AuthPickerOutcome::Confirmed(item.value.clone()));
        }));
        let cancel = outcome.clone();
        inner.on_cancel = Some(Box::new(move || {
            cancel.set(AuthPickerOutcome::Cancelled);
        }));

        Self { inner, outcome }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        self.outcome.clone()
    }
}

impl aj_tui::component::Component for AuthPickerComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.inner.render(width)
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
    use aj_tui::component::Component;
    use aj_tui::keys::Key;

    use std::sync::Arc;

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

    fn sample() -> AuthPickerComponent {
        AuthPickerComponent::new(
            identity_theme(),
            vec![
                AuthProviderItem {
                    provider_id: "anthropic".into(),
                    label: "Anthropic (Claude Pro/Max)".into(),
                    description: "subscription".into(),
                },
                AuthProviderItem {
                    provider_id: "openai-codex".into(),
                    label: "ChatGPT Plus/Pro".into(),
                    description: "not configured".into(),
                },
            ],
        )
    }

    #[test]
    fn enter_confirms_highlighted_provider() {
        let mut p = sample();
        let outcome = p.outcome_handle();
        p.handle_input(&Key::enter());
        assert_eq!(
            outcome.take(),
            Some(AuthPickerOutcome::Confirmed("anthropic".into()))
        );
    }

    #[test]
    fn down_then_enter_confirms_second_provider() {
        let mut p = sample();
        let outcome = p.outcome_handle();
        p.handle_input(&Key::down());
        p.handle_input(&Key::enter());
        assert_eq!(
            outcome.take(),
            Some(AuthPickerOutcome::Confirmed("openai-codex".into()))
        );
    }

    #[test]
    fn esc_cancels() {
        let mut p = sample();
        let outcome = p.outcome_handle();
        p.handle_input(&Key::escape());
        assert_eq!(outcome.take(), Some(AuthPickerOutcome::Cancelled));
    }
}
