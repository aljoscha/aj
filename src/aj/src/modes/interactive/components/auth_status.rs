//! Read-only authentication-status overlay (`/auth`, shown as
//! "auth status" in the palette).
//!
//! A non-interactive [`SelectList`] (selection indicator suppressed),
//! one row per provider: the provider id as a dim prefix column, the
//! credential method/source summary as the primary label, and the
//! optional detail (e.g. OAuth token expiry) in the right column.
//! Both `Esc` and `Enter` close it, mirroring
//! [`crate::modes::interactive::components::help_overlay`].

use std::sync::{Arc, Mutex};

use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::auth::ProviderAuthStatus;

/// Outcome of a single status-overlay session. Read-only, so the only
/// terminal state is `Closed`.
#[derive(Clone, Debug)]
pub enum AuthStatusOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the overlay's outcome slot.
#[derive(Clone)]
pub struct AuthStatusOutcomeHandle(Arc<Mutex<Option<AuthStatusOutcome>>>);

impl AuthStatusOutcomeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<AuthStatusOutcome> {
        self.0
            .lock()
            .expect("auth status outcome mutex poisoned")
            .take()
    }

    fn set(&self, value: AuthStatusOutcome) {
        *self.0.lock().expect("auth status outcome mutex poisoned") = Some(value);
    }
}

/// Read-only status list.
pub struct AuthStatusComponent {
    list: SelectList,
    outcome: AuthStatusOutcomeHandle,
    focused: bool,
}

impl AuthStatusComponent {
    /// Build the overlay from pre-computed per-provider statuses.
    pub fn new(list_theme: SelectListTheme, statuses: Vec<ProviderAuthStatus>) -> Self {
        let layout = SelectListLayout {
            show_selection_indicator: false,
            ..Default::default()
        };
        let visible = statuses.len().max(1);
        let list = SelectList::new(build_items(&statuses), visible, list_theme, layout);
        Self {
            list,
            outcome: AuthStatusOutcomeHandle::new(),
            focused: true,
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> AuthStatusOutcomeHandle {
        self.outcome.clone()
    }
}

fn build_items(statuses: &[ProviderAuthStatus]) -> Vec<SelectItem> {
    statuses
        .iter()
        .map(|s| {
            let item = SelectItem::new(&s.provider_id, &s.summary).with_prefix(&s.provider_id);
            match &s.detail {
                Some(detail) => item.with_description(detail),
                None => item,
            }
        })
        .collect()
}

impl Component for AuthStatusComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(AuthStatusOutcome::Closed);
            return true;
        }
        // Swallow every other key — the list is read-only.
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

    fn sample() -> AuthStatusComponent {
        AuthStatusComponent::new(
            identity_theme(),
            vec![
                ProviderAuthStatus {
                    provider_id: "anthropic".into(),
                    configured: true,
                    summary: "subscription — Anthropic (Claude Pro/Max)".into(),
                    detail: Some("expires in 1h 47m".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(),
                    configured: false,
                    summary: "not configured".into(),
                    detail: None,
                },
            ],
        )
    }

    #[test]
    fn renders_every_provider() {
        let mut c = sample();
        let body = c.render(120).join("\n");
        assert!(body.contains("anthropic"), "{body}");
        assert!(body.contains("subscription"), "{body}");
        assert!(body.contains("expires in 1h 47m"), "{body}");
        assert!(body.contains("not configured"), "{body}");
    }

    #[test]
    fn esc_and_enter_close() {
        let mut c = sample();
        let h = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(matches!(h.take(), Some(AuthStatusOutcome::Closed)));

        let mut c = sample();
        let h = c.outcome_handle();
        c.handle_input(&Key::enter());
        assert!(matches!(h.take(), Some(AuthStatusOutcome::Closed)));
    }
}
