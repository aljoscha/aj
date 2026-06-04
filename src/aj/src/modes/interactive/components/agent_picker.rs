//! Agent picker overlay (`alt+a`).
//!
//! Lists the main agent plus the session's sub-agents and lets the user
//! pick one to observe in the main chat view. Modeled on
//! [`crate::modes::interactive::components::thinking_selector`]: a
//! [`SelectList`] wrapped with a shared-state outcome handle the host
//! polls after each input event.
//!
//! Two scopes drive which agents are listed:
//!
//! - **Active** (the default): the main agent plus currently-running
//!   sub-agents — the agents you can usefully watch right now.
//! - **All**: the main agent plus every sub-agent in the session, each
//!   labelled with its status (`running` / `done` / `failed`).
//!
//! The main agent is always present in both scopes so the user can
//! return home. `ctrl+t` ([`ACTION_AGENT_TOGGLE_SCOPE`]) toggles between
//! the two, rebuilding the list in place.

use std::sync::{Arc, Mutex};

use aj_agent::events::AgentId;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::keybindings::ACTION_AGENT_TOGGLE_SCOPE;
use crate::modes::interactive::components::chat_view::AgentEntry;
use crate::modes::interactive::components::subagent_box::SubAgentStatus;

/// Outcome of one picker session. `Confirmed` carries the chosen
/// agent; `Cancelled` is `Esc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentPickerOutcome {
    Confirmed(AgentId),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the overlay
/// writes into.
pub type AgentPickerOutcomeHandle = Arc<Mutex<Option<AgentPickerOutcome>>>;

/// Which agents the picker lists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Active,
    All,
}

/// The overlay's top-level component. Owns the underlying
/// [`SelectList`] and a clone of the outcome slot, plus the snapshot
/// needed to rebuild the list when the scope toggles.
pub struct AgentPickerComponent {
    inner: SelectList,
    outcome: AgentPickerOutcomeHandle,
    /// Full snapshot incl. the main agent (Main first).
    agents: Vec<AgentEntry>,
    active: AgentId,
    scope: Scope,
    theme: SelectListTheme,
}

impl AgentPickerComponent {
    /// Build a fresh picker. `agents` is the full snapshot (Main first);
    /// `active` highlights the currently-observed agent so `Enter`
    /// re-selects it. Opens in [`Scope::Active`].
    pub fn new(theme: SelectListTheme, agents: Vec<AgentEntry>, active: AgentId) -> Self {
        let outcome: AgentPickerOutcomeHandle = Arc::new(Mutex::new(None));
        let scope = Scope::Active;
        let inner = Self::build_list(theme.clone(), &agents, active, scope, &outcome);
        Self {
            inner,
            outcome,
            agents,
            active,
            scope,
            theme,
        }
    }

    /// Build the [`SelectList`] for the given scope, wiring its
    /// callbacks to the shared `outcome` slot.
    fn build_list(
        theme: SelectListTheme,
        agents: &[AgentEntry],
        active: AgentId,
        scope: Scope,
        outcome: &AgentPickerOutcomeHandle,
    ) -> SelectList {
        let items: Vec<SelectItem> = agents
            .iter()
            .filter(|entry| visible_in_scope(entry, scope))
            .map(|entry| {
                let mut label = match entry.id {
                    AgentId::Main => "main agent".to_string(),
                    AgentId::Sub(n) => format!("agent {n}"),
                };
                if entry.id == active {
                    label.push_str(" (current)");
                }
                let mut item = SelectItem::new(&encode(entry.id), &label);
                if let Some(task) = entry.task.as_deref() {
                    item = item.with_description(task);
                }
                // In `All` scope surface the status; in `Active` they
                // are all running, so the column would be noise.
                if scope == Scope::All
                    && let Some(status) = entry.status
                {
                    item = item.with_shortcut(status_label(status));
                }
                item
            })
            .collect();

        let visible_count = items.len().max(1);
        // `max_visible` is the item count (clamped to >= 1); the overlay
        // frame caps the on-screen height.
        let mut list = SelectList::new(items, visible_count, theme, SelectListLayout::default());
        // Pre-select the active agent so the highlight starts there and
        // `Enter` re-selects it. Whether it is present depends on scope;
        // ignore the result.
        list.select_by_value(&encode(active));

        let confirm = Arc::clone(outcome);
        list.on_select = Some(Box::new(move |item| {
            if let Some(id) = decode_agent_id(&item.value) {
                *confirm.lock().expect("agent picker outcome poisoned") =
                    Some(AgentPickerOutcome::Confirmed(id));
            }
        }));
        let cancel = Arc::clone(outcome);
        list.on_cancel = Some(Box::new(move || {
            *cancel.lock().expect("agent picker outcome poisoned") =
                Some(AgentPickerOutcome::Cancelled);
        }));

        list
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> AgentPickerOutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Rebuild the inner list after a scope change.
    fn rebuild(&mut self) {
        self.inner = Self::build_list(
            self.theme.clone(),
            &self.agents,
            self.active,
            self.scope,
            &self.outcome,
        );
    }
}

/// Whether an entry is listed in the given scope. The main agent is
/// always listed; sub-agents are listed in `Active` only while running,
/// and always in `All`.
fn visible_in_scope(entry: &AgentEntry, scope: Scope) -> bool {
    if entry.id == AgentId::Main {
        return true;
    }
    match scope {
        Scope::Active => entry.status == Some(SubAgentStatus::Running),
        Scope::All => true,
    }
}

/// Encode an [`AgentId`] into a [`SelectItem`] value.
fn encode(id: AgentId) -> String {
    match id {
        AgentId::Main => "main".to_string(),
        AgentId::Sub(n) => format!("sub:{n}"),
    }
}

/// Decode a [`SelectItem`] value back into an [`AgentId`].
fn decode_agent_id(value: &str) -> Option<AgentId> {
    if value == "main" {
        return Some(AgentId::Main);
    }
    value
        .strip_prefix("sub:")?
        .parse::<usize>()
        .ok()
        .map(AgentId::Sub)
}

/// Human-readable status label for the `All`-scope shortcut column.
fn status_label(status: SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "running",
        SubAgentStatus::Done => "done",
        SubAgentStatus::Failed => "failed",
    }
}

impl aj_tui::component::Component for AgentPickerComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let header = match self.scope {
            Scope::Active => "Showing: running agents",
            Scope::All => "Showing: all agents",
        };
        let mut lines = Vec::new();
        lines.push(style::dim(header));
        lines.push(String::new());
        lines.extend(self.inner.render(width));
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, ACTION_AGENT_TOGGLE_SCOPE) {
            drop(kb);
            self.scope = match self.scope {
                Scope::Active => Scope::All,
                Scope::All => Scope::Active,
            };
            self.rebuild();
            return true;
        }
        drop(kb);
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
    use aj_tui::keys::Key;

    use super::*;

    /// Identity theme for tests — passes every closure through verbatim
    /// so renders show structural text rather than ANSI escapes.
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

    fn main_entry() -> AgentEntry {
        AgentEntry {
            id: AgentId::Main,
            task: None,
            status: None,
        }
    }

    fn sub_entry(n: usize, status: SubAgentStatus) -> AgentEntry {
        AgentEntry {
            id: AgentId::Sub(n),
            task: Some(format!("task {n}")),
            status: Some(status),
        }
    }

    /// Main + a running Sub(1) + a finished Sub(2).
    fn fixture() -> Vec<AgentEntry> {
        vec![
            main_entry(),
            sub_entry(1, SubAgentStatus::Running),
            sub_entry(2, SubAgentStatus::Done),
        ]
    }

    #[test]
    fn active_scope_lists_main_and_running_only() {
        let mut picker = AgentPickerComponent::new(identity_theme(), fixture(), AgentId::Main);
        let body = picker.render(80).join("\n");
        assert!(body.contains("Showing: running agents"), "got: {body}");
        assert!(body.contains("main agent"), "got: {body}");
        assert!(body.contains("agent 1"), "got: {body}");
        assert!(!body.contains("agent 2"), "got: {body}");
    }

    #[test]
    fn toggle_scope_reveals_finished_subs() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut picker = AgentPickerComponent::new(identity_theme(), fixture(), AgentId::Main);
        assert!(picker.handle_input(&Key::ctrl('t')));
        let body = picker.render(80).join("\n");
        assert!(body.contains("Showing: all agents"), "got: {body}");
        assert!(body.contains("agent 2"), "got: {body}");
        // The finished sub surfaces its status in the shortcut column.
        assert!(body.contains("done"), "got: {body}");
    }

    #[test]
    fn confirm_emits_decoded_active_agent() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut picker = AgentPickerComponent::new(identity_theme(), fixture(), AgentId::Main);
        let handle = picker.outcome_handle();
        // Main is pre-selected (it's the active agent in the fixture).
        assert!(picker.handle_input(&Key::enter()));
        assert_eq!(
            handle.lock().unwrap().take(),
            Some(AgentPickerOutcome::Confirmed(AgentId::Main))
        );
    }

    #[test]
    fn cancel_emits_cancelled() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut picker = AgentPickerComponent::new(identity_theme(), fixture(), AgentId::Main);
        let handle = picker.outcome_handle();
        assert!(picker.handle_input(&Key::escape()));
        assert_eq!(
            handle.lock().unwrap().take(),
            Some(AgentPickerOutcome::Cancelled)
        );
    }

    #[test]
    fn decode_agent_id_round_trips_and_rejects_garbage() {
        assert_eq!(decode_agent_id("main"), Some(AgentId::Main));
        assert_eq!(decode_agent_id("sub:3"), Some(AgentId::Sub(3)));
        assert_eq!(decode_agent_id("bogus"), None);
    }

    #[test]
    fn main_always_present_with_no_running_subs() {
        let agents = vec![
            main_entry(),
            sub_entry(1, SubAgentStatus::Done),
            sub_entry(2, SubAgentStatus::Failed),
        ];
        let mut picker = AgentPickerComponent::new(identity_theme(), agents, AgentId::Main);
        let body = picker.render(80).join("\n");
        assert!(body.contains("main agent"), "got: {body}");
        // No running subs, so none of them appear in Active scope.
        assert!(!body.contains("agent 1"), "got: {body}");
        assert!(!body.contains("agent 2"), "got: {body}");
    }
}
