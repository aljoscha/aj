//! Read-only authentication-status overlay (`/auth`, shown as
//! "auth status" in the palette).
//!
//! A non-interactive [`SelectList`] (selection indicator suppressed),
//! one row per provider: the provider id as a dim prefix column, the
//! credential method/source summary as the primary label, and the
//! optional detail (e.g. OAuth token expiry) in the right column. The
//! list/close-key mechanics are the shared [`ReadOnlyListOverlay`]; this
//! module only builds the rows.

use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};

use crate::auth::ProviderAuthStatus;
use crate::modes::interactive::components::read_only_list::{
    ReadOnlyCloseHandle, ReadOnlyListOverlay,
};

/// Cheap-to-clone handle the host polls to learn the overlay was closed.
pub type AuthStatusOutcomeHandle = ReadOnlyCloseHandle;

/// Build a read-only status overlay from pre-computed per-provider
/// statuses.
pub fn build_overlay(
    list_theme: SelectListTheme,
    statuses: Vec<ProviderAuthStatus>,
) -> ReadOnlyListOverlay {
    let layout = SelectListLayout {
        show_selection_indicator: false,
        ..Default::default()
    };
    let visible = statuses.len().max(1);
    let scroll_info = std::sync::Arc::clone(&list_theme.scroll_info);
    let list = SelectList::new(build_items(&statuses), visible, list_theme, layout);
    ReadOnlyListOverlay::new(list, scroll_info)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::component::Component;
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

    fn sample() -> ReadOnlyListOverlay {
        build_overlay(
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
        let body = c
            .render(120)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
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
        assert!(h.take().is_some(), "Esc should close");

        let mut c = sample();
        let h = c.outcome_handle();
        c.handle_input(&Key::enter());
        assert!(h.take().is_some(), "Enter should close");
    }
}
