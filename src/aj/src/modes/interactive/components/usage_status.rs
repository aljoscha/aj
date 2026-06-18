//! Read-only usage overlay (`/usage`).
//!
//! One page for every provider's plan-usage report: rate-limit
//! windows as rows (provider id shown as a dim prefix on the
//! provider's first row only, so consecutive rows read as a group),
//! plus reason rows for providers that can't report usage. The
//! reports arrive from a background fetch through a oneshot channel
//! drained in `render`; until then a loading row is shown. Both `Esc`
//! and `Enter` close the overlay, mirroring
//! [`crate::modes::interactive::components::auth_status`].

use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use tokio::sync::oneshot;

use crate::modes::interactive::components::outcome::OutcomeSlot;
use crate::usage::{ProviderUsageStatus, UsageOutcome, format_window_status, now_unix_ms};

/// Outcome of a single usage-overlay session. Read-only, so the only
/// terminal state is `Closed`.
#[derive(Clone, Debug)]
pub enum UsageStatusOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the overlay's outcome slot.
pub type UsageStatusOutcomeHandle = OutcomeSlot<UsageStatusOutcome>;

/// Read-only usage list with an async-fill loading state.
pub struct UsageStatusComponent {
    list: SelectList,
    /// Pending fetch result; `None` once received (or after the
    /// sender vanished, in which case an error row is shown).
    statuses_rx: Option<oneshot::Receiver<Vec<ProviderUsageStatus>>>,
    theme: SelectListTheme,
    outcome: UsageStatusOutcomeHandle,
    focused: bool,
}

impl UsageStatusComponent {
    /// Build the overlay in its loading state. `statuses_rx` delivers
    /// the fetched reports; the host is expected to request a render
    /// when it completes so the page repaints without a keypress.
    pub fn new(
        list_theme: SelectListTheme,
        statuses_rx: oneshot::Receiver<Vec<ProviderUsageStatus>>,
    ) -> Self {
        let loading = vec![SelectItem::new("loading", "Loading usage…")];
        let list = build_list(loading, list_theme.clone());
        Self {
            list,
            statuses_rx: Some(statuses_rx),
            theme: list_theme,
            outcome: UsageStatusOutcomeHandle::new(),
            focused: true,
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> UsageStatusOutcomeHandle {
        self.outcome.clone()
    }

    /// Swap the loading row for the fetched reports once they arrive.
    fn poll_statuses(&mut self) {
        let Some(rx) = self.statuses_rx.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(statuses) => {
                self.list = build_list(build_items(&statuses), self.theme.clone());
                self.statuses_rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            // The fetch task died without sending (it doesn't panic by
            // design, but a dropped sender must not wedge the overlay
            // in its loading state).
            Err(oneshot::error::TryRecvError::Closed) => {
                let items = vec![SelectItem::new("error", "Usage fetch failed.")];
                self.list = build_list(items, self.theme.clone());
                self.statuses_rx = None;
            }
        }
    }
}

/// Non-interactive list construction shared by every fill state.
fn build_list(items: Vec<SelectItem>, theme: SelectListTheme) -> SelectList {
    let layout = SelectListLayout {
        show_selection_indicator: false,
        ..Default::default()
    };
    let visible = items.len().max(1);
    SelectList::new(items, visible, theme, layout)
}

/// Flatten the per-provider reports into list rows. Only a provider's
/// first row carries the provider-id prefix; continuation rows leave
/// it empty so the prefix column groups the windows visually.
fn build_items(statuses: &[ProviderUsageStatus]) -> Vec<SelectItem> {
    let now_ms = now_unix_ms();
    let mut items = Vec::new();
    for status in statuses {
        let id = &status.provider_id;
        let mut prefix = id.as_str();
        let mut push = |items: &mut Vec<SelectItem>, label: &str, description: Option<&str>| {
            let item = SelectItem::new(id, label).with_prefix(prefix);
            items.push(match description {
                Some(desc) => item.with_description(desc),
                None => item,
            });
            prefix = "";
        };
        match &status.outcome {
            UsageOutcome::Usage(usage) => {
                if usage.windows.is_empty() && usage.notes.is_empty() {
                    push(&mut items, "no usage data reported", None);
                }
                for window in &usage.windows {
                    let desc = format_window_status(window.used, window.resets_at, now_ms);
                    push(&mut items, &window.label, Some(&desc));
                }
                for note in &usage.notes {
                    push(&mut items, note, None);
                }
            }
            UsageOutcome::Unsupported { reason } => {
                push(&mut items, &format!("usage not available — {reason}"), None);
            }
            UsageOutcome::NotConfigured => push(&mut items, "not configured", None),
            UsageOutcome::NoSource => push(&mut items, "usage reporting not supported", None),
            UsageOutcome::Error(err) => push(&mut items, &format!("error: {err}"), None),
        }
    }
    items
}

impl Component for UsageStatusComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.poll_statuses();
        self.list.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") || kb.matches(event, "tui.input.submit") {
            self.outcome.set(UsageStatusOutcome::Closed);
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
    use aj_models::usage::{ProviderUsage, UsageWindow};
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

    fn sample_statuses() -> Vec<ProviderUsageStatus> {
        vec![
            ProviderUsageStatus {
                provider_id: "anthropic".into(),
                outcome: UsageOutcome::Usage(ProviderUsage {
                    windows: vec![
                        UsageWindow {
                            label: "Current session".into(),
                            used: 0.12,
                            resets_at: None,
                        },
                        UsageWindow {
                            label: "Current week (all models)".into(),
                            used: 0.34,
                            resets_at: None,
                        },
                    ],
                    notes: vec!["Extra usage credits: $1.23 of $50.00 spent".into()],
                }),
            },
            ProviderUsageStatus {
                provider_id: "openai".into(),
                outcome: UsageOutcome::NoSource,
            },
        ]
    }

    #[test]
    fn shows_loading_until_statuses_arrive() {
        let (tx, rx) = oneshot::channel();
        let mut c = UsageStatusComponent::new(identity_theme(), rx);
        let body = c.render(120).join("\n");
        assert!(body.contains("Loading usage…"), "{body}");

        tx.send(sample_statuses()).unwrap();
        let body = c.render(120).join("\n");
        assert!(!body.contains("Loading"), "{body}");
        assert!(body.contains("anthropic"), "{body}");
        assert!(body.contains("Current session"), "{body}");
        assert!(body.contains("12% used"), "{body}");
        assert!(body.contains("Extra usage credits"), "{body}");
        assert!(body.contains("usage reporting not supported"), "{body}");
    }

    #[test]
    fn provider_prefix_only_on_first_row() {
        let items = build_items(&sample_statuses());
        let prefixes: Vec<&str> = items
            .iter()
            .map(|i| i.prefix.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(prefixes, vec!["anthropic", "", "", "openai"]);
    }

    #[test]
    fn dropped_sender_shows_error_instead_of_wedging() {
        let (tx, rx) = oneshot::channel::<Vec<ProviderUsageStatus>>();
        drop(tx);
        let mut c = UsageStatusComponent::new(identity_theme(), rx);
        let body = c.render(120).join("\n");
        assert!(body.contains("Usage fetch failed."), "{body}");
    }

    #[test]
    fn esc_and_enter_close() {
        let (_tx, rx) = oneshot::channel();
        let mut c = UsageStatusComponent::new(identity_theme(), rx);
        let h = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(matches!(h.take(), Some(UsageStatusOutcome::Closed)));

        let (_tx, rx) = oneshot::channel();
        let mut c = UsageStatusComponent::new(identity_theme(), rx);
        let h = c.outcome_handle();
        c.handle_input(&Key::enter());
        assert!(matches!(h.take(), Some(UsageStatusOutcome::Closed)));
    }
}
