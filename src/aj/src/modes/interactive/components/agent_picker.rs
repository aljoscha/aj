//! Agent picker overlay (`alt+a`).
//!
//! Lists the main agent plus the session's sub-agents and lets the user
//! pick one to observe in the main chat view. Modeled on
//! [`crate::modes::interactive::components::thinking_selector`]: a
//! [`SelectList`] wrapped with a shared-state outcome handle the host
//! polls after each input event.
//!
//! Background bash tasks are listed beneath the agents (agent-backed
//! tasks are not duplicated here — the sub-agent entry already covers
//! them). Selecting a task entry jumps to the owning agent's
//! transcript; `ctrl+k` ([`ACTION_TASK_KILL`]) on a running task asks
//! the host to kill it.
//!
//! Two scopes drive which agents and tasks are listed:
//!
//! - **Active** (the default): the main agent plus currently-running
//!   sub-agents and tasks — the work you can usefully watch right now.
//! - **All**: the main agent plus every sub-agent and task in the
//!   session, each labelled with its status.
//!
//! The main agent is always present in both scopes so the user can
//! return home. `ctrl+t` ([`ACTION_AGENT_TOGGLE_SCOPE`]) toggles between
//! the two, rebuilding the list in place.

use std::time::Duration;

use aj_agent::events::AgentId;
use aj_agent::tool::{TaskId, TaskStatus};
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::keybindings::{ACTION_AGENT_TOGGLE_SCOPE, ACTION_TASK_KILL};
use crate::modes::interactive::components::chat_view::AgentEntry;
use crate::modes::interactive::components::outcome::OutcomeSlot;
use crate::modes::interactive::components::subagent_box::SubAgentStatus;

/// A description of a known background bash task, suitable for a
/// picker row. Snapshotted by the event pump when the picker opens.
#[derive(Clone, Debug)]
pub struct TaskPickerEntry {
    pub id: TaskId,
    /// Display label — the command line for bash tasks.
    pub label: String,
    pub status: TaskStatus,
    /// Elapsed runtime; frozen at the task's end for terminal tasks.
    pub runtime: Duration,
}

/// Outcome of one picker session. `Confirmed` carries the chosen
/// agent; `ConfirmedTask` a chosen background task (the host jumps
/// to its owner's transcript); `KillTask` asks the host to kill the
/// task through the registry; `Cancelled` is `Esc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentPickerOutcome {
    Confirmed(AgentId),
    ConfirmedTask(TaskId),
    KillTask(TaskId),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the overlay
/// writes into.
pub type AgentPickerOutcomeHandle = OutcomeSlot<AgentPickerOutcome>;

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
    /// Background bash tasks, in id order.
    tasks: Vec<TaskPickerEntry>,
    active: AgentId,
    scope: Scope,
    theme: SelectListTheme,
}

impl AgentPickerComponent {
    /// Build a fresh picker. `agents` is the full agent snapshot
    /// (Main first), `tasks` the background-bash-task snapshot;
    /// `active` highlights the currently-observed agent so `Enter`
    /// re-selects it. Opens in [`Scope::Active`].
    pub fn new(
        theme: SelectListTheme,
        agents: Vec<AgentEntry>,
        tasks: Vec<TaskPickerEntry>,
        active: AgentId,
    ) -> Self {
        let outcome = AgentPickerOutcomeHandle::new();
        let scope = Scope::Active;
        let inner = Self::build_list(theme.clone(), &agents, &tasks, active, scope, &outcome);
        Self {
            inner,
            outcome,
            agents,
            tasks,
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
        tasks: &[TaskPickerEntry],
        active: AgentId,
        scope: Scope,
        outcome: &AgentPickerOutcomeHandle,
    ) -> SelectList {
        let mut items: Vec<SelectItem> = agents
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
        items.extend(
            tasks
                .iter()
                .filter(|task| scope == Scope::All || task.status == TaskStatus::Running)
                .map(|task| {
                    let label = format!("{} task #{}", task_glyph(task.status), task.id);
                    // The list's right column renders either the
                    // shortcut or the description, never both — so
                    // the runtime (and, in `All` scope, the status
                    // word the glyph alone can't disambiguate) rides
                    // in the description next to the command.
                    let runtime = format_runtime(task.runtime);
                    let desc = match scope {
                        Scope::Active => format!("{} · {}", command_tail(&task.label), runtime),
                        Scope::All => format!(
                            "{} · {} · {}",
                            command_tail(&task.label),
                            task_status_label(task.status),
                            runtime,
                        ),
                    };
                    SelectItem::new(&encode_task(task.id), &label).with_description(&desc)
                }),
        );

        let visible_count = items.len().max(1);
        // `max_visible` is the item count (clamped to >= 1); the overlay
        // frame caps the on-screen height.
        let mut list = SelectList::new(items, visible_count, theme, SelectListLayout::default());
        // Pre-select the active agent so the highlight starts there and
        // `Enter` re-selects it. Whether it is present depends on scope;
        // ignore the result.
        list.select_by_value(&encode(active));

        let confirm = outcome.clone();
        list.on_select = Some(Box::new(move |item| {
            let chosen = if let Some(id) = decode_agent_id(&item.value) {
                Some(AgentPickerOutcome::Confirmed(id))
            } else {
                decode_task_id(&item.value).map(AgentPickerOutcome::ConfirmedTask)
            };
            if let Some(chosen) = chosen {
                confirm.set(chosen);
            }
        }));
        let cancel = outcome.clone();
        list.on_cancel = Some(Box::new(move || {
            cancel.set(AgentPickerOutcome::Cancelled);
        }));

        list
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> AgentPickerOutcomeHandle {
        self.outcome.clone()
    }

    /// Whether the picker is currently showing all agents (as opposed
    /// to only running ones), for the border key-hint.
    pub fn showing_all(&self) -> bool {
        self.scope == Scope::All
    }

    /// Whether the picker carries any running task entries, for the
    /// border key-hint (the kill chord only acts on running tasks, so
    /// it is only advertised when one is listed).
    pub fn has_killable_tasks(&self) -> bool {
        self.tasks.iter().any(|t| t.status == TaskStatus::Running)
    }

    /// Rebuild the inner list after a scope change.
    fn rebuild(&mut self) {
        self.inner = Self::build_list(
            self.theme.clone(),
            &self.agents,
            &self.tasks,
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

/// Encode a [`TaskId`] into a [`SelectItem`] value.
fn encode_task(id: TaskId) -> String {
    format!("task:{id}")
}

/// Decode a [`SelectItem`] value back into a [`TaskId`].
fn decode_task_id(value: &str) -> Option<TaskId> {
    value.strip_prefix("task:")?.parse::<TaskId>().ok()
}

/// Human-readable status label for the `All`-scope shortcut column.
fn status_label(status: SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "running",
        SubAgentStatus::Done => "done",
        SubAgentStatus::Failed => "failed",
    }
}

/// Status glyph prefixed to a task row's label, matching the tool
/// cell's header glyphs.
fn task_glyph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "…",
        TaskStatus::Exited(Some(0)) => "✓",
        TaskStatus::Exited(_) | TaskStatus::Killed => "✗",
    }
}

/// Human-readable status label for a task row's shortcut column.
fn task_status_label(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exited {code}"),
        TaskStatus::Exited(None) => "signalled".to_string(),
        TaskStatus::Killed => "killed".to_string(),
    }
}

/// Cap a task's command label for the description column, keeping
/// the tail — for long command lines the trailing part (file names,
/// the actual command after env/cd prefixes) is usually the
/// distinguishing bit.
fn command_tail(command: &str) -> String {
    const MAX: usize = 60;
    // Collapse newlines so multi-line commands stay on one row.
    let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= MAX {
        return flat;
    }
    let tail: String = chars[chars.len() - (MAX - 1)..].iter().collect();
    format!("…{tail}")
}

/// Compact `1m 23s`-style runtime formatter for task rows.
fn format_runtime(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
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
        if kb.matches(event, ACTION_TASK_KILL) {
            drop(kb);
            // Kill only acts on a selected, still-running task row;
            // on anything else the chord is consumed but inert (the
            // picker is a capturing overlay, so letting it fall
            // through would do nothing useful anyway).
            if let Some(id) = self
                .inner
                .selected_item()
                .and_then(|i| decode_task_id(&i.value))
                && self
                    .tasks
                    .iter()
                    .any(|t| t.id == id && t.status == TaskStatus::Running)
            {
                self.outcome.set(AgentPickerOutcome::KillTask(id));
            }
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

    fn task_entry(id: TaskId, status: TaskStatus, secs: u64) -> TaskPickerEntry {
        TaskPickerEntry {
            id,
            label: format!("cargo build --task-{id}"),
            status,
            runtime: Duration::from_secs(secs),
        }
    }

    fn picker(
        agents: Vec<AgentEntry>,
        tasks: Vec<TaskPickerEntry>,
        active: AgentId,
    ) -> AgentPickerComponent {
        AgentPickerComponent::new(identity_theme(), agents, tasks, active)
    }

    #[test]
    fn active_scope_lists_main_and_running_only() {
        let mut picker = picker(fixture(), Vec::new(), AgentId::Main);
        let body = picker.render(80).join("\n");
        assert!(body.contains("Showing: running agents"), "got: {body}");
        assert!(body.contains("main agent"), "got: {body}");
        assert!(body.contains("agent 1"), "got: {body}");
        assert!(!body.contains("agent 2"), "got: {body}");
    }

    #[test]
    fn toggle_scope_reveals_finished_subs() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut picker = picker(fixture(), Vec::new(), AgentId::Main);
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
        let mut picker = picker(fixture(), Vec::new(), AgentId::Main);
        let handle = picker.outcome_handle();
        // Main is pre-selected (it's the active agent in the fixture).
        assert!(picker.handle_input(&Key::enter()));
        assert_eq!(
            handle.take(),
            Some(AgentPickerOutcome::Confirmed(AgentId::Main))
        );
    }

    #[test]
    fn cancel_emits_cancelled() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut picker = picker(fixture(), Vec::new(), AgentId::Main);
        let handle = picker.outcome_handle();
        assert!(picker.handle_input(&Key::escape()));
        assert_eq!(handle.take(), Some(AgentPickerOutcome::Cancelled));
    }

    #[test]
    fn decode_agent_id_round_trips_and_rejects_garbage() {
        assert_eq!(decode_agent_id("main"), Some(AgentId::Main));
        assert_eq!(decode_agent_id("sub:3"), Some(AgentId::Sub(3)));
        assert_eq!(decode_agent_id("bogus"), None);
        assert_eq!(decode_task_id("task:7"), Some(7));
        assert_eq!(decode_task_id("sub:3"), None);
    }

    #[test]
    fn main_always_present_with_no_running_subs() {
        let agents = vec![
            main_entry(),
            sub_entry(1, SubAgentStatus::Done),
            sub_entry(2, SubAgentStatus::Failed),
        ];
        let mut picker = picker(agents, Vec::new(), AgentId::Main);
        let body = picker.render(80).join("\n");
        assert!(body.contains("main agent"), "got: {body}");
        // No running subs, so none of them appear in Active scope.
        assert!(!body.contains("agent 1"), "got: {body}");
        assert!(!body.contains("agent 2"), "got: {body}");
    }

    #[test]
    fn task_rows_render_beneath_agents_with_status_and_runtime() {
        let tasks = vec![
            task_entry(1, TaskStatus::Running, 83),
            task_entry(2, TaskStatus::Exited(Some(0)), 5),
        ];
        let mut picker = picker(fixture(), tasks, AgentId::Main);
        let body = picker.render(100).join("\n");
        // Active scope: only the running task is listed, with its
        // glyph, command, and runtime.
        assert!(body.contains("… task #1"), "got: {body}");
        assert!(
            body.contains("cargo build --task-1 · 1m 23s"),
            "got: {body}"
        );
        assert!(!body.contains("task #2"), "got: {body}");
        // Tasks come after the agent rows.
        let agent_pos = body.find("agent 1").expect("agent row");
        let task_pos = body.find("task #1").expect("task row");
        assert!(agent_pos < task_pos, "got: {body}");
    }

    #[test]
    fn all_scope_reveals_finished_tasks() {
        crate::config::keybindings::install_global_manager_defaults();
        let tasks = vec![
            task_entry(1, TaskStatus::Exited(Some(0)), 5),
            task_entry(2, TaskStatus::Killed, 9),
        ];
        let mut picker = picker(fixture(), tasks, AgentId::Main);
        assert!(picker.handle_input(&Key::ctrl('t')));
        let body = picker.render(100).join("\n");
        assert!(body.contains("✓ task #1"), "got: {body}");
        assert!(body.contains("exited 0 · 5s"), "got: {body}");
        assert!(body.contains("✗ task #2"), "got: {body}");
        assert!(body.contains("killed · 9s"), "got: {body}");
    }

    #[test]
    fn confirming_a_task_row_emits_confirmed_task() {
        crate::config::keybindings::install_global_manager_defaults();
        let tasks = vec![task_entry(3, TaskStatus::Running, 1)];
        let mut picker = picker(vec![main_entry()], tasks, AgentId::Main);
        let handle = picker.outcome_handle();
        // Move from the pre-selected Main row onto the task row.
        assert!(picker.handle_input(&Key::down()));
        assert!(picker.handle_input(&Key::enter()));
        assert_eq!(handle.take(), Some(AgentPickerOutcome::ConfirmedTask(3)));
    }

    #[test]
    fn kill_chord_emits_kill_task_for_a_running_task_only() {
        crate::config::keybindings::install_global_manager_defaults();
        let tasks = vec![task_entry(3, TaskStatus::Running, 1)];
        let mut picker = picker(vec![main_entry()], tasks, AgentId::Main);
        let handle = picker.outcome_handle();

        // On the Main row the chord is consumed but emits nothing.
        assert!(picker.handle_input(&Key::ctrl('k')));
        assert_eq!(handle.take(), None);

        // On the running task row it requests the kill.
        assert!(picker.handle_input(&Key::down()));
        assert!(picker.handle_input(&Key::ctrl('k')));
        assert_eq!(handle.take(), Some(AgentPickerOutcome::KillTask(3)));
    }

    #[test]
    fn kill_chord_is_inert_on_a_finished_task() {
        crate::config::keybindings::install_global_manager_defaults();
        let tasks = vec![task_entry(4, TaskStatus::Exited(Some(1)), 2)];
        let mut picker = picker(vec![main_entry()], tasks, AgentId::Main);
        // All scope so the finished task is listed at all.
        assert!(picker.handle_input(&Key::ctrl('t')));
        let handle = picker.outcome_handle();
        assert!(picker.handle_input(&Key::down()));
        assert!(picker.handle_input(&Key::ctrl('k')));
        assert_eq!(handle.take(), None);
    }

    #[test]
    fn command_tail_keeps_the_end_of_long_commands() {
        let long = format!(
            "FOO=bar cd /some/deep/dir && {} target-file.rs",
            "x".repeat(80)
        );
        let tail = command_tail(&long);
        assert!(tail.starts_with('…'), "got: {tail}");
        assert!(tail.ends_with("target-file.rs"), "got: {tail}");
        assert!(tail.chars().count() <= 60, "got: {tail}");
        // Short commands pass through untouched.
        assert_eq!(command_tail("echo hi"), "echo hi");
    }

    #[test]
    fn format_runtime_spans_all_bands() {
        assert_eq!(format_runtime(Duration::from_secs(9)), "9s");
        assert_eq!(format_runtime(Duration::from_secs(83)), "1m 23s");
        assert_eq!(format_runtime(Duration::from_secs(3_725)), "1h 2m");
    }
}
