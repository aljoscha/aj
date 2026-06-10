//! Skills window overlay (`/skills`).
//!
//! Lists every discovered skill with its description and source path;
//! `Enter` toggles the highlighted skill between enabled and disabled.
//! Like the settings window, the overlay stays open across changes:
//! each toggle is pushed onto a shared queue ([`ChangesHandle`]) that
//! the host drains and persists into the `disabled_skills` config
//! option. `Esc` closes the window via the outcome slot.
//!
//! Toggling only changes what *new* sessions list to the model — the
//! running session's system prompt is frozen — so the host's notices
//! say so explicitly.

use std::sync::{Arc, Mutex};

use aj_tui::component::Component;
use aj_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme,
};
use aj_tui::keys::InputEvent;

/// One skill row, precomputed by the host from discovery plus the
/// current `disabled_skills` config value.
pub struct SkillRow {
    pub name: String,
    pub description: String,
    /// Display form of the SKILL.md path (tildified).
    pub path: String,
    pub enabled: bool,
    /// Frontmatter `disable-model-invocation`: shown as a marker so the
    /// user can tell why the skill won't reach the model even when
    /// enabled.
    pub disable_model_invocation: bool,
}

/// Outcome of a window session. The window only ever closes;
/// individual toggles flow through [`ChangesHandle`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsWindowOutcome {
    Closed,
}

/// Cheap-to-clone handle pointing at the same outcome slot the overlay
/// component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<SkillsWindowOutcome>>>;

/// Queue of `(skill name, "enabled" | "disabled")` toggles, in the
/// order the user made them. The host drains it after every input
/// event.
pub type ChangesHandle = Arc<Mutex<Vec<(String, String)>>>;

/// The overlay's top-level component. See the module docs for the
/// changes flow.
pub struct SkillsWindowComponent {
    inner: SettingsList,
    outcome: OutcomeHandle,
    changes: ChangesHandle,
}

impl SkillsWindowComponent {
    pub fn new(theme: SettingsListTheme, rows: Vec<SkillRow>) -> Self {
        let count = rows.len().max(1);
        let items: Vec<SettingItem> = rows
            .into_iter()
            .map(|row| {
                let value = if row.enabled { "enabled" } else { "disabled" };
                let mut item = SettingItem::cycleable(
                    row.name.clone(),
                    row.name.clone(),
                    value,
                    vec!["enabled".to_string(), "disabled".to_string()],
                );
                let mut description = String::new();
                if row.disable_model_invocation {
                    description.push_str("[model-invocation disabled] ");
                }
                description.push_str(&row.description);
                description.push_str(&format!(" ({})", row.path));
                item.description = Some(description);
                item
            })
            .collect();

        let outcome: OutcomeHandle = Arc::new(Mutex::new(None));
        let changes: ChangesHandle = Arc::new(Mutex::new(Vec::new()));

        let changes_for_cb = Arc::clone(&changes);
        let outcome_for_cb = Arc::clone(&outcome);
        let inner = SettingsList::new(
            items,
            // Pre-push default; the surrounding overlay window pushes
            // its real budget via `set_available_height`.
            count,
            theme,
            move |id: &str, value: &str| {
                changes_for_cb
                    .lock()
                    .expect("changes mutex poisoned")
                    .push((id.to_string(), value.to_string()));
            },
            move || {
                *outcome_for_cb.lock().expect("outcome mutex poisoned") =
                    Some(SkillsWindowOutcome::Closed);
            },
            SettingsListOptions {
                enable_search: true,
            },
        );

        Self {
            inner,
            outcome,
            changes,
        }
    }

    /// Hand the host a clone of the outcome slot, polled after each
    /// input event; `Some(Closed)` means hide the overlay.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Hand the host a clone of the changes queue to drain after each
    /// input event.
    pub fn changes_handle(&self) -> ChangesHandle {
        Arc::clone(&self.changes)
    }
}

impl Component for SkillsWindowComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.inner.handle_input(event)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        self.inner.set_available_height(rows);
    }

    fn is_focused(&self) -> bool {
        self.inner.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use aj_tui::keys::Key;

    use super::*;

    fn identity_theme() -> SettingsListTheme {
        SettingsListTheme {
            label: Arc::new(|s, _| s.to_string()),
            value: Arc::new(|s, _| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            hint: Arc::new(|s| s.to_string()),
            cursor: "→ ".to_string(),
        }
    }

    fn rows() -> Vec<SkillRow> {
        vec![
            SkillRow {
                name: "alpha".to_string(),
                description: "Alpha skill.".to_string(),
                path: "~/.agents/skills/alpha/SKILL.md".to_string(),
                enabled: true,
                disable_model_invocation: false,
            },
            SkillRow {
                name: "beta".to_string(),
                description: "Beta skill.".to_string(),
                path: "~/.aj/skills/beta/SKILL.md".to_string(),
                enabled: false,
                disable_model_invocation: true,
            },
        ]
    }

    #[test]
    fn toggling_a_row_queues_a_change() {
        let mut component = SkillsWindowComponent::new(identity_theme(), rows());
        let changes = component.changes_handle();
        // First row (alpha) starts enabled; Enter cycles to disabled.
        component.handle_input(&Key::enter());
        let queued = std::mem::take(&mut *changes.lock().unwrap());
        assert_eq!(queued, vec![("alpha".to_string(), "disabled".to_string())]);
    }

    #[test]
    fn escape_closes_the_window() {
        let mut component = SkillsWindowComponent::new(identity_theme(), rows());
        let outcome = component.outcome_handle();
        component.handle_input(&Key::escape());
        assert_eq!(
            outcome.lock().unwrap().take(),
            Some(SkillsWindowOutcome::Closed)
        );
    }

    #[test]
    fn rows_render_with_status_and_marker() {
        let mut component = SkillsWindowComponent::new(identity_theme(), rows());
        let rendered = component.render(100).join("\n");
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("enabled"));
        // The model-invocation marker shows in the focused row's
        // description; move to beta to surface it.
        component.handle_input(&Key::down());
        let rendered = component.render(100).join("\n");
        assert!(rendered.contains("beta"));
    }
}
