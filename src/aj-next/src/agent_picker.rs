//! The agent-picker overlay: switch the viewed transcript between the
//! main agent and any sub-agent, or drill into a background task.
//!
//! A [`FilterableSelect`] lists the live agents (main plus sub-agents)
//! and the background bash tasks. Confirming an agent row parks an
//! [`AgentPickerOutcome::Observe`] the host applies with
//! [`aj_app::chat::ChatState::set_active_view`]. Confirming a task row
//! drills into the task-output viewer. Two overlay-local chords act
//! at-target (Spec F), so they never enter the global keymap:
//!
//! - `Ctrl+T` ([`ACTION_AGENT_TOGGLE_SCOPE`]) flips the scope between
//!   "running only" and "all", rebuilding the row set in place.
//! - `Ctrl+K` ([`ACTION_TASK_KILL`]) on a running task row parks an
//!   [`AgentPickerOutcome::Kill`] and closes.
//!
//! The main agent is always listed so the user can return home. The
//! current view is pre-selected. Task rows are recovered on confirm by
//! decoding the row's filter key, which encodes the target's identity
//! (a self-contained scheme, so the confirm callback needs no shared
//! lookup table).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use aj_agent::events::AgentId;
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use aj_app::chat::{AgentEntry, ChatState, SubAgentStatus};
use aj_app::keybindings::{ACTION_AGENT_TOGGLE_SCOPE, ACTION_TASK_KILL, default_action_shortcut};
use vaxis::key::Modifiers;
use vaxis::vxfw::{
    DrawContext, Event, EventContext, FilterableSelect, OverlayWindow, RelativePoint, SelectItem,
    SubSurface, Surface, Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_key_label, close_top, confirm_key_label,
};
use crate::settings_ui::push_window;

/// Filter-key prefix marking a task row, so the confirm path can decode
/// a [`TaskId`] back out of the row it landed on.
const TASK_PREFIX: &str = "task #";
/// Filter-key prefix for a sub-agent row.
const AGENT_PREFIX: &str = "agent ";
/// Filter key of the main-agent row.
const MAIN_KEY: &str = "main agent";

/// What confirming (or `Ctrl+K`-ing) a picker row asks the host to do.
///
/// The host owns the session world (the chat model, the task registry),
/// which the widget can't reach, so the widget records intent and the
/// drive loop applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentPickerOutcome {
    /// Switch the viewed transcript to this agent.
    Observe(AgentId),
    /// Drill into this task's output viewer.
    OpenTask(TaskId),
    /// Kill this (still-running at snapshot time) background task.
    Kill(TaskId),
}

/// Which agents and tasks the picker lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// The main agent plus currently-running sub-agents and tasks: the
    /// work worth watching right now.
    Running,
    /// The main agent plus every sub-agent and task in the session.
    All,
}

/// A background bash task, snapshotted for a picker row.
#[derive(Clone, Debug)]
pub(crate) struct TaskRow {
    pub(crate) id: TaskId,
    /// Command line, shown (tail-truncated) in the row description.
    pub(crate) command: String,
    pub(crate) status: TaskStatus,
    /// Elapsed runtime, frozen at the task's end for terminal tasks.
    pub(crate) runtime: Duration,
}

/// Snapshot of the picker-relevant world state, gathered by the host at
/// open time (the widget can't reach the world).
pub(crate) struct PickerSnapshot {
    pub(crate) agents: Vec<AgentEntry>,
    pub(crate) tasks: Vec<TaskRow>,
    pub(crate) active: AgentId,
}

impl PickerSnapshot {
    /// Gather the live agents and background bash tasks from the chat
    /// model.
    ///
    /// Agent-backed tasks are skipped: their sub-agent already appears
    /// as an agent row, so a task row would duplicate it.
    pub(crate) fn gather(chat: &ChatState) -> PickerSnapshot {
        let now = Instant::now();
        let tasks = chat
            .tasks()
            .iter()
            .filter(|(_, info)| matches!(info.kind, TaskKind::Bash { .. }))
            .map(|(&id, info)| TaskRow {
                id,
                command: info.label.clone(),
                status: info.status,
                runtime: info
                    .finished_at
                    .unwrap_or(now)
                    .duration_since(info.started_at),
            })
            .collect();
        PickerSnapshot {
            agents: chat.agents(),
            tasks,
            active: chat.active_view(),
        }
    }
}

/// The agent-picker widget: a [`FilterableSelect`] plus the snapshot and
/// scope needed to rebuild its rows on a scope toggle, and the handles
/// it needs to park an outcome and close.
pub(crate) struct AgentPicker {
    select: Rc<RefCell<FilterableSelect>>,
    agents: Vec<AgentEntry>,
    tasks: Vec<TaskRow>,
    active: AgentId,
    scope: Scope,
    /// The window frame, kept so a scope toggle can refresh its dynamic
    /// subtitle. `None` until [`AgentPicker::set_window`] wires it after
    /// the push.
    window: Option<Rc<RefCell<OverlayWindow>>>,
    outcome: Rc<RefCell<Option<AgentPickerOutcome>>>,
    stack: Rc<RefCell<OverlayStack>>,
    editor: WidgetRef,
}

impl AgentPicker {
    fn set_window(&mut self, window: Rc<RefCell<OverlayWindow>>) {
        self.window = Some(window);
    }

    /// Whether the picker lists any running task, for the kill hint (the
    /// kill chord only acts on running tasks, so it is advertised only
    /// when one is present).
    fn has_killable_tasks(&self) -> bool {
        self.tasks.iter().any(|t| t.status == TaskStatus::Running)
    }

    /// The scope-toggle subtitle, resolved from keybinding data: the
    /// toggle hint names the scope it would switch *to*, and the kill
    /// hint appears only while a running task is listed.
    fn subtitle(&self) -> String {
        let scope = default_action_shortcut(ACTION_AGENT_TOGGLE_SCOPE)
            .expect("aj.agent.toggle_scope has a default chord");
        let scope_target = match self.scope {
            Scope::All => "running agents",
            Scope::Running => "all agents",
        };
        let confirm = confirm_key_label();
        let close = close_key_label();
        let mut hint =
            format!("{confirm} to observe  \u{2022}  {scope} {scope_target}  \u{2022}  ");
        if self.has_killable_tasks() {
            let kill = default_action_shortcut(ACTION_TASK_KILL)
                .expect("aj.task.kill has a default chord");
            hint.push_str(&format!("{kill} kill task  \u{2022}  "));
        }
        hint.push_str(&format!("{close} to close"));
        hint
    }

    /// Rebuild the row set for the current scope, preserving the active
    /// agent's highlight, and refresh the window subtitle.
    fn rebuild(&self) {
        let items = build_items(&self.agents, &self.tasks, self.active, self.scope);
        let select = self.select.borrow();
        select.set_items(items);
        preselect_active(&select, self.active);
        if let Some(window) = &self.window {
            window.borrow_mut().subtitle = self.subtitle();
        }
    }
}

impl Widget for AgentPicker {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Wrap the select's surface as a child so both identities stay on
        // the focus path: a bare return would let the caller's
        // `draw_widget` re-stamp it with the picker's identity and drop
        // the select (hence its capture chords) off the path.
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.select)), ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        // Overlay-local scope toggle (Spec F): matched here at-target,
        // ahead of the inner select, and rebuilds the rows in place.
        if key.matches(u32::from('t'), Modifiers::CTRL) {
            self.scope = match self.scope {
                Scope::Running => Scope::All,
                Scope::All => Scope::Running,
            };
            self.rebuild();
            ctx.consume_and_redraw();
            return;
        }
        // Overlay-local kill: acts only on a selected, still-running task
        // row. On anything else the chord is consumed but inert, matching
        // the capturing overlay's swallow-everything contract.
        if key.matches(u32::from('k'), Modifiers::CTRL) {
            if let Some(id) = self
                .select
                .borrow()
                .selected()
                .and_then(|item| decode_task(&item.filter_key))
                && self
                    .tasks
                    .iter()
                    .any(|t| t.id == id && t.status == TaskStatus::Running)
            {
                *self.outcome.borrow_mut() = Some(AgentPickerOutcome::Kill(id));
                close_top(&self.stack, ctx, &self.editor);
            }
            ctx.consume_and_redraw();
        }
        // Everything else (Enter/Esc/nav/typing) falls through to the
        // inner select and the filter field below it.
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Whether an agent entry is listed in `scope`. Main is always listed.
/// A sub-agent shows in `Running` scope only while running, and always
/// in `All`.
fn agent_visible(entry: &AgentEntry, scope: Scope) -> bool {
    if entry.id == AgentId::Main {
        return true;
    }
    match scope {
        Scope::Running => entry.status == Some(SubAgentStatus::Running),
        Scope::All => true,
    }
}

/// Build the row set: agents first, then tasks, filtered by scope.
fn build_items(
    agents: &[AgentEntry],
    tasks: &[TaskRow],
    active: AgentId,
    scope: Scope,
) -> Vec<SelectItem> {
    let mut items: Vec<SelectItem> = agents
        .iter()
        .filter(|entry| agent_visible(entry, scope))
        .map(|entry| agent_item(entry, active, scope))
        .collect();
    items.extend(
        tasks
            .iter()
            .filter(|t| scope == Scope::All || t.status == TaskStatus::Running)
            .map(|t| task_item(t, scope)),
    );
    items
}

/// One agent row. The label carries the `(current)` tag. The filter key
/// stays clean so it both decodes unambiguously and fuzzy-matches the
/// human name (and, for a sub, its task text).
fn agent_item(entry: &AgentEntry, active: AgentId, scope: Scope) -> SelectItem {
    let name = match entry.id {
        AgentId::Main => MAIN_KEY.to_string(),
        AgentId::Sub(n) => format!("{AGENT_PREFIX}{n}"),
    };
    let mut label = name.clone();
    if entry.id == active {
        label.push_str(" (current)");
    }
    // Sub rows carry their task in the filter key so a query matches it,
    // and in the description so it shows.
    let filter_key = match &entry.task {
        Some(task) => format!("{name} {task}"),
        None => name,
    };
    let mut item = SelectItem::new(label, filter_key);
    if let Some(task) = &entry.task {
        let description = match (scope, entry.status) {
            (Scope::All, Some(status)) => format!("{task} \u{b7} {}", sub_status_label(status)),
            _ => task.clone(),
        };
        item = item.with_description(description);
    }
    item
}

/// One task row: a status-glyphed label, with the command tail (and, in
/// `All` scope, the status word) plus the runtime in the description.
fn task_item(task: &TaskRow, scope: Scope) -> SelectItem {
    let label = format!("{} {TASK_PREFIX}{}", task_glyph(task.status), task.id);
    let runtime = format_runtime(task.runtime);
    let tail = command_tail(&task.command);
    let description = match scope {
        Scope::Running => format!("{tail} \u{b7} {runtime}"),
        Scope::All => format!(
            "{tail} \u{b7} {} \u{b7} {runtime}",
            task_status_label(task.status)
        ),
    };
    // The command tail rides in the filter key too so a query matches
    // the command, not just the `task #N` label.
    SelectItem::new(label, format!("{TASK_PREFIX}{} {tail}", task.id)).with_description(description)
}

/// Move the highlight onto the active agent's row on open.
fn preselect_active(select: &FilterableSelect, active: AgentId) {
    select.select_matching(|item| decode_agent(&item.filter_key) == Some(active));
}

/// Decode an agent id from a row's filter key, or `None` for a task row.
fn decode_agent(filter_key: &str) -> Option<AgentId> {
    if filter_key == MAIN_KEY {
        return Some(AgentId::Main);
    }
    filter_key
        .strip_prefix(AGENT_PREFIX)?
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()
        .map(AgentId::Sub)
}

/// Decode a task id from a row's filter key, or `None` for an agent row.
fn decode_task(filter_key: &str) -> Option<TaskId> {
    filter_key
        .strip_prefix(TASK_PREFIX)?
        .split_whitespace()
        .next()?
        .parse::<TaskId>()
        .ok()
}

/// Status glyph prefixed to a task row's label.
fn task_glyph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "\u{2026}",
        TaskStatus::Exited(Some(0)) => "\u{2713}",
        TaskStatus::Exited(_) | TaskStatus::Killed => "\u{2717}",
    }
}

/// Human-readable status word for a task row's description.
fn task_status_label(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exited {code}"),
        TaskStatus::Exited(None) => "signalled".to_string(),
        TaskStatus::Killed => "killed".to_string(),
    }
}

/// Human-readable status word for a sub-agent row's description.
fn sub_status_label(status: SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "running",
        SubAgentStatus::Done => "done",
    }
}

/// Cap a task's command for the description column, keeping the tail:
/// for long command lines the trailing part (file names, the actual
/// command after env/cd prefixes) is usually the distinguishing bit.
fn command_tail(command: &str) -> String {
    const MAX: usize = 60;
    // Collapse whitespace so a multi-line command stays on one row.
    let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= MAX {
        return flat;
    }
    let tail: String = chars[chars.len() - (MAX - 1)..].iter().collect();
    format!("\u{2026}{tail}")
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

/// Open the agent picker over `snapshot`, pushing it onto `stack` and
/// pre-selecting the active view. Confirmed picks and kills land in
/// `outcome` for the host to apply. Does not move focus: the caller
/// (host) posts the refocus event.
pub(crate) fn open_agent_picker(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    outcome: &Rc<RefCell<Option<AgentPickerOutcome>>>,
    snapshot: PickerSnapshot,
) {
    let scope = Scope::Running;
    let PickerSnapshot {
        agents,
        tasks,
        active,
    } = snapshot;
    let items = build_items(&agents, &tasks, active, scope);
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        items,
        chrome.select.clone(),
    )));
    preselect_active(&select.borrow(), active);
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        let outcome_c = Rc::clone(outcome);
        let stack_c = Rc::clone(stack);
        let editor_c = Rc::clone(editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            let chosen = decode_agent(&item.filter_key)
                .map(AgentPickerOutcome::Observe)
                .or_else(|| decode_task(&item.filter_key).map(AgentPickerOutcome::OpenTask));
            if let Some(chosen) = chosen {
                *outcome_c.borrow_mut() = Some(chosen);
            }
            close_top(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(stack);
        let editor_cancel = Rc::clone(editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    let picker = Rc::new(RefCell::new(AgentPicker {
        select: Rc::clone(&select),
        agents,
        tasks,
        active,
        scope,
        window: None,
        outcome: Rc::clone(outcome),
        stack: Rc::clone(stack),
        editor: Rc::clone(editor),
    }));
    let subtitle = picker.borrow().subtitle();
    let window = push_window(
        stack,
        chrome,
        "Agents",
        subtitle,
        to_widget_ref(Rc::clone(&picker)),
        focus,
        OverlayPlacement::Small,
    );
    picker.borrow_mut().set_window(window);
}

#[cfg(test)]
mod tests {
    use vaxis::key::Key;
    use vaxis::vxfw::{Phase, SelectStyles};

    use super::*;

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

    fn task_row(id: TaskId, status: TaskStatus, secs: u64) -> TaskRow {
        TaskRow {
            id,
            command: format!("cargo build --task-{id}"),
            status,
            runtime: Duration::from_secs(secs),
        }
    }

    fn labels(items: &[SelectItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn running_scope_lists_main_and_running_only() {
        let agents = vec![
            main_entry(),
            sub_entry(1, SubAgentStatus::Running),
            sub_entry(2, SubAgentStatus::Done),
        ];
        let items = build_items(&agents, &[], AgentId::Main, Scope::Running);
        let labels = labels(&items);
        assert!(labels.iter().any(|l| l.starts_with("main agent")));
        assert!(labels.iter().any(|l| l.starts_with("agent 1")));
        assert!(!labels.iter().any(|l| l.starts_with("agent 2")));
        // The active agent is tagged.
        assert!(labels.iter().any(|l| l == "main agent (current)"));
    }

    #[test]
    fn all_scope_reveals_finished_subs_and_tasks() {
        let agents = vec![main_entry(), sub_entry(2, SubAgentStatus::Done)];
        let tasks = vec![
            task_row(1, TaskStatus::Running, 83),
            task_row(2, TaskStatus::Exited(Some(0)), 5),
        ];
        let running = build_items(&agents, &tasks, AgentId::Main, Scope::Running);
        assert!(!labels(&running).iter().any(|l| l.starts_with("agent 2")));
        // Only the running task shows in the running scope.
        assert!(labels(&running).iter().any(|l| l.contains("task #1")));
        assert!(!labels(&running).iter().any(|l| l.contains("task #2")));

        let all = build_items(&agents, &tasks, AgentId::Main, Scope::All);
        assert!(labels(&all).iter().any(|l| l.starts_with("agent 2")));
        assert!(labels(&all).iter().any(|l| l.contains("task #2")));
        // Finished sub surfaces its status in the description.
        let sub2 = all
            .iter()
            .find(|i| i.label.starts_with("agent 2"))
            .expect("sub 2 row");
        assert!(
            sub2.description
                .as_deref()
                .is_some_and(|d| d.contains("done")),
            "{sub2:?}"
        );
    }

    #[test]
    fn task_rows_carry_glyph_command_and_runtime() {
        let tasks = vec![task_row(1, TaskStatus::Running, 83)];
        let items = build_items(&[main_entry()], &tasks, AgentId::Main, Scope::Running);
        let task = items.iter().find(|i| i.label.contains("task #1")).unwrap();
        assert!(task.label.starts_with('\u{2026}'), "{task:?}");
        let desc = task.description.as_deref().unwrap();
        assert!(desc.contains("cargo build --task-1"), "{desc}");
        assert!(desc.contains("1m 23s"), "{desc}");
    }

    #[test]
    fn decode_round_trips_and_rejects_the_wrong_kind() {
        assert_eq!(decode_agent("main agent"), Some(AgentId::Main));
        assert_eq!(decode_agent("agent 3 do things"), Some(AgentId::Sub(3)));
        assert_eq!(decode_agent("task #7 cmd"), None);
        assert_eq!(decode_task("task #7 cmd"), Some(7));
        assert_eq!(decode_task("agent 3"), None);
    }

    #[test]
    fn preselects_the_active_agent() {
        let agents = vec![main_entry(), sub_entry(1, SubAgentStatus::Running)];
        let items = build_items(&agents, &[], AgentId::Sub(1), Scope::Running);
        let select = FilterableSelect::new(items, SelectStyles::default());
        preselect_active(&select, AgentId::Sub(1));
        assert_eq!(
            select.selected().and_then(|i| decode_agent(&i.filter_key)),
            Some(AgentId::Sub(1)),
            "the active sub-agent starts highlighted"
        );
    }

    #[test]
    fn command_tail_keeps_the_end_of_long_commands() {
        let long = format!("FOO=bar cd /deep && {} target.rs", "x".repeat(80));
        let tail = command_tail(&long);
        assert!(tail.starts_with('\u{2026}'), "{tail}");
        assert!(tail.ends_with("target.rs"), "{tail}");
        assert!(tail.chars().count() <= 60, "{tail}");
        assert_eq!(command_tail("echo hi"), "echo hi");
    }

    #[test]
    fn format_runtime_spans_bands() {
        assert_eq!(format_runtime(Duration::from_secs(9)), "9s");
        assert_eq!(format_runtime(Duration::from_secs(83)), "1m 23s");
        assert_eq!(format_runtime(Duration::from_secs(3_725)), "1h 2m");
    }

    #[test]
    fn subtitle_hides_kill_hint_without_running_tasks_and_resolves_labels() {
        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let editor: WidgetRef = Rc::new(RefCell::new(crate::overlay::Scrim));
        let outcome = Rc::new(RefCell::new(None));
        let make = |tasks: Vec<TaskRow>| AgentPicker {
            select: Rc::new(RefCell::new(FilterableSelect::new(
                Vec::new(),
                SelectStyles::default(),
            ))),
            agents: vec![main_entry()],
            tasks,
            active: AgentId::Main,
            scope: Scope::Running,
            window: None,
            outcome: Rc::clone(&outcome),
            stack: Rc::clone(&stack),
            editor: Rc::clone(&editor),
        };
        let scope = default_action_shortcut(ACTION_AGENT_TOGGLE_SCOPE).unwrap();
        let kill = default_action_shortcut(ACTION_TASK_KILL).unwrap();

        let no_tasks = make(Vec::new());
        let sub = no_tasks.subtitle();
        assert!(sub.contains(&scope), "scope hint resolved from data: {sub}");
        assert!(!sub.contains(&kill), "no kill hint without tasks: {sub}");
        // The confirm/close labels track the keybinding data, not a literal.
        assert!(
            sub.contains(&confirm_key_label()),
            "confirm label resolved from data: {sub}"
        );
        assert!(
            sub.contains(&close_key_label()),
            "close label resolved from data: {sub}"
        );

        let with_task = make(vec![task_row(1, TaskStatus::Running, 1)]);
        assert!(
            with_task.subtitle().contains(&kill),
            "kill hint shown with a running task"
        );
    }

    /// Ctrl+K on a running task row parks a kill outcome. On a non-task
    /// row it is inert.
    #[test]
    fn ctrl_k_parks_kill_for_a_running_task_only() {
        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let editor: WidgetRef = Rc::new(RefCell::new(crate::overlay::Scrim));
        let outcome = Rc::new(RefCell::new(None));
        let tasks = vec![task_row(3, TaskStatus::Running, 1)];
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            build_items(&[main_entry()], &tasks, AgentId::Main, Scope::Running),
            SelectStyles::default(),
        )));
        let mut picker = AgentPicker {
            select: Rc::clone(&select),
            agents: vec![main_entry()],
            tasks,
            active: AgentId::Main,
            scope: Scope::Running,
            window: None,
            outcome: Rc::clone(&outcome),
            stack,
            editor,
        };

        let ctrl_k = Event::KeyPress(Key {
            codepoint: u32::from('k'),
            mods: Modifiers::CTRL,
            ..Key::default()
        });
        // On the pre-selected main row: consumed but inert.
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        picker.capture_event(&mut ctx, &ctrl_k);
        assert!(outcome.borrow().is_none(), "no kill on the main row");

        // Move onto the task row, then kill.
        select
            .borrow()
            .select_matching(|i| decode_task(&i.filter_key) == Some(3));
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        picker.capture_event(&mut ctx, &ctrl_k);
        assert_eq!(*outcome.borrow(), Some(AgentPickerOutcome::Kill(3)));
    }

    /// Ctrl+T flips the scope and rebuilds the rows: a finished sub that
    /// was hidden in the running scope appears after the toggle.
    #[test]
    fn ctrl_t_toggles_scope_and_rebuilds() {
        let stack = Rc::new(RefCell::new(OverlayStack::default()));
        let editor: WidgetRef = Rc::new(RefCell::new(crate::overlay::Scrim));
        let outcome = Rc::new(RefCell::new(None));
        let agents = vec![main_entry(), sub_entry(2, SubAgentStatus::Done)];
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            build_items(&agents, &[], AgentId::Main, Scope::Running),
            SelectStyles::default(),
        )));
        let mut picker = AgentPicker {
            select: Rc::clone(&select),
            agents,
            tasks: Vec::new(),
            active: AgentId::Main,
            scope: Scope::Running,
            window: None,
            outcome,
            stack,
            editor,
        };
        assert!(
            !select
                .borrow()
                .visible_labels()
                .iter()
                .any(|l| l.starts_with("agent 2"))
        );

        let ctrl_t = Event::KeyPress(Key {
            codepoint: u32::from('t'),
            mods: Modifiers::CTRL,
            ..Key::default()
        });
        let mut ctx = EventContext::new();
        ctx.phase = Phase::Capturing;
        picker.capture_event(&mut ctx, &ctrl_t);
        assert_eq!(picker.scope, Scope::All);
        assert!(
            select
                .borrow()
                .visible_labels()
                .iter()
                .any(|l| l.starts_with("agent 2")),
            "finished sub revealed after toggle"
        );
    }
}
