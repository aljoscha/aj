//! Event pump — translates each [`AgentEvent`] into a layout
//! mutation.
//!
//! The interactive mode subscribes to the agent's bus through
//! [`aj_agent::Agent::subscribe_channel`] and pulls events off the
//! receiver in its `tokio::select!` loop. For each event the
//! [`EventPump`] looks up (or creates) the matching component in
//! the chat / status slots and forwards the update.
//!
//! Sub-agents share the parent's bus and emit their events tagged
//! with [`AgentId::Sub`]. The pump routes each event to the owning
//! agent's transcript via the [`ChatView`] in `SlotIndex::Chat`: the
//! main agent's events land in the main transcript, a sub-agent's
//! events land inside its [`SubAgentBox`]. The box is created on
//! [`AgentEvent::SubAgentStart`] (which also drives the footer's
//! running-agent indicator) and finalized on
//! [`AgentEvent::SubAgentEnd`]. The parent's `agent` tool call is the
//! box's visual representation, so its `ToolExecution*` events are
//! skipped to avoid duplicating the report.
//!
//! See `docs/aj-next-plan.md` §1.1 (event protocol) and §4
//! (event-pump shape), plus `docs/subagent-observability-spec.md`.
//!
//! [`SubAgentBox`]: crate::modes::interactive::components::subagent_box::SubAgentBox

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::queue::MessageQueues;
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use aj_agent::types::TokenUsage;
use aj_models::registry::ModelInfo;
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantContent, Message, UserContent};
use aj_tui::components::editor::Editor;
use aj_tui::components::spacer::Spacer;
use aj_tui::components::text::Text;
use aj_tui::container::Container;
use aj_tui::tui::Tui;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::agent_picker::TaskPickerEntry;
use crate::modes::interactive::components::assistant_message::{
    AssistantMessageComponent, BlockKind,
};
use crate::modes::interactive::components::chat_view::{AgentEntry, ChatView};
use crate::modes::interactive::components::footer::{AgentActivity, Footer};
use crate::modes::interactive::components::loader_status::LoaderStatus;
use crate::modes::interactive::components::pending_message::PendingMessage;
use crate::modes::interactive::components::subagent_box::SubAgentStatus;
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;
use crate::modes::interactive::components::user_message::UserMessageComponent;
use crate::modes::interactive::footer_data::AgentFooters;
use crate::modes::interactive::layout::SlotIndex;
use crate::modes::interactive::render_settings::RenderSettings;

/// Per-agent streaming bookkeeping. The pump keeps one of these for
/// the main agent and one per sub-agent so streaming events route to
/// the right component inside that agent's own transcript container.
///
/// The indices are container-local: `current_assistant` and the
/// values in `tool_index` index into the agent's own [`Container`]
/// (the main transcript for [`AgentId::Main`], or a
/// [`SubAgentBox`](crate::modes::interactive::components::subagent_box::SubAgentBox)'s
/// inner container for a sub-agent). Each container only ever appends,
/// so recorded indices stay valid for the session.
#[derive(Default)]
struct AgentRender {
    /// Index, inside this agent's container, of the in-flight
    /// assistant message component. `None` between turns.
    current_assistant: Option<usize>,
    /// Map of `tool_use_id` → index inside this agent's container of
    /// the matching [`ToolExecutionComponent`].
    tool_index: HashMap<String, usize>,
}

/// One background task tracked from [`AgentEvent::TaskStart`] /
/// [`AgentEvent::TaskEnd`]. Drives the footer's task count, the
/// picker's task rows, and the routing of task events to the
/// launching tool call's transcript cell.
struct TaskInfo {
    kind: TaskKind,
    /// Display label — the command line for bash tasks, the task
    /// description for agent-backed ones.
    label: String,
    /// The agent that launched the task; its transcript holds the
    /// launch cell.
    owner: AgentId,
    /// `tool_use_id` of the originating tool call, correlating task
    /// events with the cell.
    call_id: String,
    status: TaskStatus,
    /// When the pump saw `TaskStart`, for the picker's runtime column.
    started_at: Instant,
    /// When the pump saw `TaskEnd`; freezes the displayed runtime.
    finished_at: Option<Instant>,
    /// Index of the launch cell inside the owner's container,
    /// snapshotted at `TaskStart` (containers only append, so it
    /// stays valid). The owner's `tool_index` is cleared on its
    /// `AgentEnd`, but a background task outlives the turn — this
    /// snapshot is what keeps `TaskOutput` / `TaskEnd` routable
    /// afterwards. `None` when the launching call has no cell (the
    /// `agent` tool renders as a sub-agent box, not a tool bubble).
    cell: Option<usize>,
}

/// Translates [`AgentEvent`]s into TUI mutations.
///
/// The pump owns no view state of its own — every component lives
/// inside the `Tui`'s slot tree. It tracks per-agent streaming
/// metadata ([`AgentRender`]) so streaming events reach the right
/// widget, and the in-flight-agent set that the per-view spinner,
/// the footer indicator, and per-box status all derive from.
pub struct EventPump {
    theme: ChatTheme,
    /// Per-agent streaming bookkeeping, keyed by [`AgentId`].
    agents: HashMap<AgentId, AgentRender>,
    /// The literal set of agents that have emitted
    /// [`AgentEvent::AgentStart`] without a matching
    /// [`AgentEvent::AgentEnd`] yet — the single source of truth
    /// for "what is running".
    ///
    /// `AgentStart` inserts; `AgentEnd` removes; the pump never
    /// special-cases [`AgentId::Main`]. Under concurrency a main
    /// turn can legitimately start or end while a sub-agent turn
    /// runs, so no agent's lifecycle may touch another's entry.
    ///
    /// Three things derive from this set:
    /// - the per-view spinner ([`Self::sync_loader`]) — active iff
    ///   the *viewed* agent is in the set, so a leaked entry can
    ///   never pin the spinner of an idle view;
    /// - the aggregate footer indicator
    ///   ([`Self::sync_agent_indicator`]) — the count of running
    ///   `Sub(_)` ids, so background activity stays visible while
    ///   viewing an idle agent;
    /// - per-box status (`Running` on `AgentStart(Sub n)`, `Done`
    ///   on `AgentEnd(Sub n)`), which is what flips a re-prompted
    ///   box back through `Running`→`Done`.
    ///
    /// Leak draining (a dropped `AgentEnd(Sub n)` from a cancelled
    /// initial spawn) is the *binary's* responsibility on
    /// main-turn completion via [`Self::mark_idle`]: only the
    /// binary knows which running subs are independent
    /// continuations versus nested initial spawns. The pump keeps
    /// this set as literal truth.
    running_agents: HashSet<AgentId>,
    /// Background tasks observed via [`AgentEvent::TaskStart`],
    /// keyed by task id. Entries are kept (with their terminal
    /// status) after `TaskEnd` so the picker's "all" scope can list
    /// finished tasks; the footer counts only running
    /// [`TaskKind::Bash`] entries. Task events are transient, so a
    /// resumed session starts with an empty map.
    tasks: BTreeMap<TaskId, TaskInfo>,
    /// Shared session-wide render settings (tool expansion,
    /// thinking-block fold, inline-image rendering). Cloned into
    /// every assistant / tool component the pump creates so they
    /// observe the current values; a runtime toggle (via
    /// [`Self::set_hide_thinking_block`] / [`Self::set_tools_expanded`])
    /// bumps the shared generation and the components reconcile on
    /// their next render, so the pump never walks the transcript.
    render_settings: RenderSettings,
    /// Per-agent store feeding the [`Footer`]'s model line and
    /// context-usage indicator. The footer is view-scoped: after
    /// every mutation the pump pushes the *active view's* entry
    /// into the footer, so switching views (or a settings change
    /// on the viewed agent) repaints immediately.
    footer_data: AgentFooters,
    /// Model catalog used to resolve a `(provider, model_id)`
    /// settings identity to its context window (see
    /// [`Self::resolve_window`]).
    catalog: Arc<Vec<ModelInfo>>,
    /// Shared steering / follow-up queues. The pump reads the active
    /// view's snapshot to paint the pending-message box; it never
    /// mutates them (the TUI input handlers and the agent do). See
    /// [`Self::sync_pending`].
    message_queues: MessageQueues,
}

impl EventPump {
    /// Build a fresh pump bound to the supplied [`ChatTheme`]
    /// (used when constructing assistant / user message
    /// components on the fly). `render_settings` is the shared
    /// session-wide render config (thinking-block fold, tool-output
    /// expansion, inline-image rendering); the host seeds it from
    /// `~/.aj/config.toml` on startup and the components clone the
    /// handle so a runtime toggle reaches them on their next render.
    ///
    /// `main_settings` and `main_context_window` seed the Main
    /// agent's footer entry (model line + context-usage
    /// denominator); later changes flow through
    /// [`Self::note_agent_settings`]. `catalog` is the model
    /// catalog used to resolve sub-agent context windows from
    /// their settings identity.
    pub fn new(
        theme: ChatTheme,
        render_settings: RenderSettings,
        main_settings: AgentSettings,
        main_context_window: u64,
        catalog: Arc<Vec<ModelInfo>>,
        message_queues: MessageQueues,
    ) -> Self {
        Self {
            theme,
            agents: HashMap::new(),
            running_agents: HashSet::new(),
            tasks: BTreeMap::new(),
            render_settings,
            footer_data: AgentFooters::new(main_settings, main_context_window),
            catalog,
            message_queues,
        }
    }

    /// Push the active view's footer state — model line and
    /// context usage — into the [`Footer`] component. Called after
    /// every mutation that affects the rendered state so the row
    /// stays in sync with the viewed agent.
    pub fn sync_footer(&self, tui: &mut Tui) {
        let active = self.active_view(tui);
        let model_line = self.footer_data.model_line(active);
        let usage = self.footer_data.context_usage(active);
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            footer.set_model(model_line);
            footer.set_context_usage(Some(usage));
        }
        tui.request_render();
    }

    /// Paint the pending-message box for the active view from the
    /// shared queue handle. Called after every queue change (the
    /// agent's `QueueUpdate`, a view switch, and the TUI input
    /// handlers after they enqueue), so the box always reflects the
    /// viewed agent's live snapshot rather than any event payload.
    pub fn sync_pending(&self, tui: &mut Tui) {
        let active = self.active_view(tui);
        let snapshot = self.message_queues.snapshot(active);
        if let Some(pending) = tui.get_mut_as::<PendingMessage>(SlotIndex::Pending.idx()) {
            pending.set_snapshot(snapshot);
        }
        tui.request_render();
    }

    /// Shows `N agents, M tasks (key)` while at least one sub-agent
    /// or background bash task runs, where `key` is the resolved
    /// `aj.agent.open` shortcut; clears the indicator when neither.
    /// The agent count is derived from `running_agents` (every
    /// running `Sub(_)` id) and the task count from the running
    /// [`TaskKind::Bash`] entries — agent-backed tasks are excluded
    /// because their sub-agent is already in the agent count.
    fn sync_agent_indicator(&self, tui: &mut Tui) {
        let agents = self
            .running_agents
            .iter()
            .filter(|a| matches!(a, AgentId::Sub(_)))
            .count();
        let tasks = self
            .tasks
            .values()
            .filter(|t| matches!(t.kind, TaskKind::Bash { .. }) && t.status == TaskStatus::Running)
            .count();
        let activity = (agents + tasks > 0).then(|| AgentActivity {
            agents,
            tasks,
            open_hint: agent_picker_hint(),
        });
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            footer.set_agent_activity(activity);
        }
        tui.request_render();
    }

    /// Record `id`'s next-turn settings (and context-window
    /// denominator) and refresh the footer. The sync is
    /// unconditional — it renders from the active view and is
    /// idempotent, so a change to a non-viewed agent simply
    /// repaints the viewed agent's unchanged state.
    pub fn note_agent_settings(
        &mut self,
        tui: &mut Tui,
        id: AgentId,
        settings: AgentSettings,
        context_window: u64,
    ) {
        self.footer_data.note_settings(id, settings, context_window);
        self.sync_footer(tui);
    }

    /// Replace `id`'s footer entry with a settings identity known
    /// only as strings, resolving the context window internally
    /// (catalog scan, Main-identity fallback, else 0). Exists for
    /// resume-time reconciliation, where the caller folds settings
    /// out of the session log and holds no `ModelInfo`.
    pub fn reconcile_agent_settings(
        &mut self,
        tui: &mut Tui,
        id: AgentId,
        settings: AgentSettings,
    ) {
        let window = self.resolve_window(&settings);
        self.note_agent_settings(tui, id, settings, window);
    }

    /// Read back the stored settings snapshot for `id`. `None`
    /// when the agent has no entry.
    pub fn agent_settings(&self, id: AgentId) -> Option<&AgentSettings> {
        self.footer_data.settings(id)
    }

    /// Context-window denominator currently stored for `id` (falls
    /// back to the Main entry like the footer's usage view; `0`
    /// means unknown). Lets settings stagers replace one axis of an
    /// agent's footer entry while preserving its window.
    pub fn agent_context_window(&self, id: AgentId) -> u64 {
        self.footer_data.context_usage(id).context_window
    }

    /// Resolve the context window for a settings identity known
    /// only as `(provider, model_id)` strings:
    ///
    /// 1. Catalog scan — the catalog is the authoritative source
    ///    and is loaded once at startup.
    /// 2. On a miss, an identity equal to the Main entry's
    ///    settings (provider and model_id) resolves to Main's
    ///    window. This covers scripted runs and `--model-url`
    ///    bundles absent from the catalog: sub-agents inherit the
    ///    parent's bundle, so the identity match is exact in
    ///    practice.
    /// 3. Otherwise `0`, which suppresses the indicator.
    fn resolve_window(&self, settings: &AgentSettings) -> u64 {
        if let Some(info) = self
            .catalog
            .iter()
            .find(|m| m.provider == settings.provider && m.id == settings.model_id)
        {
            return info.context_window;
        }
        if let Some(main) = self.footer_data.settings(AgentId::Main)
            && main.provider == settings.provider
            && main.model_id == settings.model_id
        {
            return self.footer_data.context_usage(AgentId::Main).context_window;
        }
        0
    }

    /// Current thinking-block render mode. Exposed so the host's
    /// `aj.thinking.toggle` handler can flip the state without first reading
    /// it back through a separate getter.
    pub fn hide_thinking_block(&self) -> bool {
        self.render_settings.hide_thinking_block()
    }

    /// Current inline-image render mode. Surface so session-swap /
    /// new-session paths can preserve the user's
    /// `image_show_in_terminal` choice across pump re-creation.
    pub fn show_image_in_terminal(&self) -> bool {
        self.render_settings.show_image_in_terminal()
    }

    /// Update the thinking-block render mode for this session.
    ///
    /// Flips the shared render setting, which bumps its generation;
    /// every assistant message component reconciles and rebuilds its
    /// thinking widgets on its next render. Invalidates the TUI's
    /// cached render output so that next paint actually happens.
    pub fn set_hide_thinking_block(&mut self, tui: &mut Tui, hide: bool) {
        self.render_settings.set_hide_thinking_block(hide);
        tui.invalidate();
        tui.request_render();
    }

    /// Current tool-output render mode. Exposed so the host's
    /// `aj.tools.expand` handler can flip the state without first
    /// reading it back through a separate getter.
    pub fn tools_expanded(&self) -> bool {
        self.render_settings.tools_expanded()
    }

    /// Update the tool-output render mode for this session.
    ///
    /// Flips the shared render setting, which bumps its generation;
    /// every tool component reconciles and rebuilds its body on its
    /// next render. Invalidates the TUI's cached render output so
    /// that next paint actually happens.
    pub fn set_tools_expanded(&mut self, tui: &mut Tui, expanded: bool) {
        self.render_settings.set_tools_expanded(expanded);
        tui.invalidate();
        tui.request_render();
    }

    /// Snapshot of every known agent (main first, then sub-agents)
    /// for the agent picker. Reads through the [`ChatView`]; empty
    /// when the chat slot is somehow absent.
    pub fn agents(&self, tui: &mut Tui) -> Vec<AgentEntry> {
        tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .map(|c| c.agents())
            .unwrap_or_default()
    }

    /// Snapshot of the tracked background bash tasks (in id order)
    /// for the agent picker. Agent-backed tasks are skipped: their
    /// sub-agent already appears as an agent entry, so a task row
    /// would duplicate it.
    pub fn tasks(&self) -> Vec<TaskPickerEntry> {
        self.tasks
            .iter()
            .filter(|(_, t)| matches!(t.kind, TaskKind::Bash { .. }))
            .map(|(&id, t)| TaskPickerEntry {
                id,
                label: t.label.clone(),
                status: t.status,
                runtime: t
                    .finished_at
                    .unwrap_or_else(Instant::now)
                    .duration_since(t.started_at),
            })
            .collect()
    }

    /// The agent that launched task `id`, for the picker's
    /// jump-to-task action. `None` for unknown ids.
    pub fn task_owner(&self, id: TaskId) -> Option<AgentId> {
        self.tasks.get(&id).map(|t| t.owner)
    }

    /// The agent whose transcript is currently the main view.
    pub fn active_view(&self, tui: &mut Tui) -> AgentId {
        tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .map(|c| c.active())
            .unwrap_or(AgentId::Main)
    }

    /// Switch the chat view to `id`'s transcript. Invalidates and
    /// requests a render because the visible chat region changes
    /// wholesale (the diff engine needs a full repaint).
    pub fn set_active_view(&mut self, tui: &mut Tui, id: AgentId) {
        if let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) {
            chat.set_active(id);
        }
        // The spinner is scoped to the viewed agent, so a view
        // switch must immediately reflect the new agent's running
        // state. The footer is view-scoped too: repaint it with the
        // new view's model line and context usage.
        self.sync_loader(tui);
        self.sync_footer(tui);
        self.sync_pending(tui);
        tui.invalidate();
        tui.request_render();
    }

    /// Whether `id` is currently running (membership in the
    /// authoritative `running_agents` set).
    pub fn is_running(&self, id: AgentId) -> bool {
        self.running_agents.contains(&id)
    }

    /// Snapshot of every agent currently in the running set. The
    /// binary iterates this to reconcile leaked nested sub-agents
    /// on main-turn completion; order is unspecified.
    pub fn running_agents(&self) -> Vec<AgentId> {
        self.running_agents.iter().copied().collect()
    }

    /// Force `id` out of the running set, reconciling everything
    /// derived from it (per-view spinner, footer count, box
    /// status). Idempotent w.r.t. an `AgentEnd` the pump already
    /// processed. The binary calls this on main-turn completion to
    /// drain a leaked sub-agent whose `AgentEnd` never arrived.
    pub fn mark_idle(&mut self, tui: &mut Tui, id: AgentId) {
        self.running_agents.remove(&id);
        if let AgentId::Sub(n) = id
            && let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            && let Some(b) = chat.sub_box_mut(n)
        {
            b.set_status(SubAgentStatus::Done);
        }
        self.sync_loader(tui);
        self.sync_agent_indicator(tui);
    }

    /// Dispatch one [`AgentEvent`] onto `tui`'s slot tree. Returns
    /// nothing — every effect is a side effect on the layout.
    /// Callers that want a render afterwards should call
    /// [`Tui::request_render`] (the pump itself does so for the
    /// events that mutate visible state).
    pub fn handle(&mut self, tui: &mut Tui, event: &AgentEvent) {
        match event {
            // ---- Lifecycle: drive the per-view spinner. ----
            //
            // `running_agents` is the literal set of in-flight
            // agents: `AgentStart` inserts, `AgentEnd` removes, no
            // agent's lifecycle touches another's entry. The single
            // status-slot spinner is scoped to the *viewed* agent
            // ([`Self::sync_loader`]), so a sub-agent's lifecycle
            // can't toggle the spinner of an unrelated view.
            AgentEvent::AgentStart { agent_id } => {
                self.running_agents.insert(*agent_id);
                // A continuation re-prompt emits no `SubAgentStart`,
                // so `AgentStart(Sub n)` is what flips a re-prompted
                // box back to `Running`. Defensive: skip when the
                // box doesn't exist yet.
                if let AgentId::Sub(n) = agent_id
                    && let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                    && let Some(b) = chat.sub_box_mut(*n)
                {
                    b.set_status(SubAgentStatus::Running);
                }
                self.sync_loader(tui);
                self.sync_agent_indicator(tui);
            }
            AgentEvent::AgentEnd { agent_id, .. } => {
                self.running_agents.remove(agent_id);
                // Each agent owns its streaming bookkeeping, so an
                // agent's end clears only its own entry; the main
                // agent's pending `agent` tool call (whose body is a
                // sub-agent run) is unaffected.
                if let Some(state) = self.agents.get_mut(agent_id) {
                    state.current_assistant = None;
                    state.tool_index.clear();
                }
                if let AgentId::Sub(n) = agent_id
                    && let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                    && let Some(b) = chat.sub_box_mut(*n)
                {
                    b.set_status(SubAgentStatus::Done);
                }
                self.sync_loader(tui);
                self.sync_agent_indicator(tui);
            }
            AgentEvent::TurnStart { agent_id } => {
                // Each new turn starts with a fresh assistant
                // message component for that agent; the previous
                // turn's component (if any) was already finalized.
                if let Some(state) = self.agents.get_mut(agent_id) {
                    state.current_assistant = None;
                }
            }

            // ---- Streaming: unified message lifecycle. ----
            //
            // The agent emits a `MessageStart` / `MessageEnd` pair
            // around every message (user, assistant, tool-result)
            // and, for assistant streaming, one `MessageUpdate` per
            // provider [`AssistantMessageEvent`]. Each event carries
            // the emitting `agent_id` so the pump routes it to that
            // agent's transcript container.
            AgentEvent::MessageStart { agent_id, message } => {
                self.handle_message_start(tui, *agent_id, message);
            }
            AgentEvent::MessageUpdate {
                agent_id, event, ..
            } => {
                self.handle_message_update(tui, *agent_id, event);
            }
            AgentEvent::MessageEnd { agent_id, message } => {
                self.handle_message_end(tui, *agent_id, message);
            }

            // ---- Tool execution: header + result. ----
            //
            // The parent's `agent` tool call is represented by the
            // sub-agent box, not a tool bubble, so its events are
            // skipped to avoid duplicating the report.
            AgentEvent::ToolExecutionStart {
                agent_id,
                call_id,
                tool,
                args,
            } => {
                if tool != "agent" {
                    self.append_tool_execution(tui, *agent_id, call_id, tool, args);
                }
            }
            AgentEvent::ToolExecutionUpdate {
                agent_id,
                tool,
                call_id,
                partial,
                content,
                ..
            } => {
                if tool != "agent" {
                    self.update_tool_execution_partial(tui, *agent_id, call_id, partial, content);
                }
            }
            AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id,
                tool,
                result,
                content,
                is_error,
            } => {
                if tool != "agent" {
                    self.update_tool_execution_result(
                        tui, *agent_id, call_id, tool, result, content, *is_error,
                    );
                }
            }

            // ---- Notices / warnings / errors. ----
            AgentEvent::Notice { agent_id, text } => {
                self.append_notice(tui, *agent_id, text);
            }
            AgentEvent::Warning { agent_id, text } => {
                self.append_styled_notice(tui, *agent_id, text, aj_tui::style::yellow);
            }
            AgentEvent::Error { agent_id, text } => {
                self.append_styled_notice(tui, *agent_id, text, aj_tui::style::red);
            }
            AgentEvent::StreamRetry {
                agent_id,
                attempt,
                delay,
                error,
            } => {
                let msg = format!(
                    "Retrying inference (attempt {attempt}, in {}ms): {error}",
                    delay.as_millis()
                );
                self.append_styled_notice(tui, *agent_id, &msg, aj_tui::style::yellow);
            }

            // ---- Per-turn token usage. ----
            AgentEvent::TurnUsage { agent_id, usage } => {
                self.append_turn_usage(tui, *agent_id, usage);
                // Every agent's usage folds into its own footer
                // entry; the footer itself tracks the viewed agent,
                // so a sub's usage moves it only when that sub's
                // view is active.
                self.footer_data.record_turn_usage(*agent_id, usage);
                if *agent_id == self.active_view(tui) {
                    self.sync_footer(tui);
                }
            }

            // ---- Compaction lifecycle. ----
            AgentEvent::CompactionStart { agent_id, .. } => {
                self.append_notice(tui, *agent_id, "Compacting context…");
                tui.request_render();
            }
            AgentEvent::CompactionEnd {
                agent_id,
                tokens_before,
                tokens_after,
                error,
                ..
            } => {
                if let Some(err) = error {
                    self.append_styled_notice(
                        tui,
                        *agent_id,
                        &format!("Compaction failed: {err}"),
                        aj_tui::style::yellow,
                    );
                } else {
                    self.append_notice(
                        tui,
                        *agent_id,
                        &format!("Context compacted: ~{tokens_before} → ~{tokens_after} tokens."),
                    );
                }
                tui.request_render();
            }

            // ---- Sub-agent boxes. ----
            AgentEvent::SubAgentStart {
                child,
                task,
                settings,
                ..
            } => {
                if let AgentId::Sub(n) = child
                    && let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                {
                    // Create the box (initial `Running`) + the
                    // persistence anchor. The footer count and the
                    // box's running status come from the paired
                    // `AgentStart(Sub n)`, not from here.
                    chat.ensure_sub_box(*n, task);
                }
                // Seed the child's footer entry with its spawn-time
                // settings so its view shows a model line and (when
                // resolvable) a context window.
                let window = self.resolve_window(settings);
                self.footer_data
                    .note_settings(*child, settings.clone(), window);
            }
            AgentEvent::SubAgentEnd { child, .. } => {
                if let AgentId::Sub(n) = child
                    && let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                    && let Some(b) = chat.sub_box_mut(*n)
                {
                    b.set_status(SubAgentStatus::Done);
                }
            }

            // ---- Background tasks. ----
            //
            // Transient events: persistence ignores them and replay
            // never synthesizes them, so everything here is
            // live-session-only state. A resumed transcript shows the
            // persisted launch cell (with its task-id badge from the
            // `ToolDetails::Bash` payload) and notices, while this
            // map starts empty — nothing below may assume an entry
            // exists for a cell that carries a badge.
            AgentEvent::TaskStart {
                agent_id,
                task_id,
                call_id,
                kind,
                label,
            } => {
                // Snapshot the launch cell's index now: the owner's
                // `tool_index` is wiped on its `AgentEnd`, and task
                // events keep arriving after the turn.
                let cell = self
                    .agents
                    .get(agent_id)
                    .and_then(|a| a.tool_index.get(call_id))
                    .copied();
                self.tasks.insert(
                    *task_id,
                    TaskInfo {
                        kind: kind.clone(),
                        label: label.clone(),
                        owner: *agent_id,
                        call_id: call_id.clone(),
                        status: TaskStatus::Running,
                        started_at: Instant::now(),
                        finished_at: None,
                        cell,
                    },
                );
                self.sync_agent_indicator(tui);
            }
            AgentEvent::TaskOutput {
                task_id, partial, ..
            } => {
                if let Some((owner, cell)) = self.task_cell(*task_id) {
                    self.update_task_cell(tui, owner, cell, |c| c.update_partial(partial, &[]));
                }
            }
            AgentEvent::TaskEnd {
                task_id, status, ..
            } => {
                if let Some(info) = self.tasks.get_mut(task_id) {
                    info.status = *status;
                    info.finished_at = Some(Instant::now());
                }
                if let Some((owner, cell)) = self.task_cell(*task_id) {
                    self.update_task_cell(tui, owner, cell, |c| c.finish_task(*status));
                }
                self.sync_agent_indicator(tui);
            }

            // ---- Placeholders: events whose UI work isn't yet wired. ----
            AgentEvent::QueueUpdate { agent_id, .. } => {
                // The agent emits this after draining a queue. Repaint
                // the box only when the change is for the viewed agent;
                // we re-read the live snapshot rather than trust the
                // payload (see `aj_agent::queue`), which keeps
                // the box correct even if a TUI enqueue raced the drain.
                if *agent_id == self.active_view(tui) {
                    self.sync_pending(tui);
                }
            }
            AgentEvent::TurnEnd { .. } => {
                // The `TurnEnd` summary lands in a follow-up commit.
                // Holding the arm here keeps the exhaustiveness check
                // active so a newly-emitted event variant shows up as a
                // compile error.
            }
        }

        tui.request_render();
    }

    // ---- Helpers ---------------------------------------------------------

    /// Drive the status-slot loader to reflect the *viewed* agent's
    /// activity: active iff the active view's agent is in
    /// `running_agents`. Only toggles on a genuine edge —
    /// `Loader::start` resets the frame clock, so calling it on
    /// every event would jitter the animation.
    fn sync_loader(&self, tui: &mut Tui) {
        let active = self.active_view(tui);
        let should_run = self.running_agents.contains(&active);
        self.with_loader(tui, |l| match (should_run, l.is_active()) {
            (true, false) => l.start(),
            (false, true) => l.stop(),
            _ => {}
        });
    }

    /// Mutate the [`LoaderStatus`] component in the status slot.
    /// Centralised so callers don't repeat the slot/container
    /// nesting; cheap because it's a couple of downcasts.
    fn with_loader<F: FnOnce(&mut LoaderStatus)>(&self, tui: &mut Tui, f: F) {
        let Some(status) = tui.get_mut_as::<Container>(SlotIndex::Status.idx()) else {
            return;
        };
        let Some(loader) = status.get_mut_as::<LoaderStatus>(0) else {
            return;
        };
        f(loader);
    }

    /// Append a `UserMessageComponent` for a user message that
    /// landed on the bus, into `agent_id`'s transcript. Used by the
    /// live readline path (via the agent's `MessageEnd { User }`
    /// event) and the resume path (via the same event synthesized
    /// by `aj_session::replay`). Multiple [`UserContent::Text`]
    /// blocks are joined with `\n` so legacy multi-block user
    /// messages collapse into one rendered component.
    fn append_user_message(&self, tui: &mut Tui, agent_id: AgentId, content: &[UserContent]) {
        let text = content
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        let component = UserMessageComponent::new(&text, &self.theme);
        self.push_chat_child(tui, agent_id, Box::new(component));
    }

    /// Handle [`AgentEvent::MessageStart`]. A no-op on the TUI
    /// surface: the authoritative payload lands on the matching
    /// [`AgentEvent::MessageEnd`] (user / tool-result) or the
    /// assistant slot is materialised lazily by the first painting
    /// [`AgentEvent::MessageUpdate`] / by [`Self::handle_message_end`]
    /// on the replay path. Retained (rather than inlined) so the
    /// rationale has a home and future Start-time work has an
    /// obvious hook.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn handle_message_start(
        &mut self,
        _tui: &mut Tui,
        _agent_id: AgentId,
        _message: &AgentMessage,
    ) {
    }

    /// Handle [`AgentEvent::MessageUpdate`] for an assistant
    /// streaming inference. Drives the in-flight
    /// [`AssistantMessageComponent`] in `agent_id`'s transcript off
    /// the embedded [`AssistantMessageEvent`].
    fn handle_message_update(
        &mut self,
        tui: &mut Tui,
        agent_id: AgentId,
        event: &AssistantMessageEvent,
    ) {
        // Early-out for events that don't paint into the assistant
        // component, BEFORE materialising the slot. Tool calls render
        // through the dedicated `ToolExecution*` events; `Start` /
        // `Done` / `Error` are agent-side lifecycle markers whose
        // bookends are the matching `MessageStart` / `MessageEnd`.
        // Returning here keeps tool-use-only turns from materialising
        // an empty assistant slot whose leading auto-spacer would
        // orphan into the next row's gap.
        if !matches!(
            event,
            AssistantMessageEvent::TextStart { .. }
                | AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::TextEnd { .. }
                | AssistantMessageEvent::ThinkingStart { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ThinkingEnd { .. }
        ) {
            return;
        }

        let idx = self.ensure_assistant_message(tui, agent_id);
        let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(container) = chat.agent_container_mut(agent_id) else {
            return;
        };
        let Some(c) = container.get_mut_as::<AssistantMessageComponent>(idx) else {
            return;
        };
        match event {
            AssistantMessageEvent::TextStart { .. } => {
                c.open_block(BlockKind::Text, String::new());
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                c.append_delta(BlockKind::Text, delta);
            }
            AssistantMessageEvent::TextEnd { content, .. } => {
                // The text block's canonical final bytes; pass
                // through so the block matches the model's
                // authoritative content even if individual deltas
                // got dropped.
                c.close_block(BlockKind::Text, Some(content.clone()));
            }
            AssistantMessageEvent::ThinkingStart { .. } => {
                c.open_block(BlockKind::Thinking, String::new());
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                c.append_delta(BlockKind::Thinking, delta);
            }
            AssistantMessageEvent::ThinkingEnd { content, .. } => {
                let payload = if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                };
                c.close_block(BlockKind::Thinking, payload);
            }
            // All non-painting variants returned early above.
            AssistantMessageEvent::ToolCallStart { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
            | AssistantMessageEvent::ToolCallEnd { .. }
            | AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {
                unreachable!("non-painting AssistantMessageEvent variants are filtered above")
            }
        }
    }

    /// Handle [`AgentEvent::MessageEnd`]. Assistant messages
    /// finalize `agent_id`'s in-flight component (next turn opens a
    /// fresh one); user messages append a fresh component from the
    /// authoritative payload (the rendering path for both live user
    /// prompts and replayed user threads). Tool-result messages are
    /// structural framing — they render through `ToolExecutionEnd`.
    fn handle_message_end(&mut self, tui: &mut Tui, agent_id: AgentId, message: &AgentMessage) {
        match &message.kind {
            AgentMessageKind::Wire(Message::User(u)) => {
                self.append_user_message(tui, agent_id, &u.content);
            }
            AgentMessageKind::Wire(Message::Assistant(a)) => {
                // Two cases share this arm:
                //
                // 1. Live streaming already painted the content
                //    through `handle_message_update`; the finalized
                //    event just unbinds the streaming target.
                //
                // 2. Replay emits `MessageStart` + `MessageEnd` with
                //    no `MessageUpdate` in between, so the slot was
                //    never materialised. We create it here and
                //    synthesize per-block open/close pairs from the
                //    finalized content.
                //
                // The slot is only materialised when the payload
                // carries at least one Text / Thinking block;
                // tool-use-only turns render entirely through the
                // [`ToolExecutionComponent`], and an empty assistant
                // slot's leading auto-spacer would double the gap to
                // the next row.
                let has_renderable = a.content.iter().any(|b| {
                    matches!(b, AssistantContent::Text(_) | AssistantContent::Thinking(_))
                });
                if has_renderable {
                    let idx = self.ensure_assistant_message(tui, agent_id);
                    if let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                        && let Some(container) = chat.agent_container_mut(agent_id)
                        && let Some(c) = container.get_mut_as::<AssistantMessageComponent>(idx)
                        && c.is_empty()
                    {
                        // Replay synthesis. Live streaming has
                        // already populated the blocks, so this
                        // branch is a no-op when `c.is_empty()` is
                        // false.
                        for block in &a.content {
                            match block {
                                AssistantContent::Thinking(t) => {
                                    c.open_block(BlockKind::Thinking, String::new());
                                    c.close_block(
                                        BlockKind::Thinking,
                                        Some(if t.redacted {
                                            format!("[Redacted thinking: {}]", t.thinking)
                                        } else {
                                            t.thinking.clone()
                                        }),
                                    );
                                }
                                AssistantContent::Text(t) => {
                                    c.open_block(BlockKind::Text, String::new());
                                    c.close_block(BlockKind::Text, Some(t.text.clone()));
                                }
                                AssistantContent::ToolCall(_) => {
                                    // Tool calls surface as
                                    // ToolExecutionStart/End in
                                    // replay; nothing to render here.
                                }
                            }
                        }
                    }
                }
                if let Some(state) = self.agents.get_mut(&agent_id) {
                    state.current_assistant = None;
                }
            }
            AgentMessageKind::Wire(Message::ToolResult(_)) => {
                // Tool results render through the dedicated
                // `ToolExecutionEnd` event (which carries the
                // structured `ToolDetails`). The unified
                // `MessageEnd { ToolResult }` is structural framing.
            }
        }
    }

    /// Ensure `agent_id`'s transcript tail is an
    /// [`AssistantMessageComponent`]. Returns its container index,
    /// creating (and remembering) a new component when the current
    /// turn doesn't have one yet.
    fn ensure_assistant_message(&mut self, tui: &mut Tui, agent_id: AgentId) -> usize {
        if let Some(idx) = self.agents.get(&agent_id).and_then(|a| a.current_assistant) {
            return idx;
        }
        let component = AssistantMessageComponent::new(&self.theme, self.render_settings.clone());
        let idx = self.push_chat_child(tui, agent_id, Box::new(component));
        self.agents.entry(agent_id).or_default().current_assistant = Some(idx);
        idx
    }

    /// Append a tool-execution component for a freshly-started tool
    /// call into `agent_id`'s transcript. Records the index in that
    /// agent's `tool_index` so subsequent `ToolExecutionUpdate` /
    /// `ToolExecutionEnd` events find it. Sub-agent tools render
    /// header-only because they live inside the sub-agent box's
    /// painted background; the box flips them to full bodies when
    /// the user switches to observe it.
    fn append_tool_execution(
        &mut self,
        tui: &mut Tui,
        agent_id: AgentId,
        call_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) {
        let cell_pixel_size = tui.terminal().cell_pixel_size();
        let mut component = ToolExecutionComponent::with_cell_pixel_size(
            tool.to_string(),
            args,
            &self.theme,
            self.render_settings.clone(),
            cell_pixel_size,
        );
        // Sub-agent tools render header-only inside the compact box;
        // when the user is observing this sub-agent (its box is the
        // active full view) they show full bodies like a main tool.
        if matches!(agent_id, AgentId::Sub(_)) {
            let observing = tui
                .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .is_some_and(|c| c.active() == agent_id);
            component.set_header_only(!observing);
        }
        let idx = self.push_chat_child(tui, agent_id, Box::new(component));
        let state = self.agents.entry(agent_id).or_default();
        state.tool_index.insert(call_id.to_string(), idx);
        // A tool call that arrives mid-turn means the assistant
        // message that emitted it is finished as far as the stream
        // is concerned. Drop the streaming target so the next
        // assistant turn opens a fresh component.
        state.current_assistant = None;
    }

    /// Update an in-flight tool's body with a partial snapshot.
    fn update_tool_execution_partial(
        &self,
        tui: &mut Tui,
        agent_id: AgentId,
        call_id: &str,
        partial: &aj_agent::tool::ToolDetails,
        content: &[aj_models::types::UserContent],
    ) {
        let Some(&idx) = self
            .agents
            .get(&agent_id)
            .and_then(|a| a.tool_index.get(call_id))
        else {
            return;
        };
        let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(container) = chat.agent_container_mut(agent_id) else {
            return;
        };
        let Some(c) = container.get_mut_as::<ToolExecutionComponent>(idx) else {
            return;
        };
        c.update_partial(partial, content);
    }

    /// Finalize a tool execution with its result.
    fn update_tool_execution_result(
        &mut self,
        tui: &mut Tui,
        agent_id: AgentId,
        call_id: &str,
        tool: &str,
        result: &aj_agent::tool::ToolDetails,
        content: &[aj_models::types::UserContent],
        is_error: bool,
    ) {
        // If we never saw `ToolExecutionStart` (replay path), build a
        // component now so the result is visible. Args aren't
        // available on the End event, so we render with an empty
        // object. The build-on-miss branch must replicate the live
        // path's bookkeeping (clear `current_assistant`) so a
        // subsequent assistant text chunk opens a fresh component
        // *after* the tool rather than reusing a pre-tool one.
        let idx = match self
            .agents
            .get(&agent_id)
            .and_then(|a| a.tool_index.get(call_id))
        {
            Some(idx) => *idx,
            None => {
                let cell_pixel_size = tui.terminal().cell_pixel_size();
                let mut component = ToolExecutionComponent::with_cell_pixel_size(
                    tool.to_string(),
                    &serde_json::json!({}),
                    &self.theme,
                    self.render_settings.clone(),
                    cell_pixel_size,
                );
                if matches!(agent_id, AgentId::Sub(_)) {
                    let observing = tui
                        .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                        .is_some_and(|c| c.active() == agent_id);
                    component.set_header_only(!observing);
                }
                let idx = self.push_chat_child(tui, agent_id, Box::new(component));
                let state = self.agents.entry(agent_id).or_default();
                state.tool_index.insert(call_id.to_string(), idx);
                state.current_assistant = None;
                idx
            }
        };
        let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(container) = chat.agent_container_mut(agent_id) else {
            return;
        };
        let Some(c) = container.get_mut_as::<ToolExecutionComponent>(idx) else {
            return;
        };
        c.update_result(result, content, is_error);
    }

    /// Resolve task `id`'s launch cell: the owner plus the cell's
    /// index in the owner's container. Prefers the live `tool_index`
    /// (by `call_id`) and falls back to the index snapshotted at
    /// `TaskStart`, which is what survives the owner's `AgentEnd`
    /// clearing its tool bookkeeping.
    fn task_cell(&self, id: TaskId) -> Option<(AgentId, usize)> {
        let info = self.tasks.get(&id)?;
        let cell = self
            .agents
            .get(&info.owner)
            .and_then(|a| a.tool_index.get(&info.call_id))
            .copied()
            .or(info.cell)?;
        Some((info.owner, cell))
    }

    /// Apply `f` to the [`ToolExecutionComponent`] at `cell` inside
    /// `owner`'s transcript container, if present.
    fn update_task_cell<F: FnOnce(&mut ToolExecutionComponent)>(
        &self,
        tui: &mut Tui,
        owner: AgentId,
        cell: usize,
        f: F,
    ) {
        let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(container) = chat.agent_container_mut(owner) else {
            return;
        };
        let Some(c) = container.get_mut_as::<ToolExecutionComponent>(cell) else {
            return;
        };
        f(c);
    }

    /// Append a plain dim-styled notice line into `agent_id`'s
    /// transcript. The auto-spacer inserted by
    /// [`Self::push_chat_child`] handles separation from neighbours,
    /// so the text component itself uses `padding_y = 0`.
    fn append_notice(&self, tui: &mut Tui, agent_id: AgentId, text: &str) {
        let styled = aj_tui::style::dim(text);
        self.push_chat_child(tui, agent_id, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append a styled notice using the supplied colour function
    /// (yellow for warnings, red for errors) into `agent_id`'s
    /// transcript. Mirrors [`Self::append_notice`]'s zero internal
    /// padding; the surrounding auto-spacer provides the gap.
    fn append_styled_notice(
        &self,
        tui: &mut Tui,
        agent_id: AgentId,
        text: &str,
        style: fn(&str) -> String,
    ) {
        let styled = style(text);
        self.push_chat_child(tui, agent_id, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append a dim `Token Usage - ...` row for a freshly-completed
    /// turn into `agent_id`'s transcript. Sub-agents get a leading
    /// `(sub agent N)` tag so their per-turn counts stay
    /// distinguishable; the format otherwise matches the legacy
    /// `display_token_usage` line.
    fn append_turn_usage(&self, tui: &mut Tui, agent_id: AgentId, usage: &TokenUsage) {
        let line = format_turn_usage_line(agent_id, usage);
        let styled = aj_tui::style::dim(&line);
        self.push_chat_child(tui, agent_id, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append `child` to `agent_id`'s transcript container and
    /// return its index. Centralises the slot lookup and the
    /// inter-element spacing: when the target container already has
    /// at least one child this inserts a one-row [`Spacer`] before
    /// `child`, so each chat component stays focused on its own
    /// layout while the container owns the vertical breathing room.
    ///
    /// The returned index is the *child's* slot, not the spacer's.
    /// Returns `0` when the chat slot or the agent's container is
    /// absent (e.g. a sub-agent event arrived before its box).
    fn push_chat_child(
        &self,
        tui: &mut Tui,
        agent_id: AgentId,
        child: Box<dyn aj_tui::component::Component>,
    ) -> usize {
        let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) else {
            return 0;
        };
        let Some(container) = chat.agent_container_mut(agent_id) else {
            return 0;
        };
        if !container.is_empty() {
            container.add_child(Box::new(Spacer::new(1)));
        }
        let idx = container.len();
        container.add_child(child);
        idx
    }
}

/// Resolve the `aj.agent.open` shortcut label for the footer's
/// running-agent hint, falling back to `Alt+A` when unbound.
fn agent_picker_hint() -> String {
    aj_tui::keybindings::format_action_shortcut(crate::config::keybindings::ACTION_AGENT_PICKER)
        .unwrap_or_else(|| "Alt+A".to_string())
}

/// Render the `Token Usage - ...` line for a single `TurnUsage`
/// event. Sub-agents are tagged with their `(sub agent N)` prefix
/// so their per-turn counts stand apart from the main agent's.
/// Visible for testing.
fn format_turn_usage_line(agent_id: AgentId, usage: &TokenUsage) -> String {
    // `format_tokens` keeps the legacy convention: render the
    // accumulated total bare when the turn contributed nothing
    // (e.g. a cached read of an existing tool result), or
    // `acc+turn` so the per-turn delta is visible at a glance.
    let format_tokens = |acc: u64, turn: u64| -> String {
        if turn == 0 {
            format!("{acc}")
        } else {
            format!("{acc}+{turn}")
        }
    };
    let input_str = format_tokens(usage.accumulated_input, usage.turn_input);
    let output_str = format_tokens(usage.accumulated_output, usage.turn_output);
    let cache_creation_str = format_tokens(usage.accumulated_cache_write, usage.turn_cache_write);
    let cache_read_str = format_tokens(usage.accumulated_cache_read, usage.turn_cache_read);
    let body = format!(
        "Token Usage - Input: {input_str} | Output: {output_str} | Cache Creation: {cache_creation_str} | Cache Read: {cache_read_str}",
    );
    match agent_id {
        AgentId::Main => body,
        AgentId::Sub(n) => format!("(sub agent {n}) {body}"),
    }
}

/// Pull and clear the editor's submitted text. Returns `Some` at
/// most once per editor submission; the host's main loop calls
/// this after every input event so a freshly-submitted prompt
/// doesn't sit in the editor's buffer waiting for the next event.
pub fn take_submitted_prompt(tui: &mut Tui) -> Option<String> {
    let editor = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx())?;
    editor.take_submitted()
}

/// Toggle the editor's `disable_submit` flag. Used by the main
/// loop to gate a second submission while the agent is still
/// running the previous prompt.
pub fn set_editor_submit_enabled(tui: &mut Tui, enabled: bool) {
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.disable_submit = !enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aj_agent::events::AgentSettings;
    use aj_tui::component::Component;
    use aj_tui::terminal::ProcessTerminal;

    use crate::config::theme::{ChatTheme, ThemeHandle, chat_theme};
    use crate::modes::interactive::layout::build_layout;

    /// Build a TokenUsage carrying the supplied per-turn deltas
    /// and the running accumulator state observed *before* this
    /// turn was folded in (`already`). Matches the wire semantic
    /// on [`aj_agent::events::AgentEvent::TurnUsage`]: each
    /// `accumulated_*` field is the running total before this
    /// turn, and `turn_*` is the delta the turn is contributing
    /// on top.
    fn token_usage(turn: [u64; 4], already: [u64; 4]) -> TokenUsage {
        TokenUsage {
            accumulated_input: already[0],
            turn_input: turn[0],
            accumulated_output: already[1],
            turn_output: turn[1],
            accumulated_cache_write: already[2],
            turn_cache_write: turn[2],
            accumulated_cache_read: already[3],
            turn_cache_read: turn[3],
        }
    }

    /// Build a synthetic `AssistantMessage` partial with the
    /// scripted-provider identity stamped onto it. Used by the
    /// tests below to construct `AssistantMessageEvent` payloads
    /// that drive the pump's message-update path.
    fn empty_assistant_partial() -> aj_models::types::AssistantMessage {
        aj_models::types::AssistantMessage {
            content: Vec::new(),
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: aj_models::types::Usage::default(),
            stop_reason: aj_models::types::StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Build an `AgentEvent::MessageUpdate` carrying the given
    /// streaming-protocol event. Threading an empty partial through
    /// keeps each call site short.
    fn message_update_event(event: AssistantMessageEvent) -> AgentEvent {
        AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(empty_assistant_partial())),
            event,
        }
    }

    /// Build an `AgentEvent::MessageStart` for an assistant turn.
    fn assistant_message_start_event() -> AgentEvent {
        AgentEvent::MessageStart {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(empty_assistant_partial())),
        }
    }

    /// Build an `AgentEvent::MessageEnd` carrying a user message
    /// with the given text. Mirrors what the agent emits for a
    /// freshly-submitted readline prompt and what
    /// `aj_session::replay` synthesizes for a resumed user thread.
    fn user_message_end_event(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(text))),
        }
    }

    #[test]
    fn format_turn_usage_line_emits_acc_plus_turn_for_main_agent() {
        // First turn: the accumulator is still zero, so each
        // field prints `0+turn` — "there was nothing before, and
        // this turn is contributing `turn`".
        let usage = token_usage([100, 50, 30, 5], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 0+100 | Output: 0+50 | Cache Creation: 0+30 | Cache Read: 0+5",
        );
    }

    #[test]
    fn format_turn_usage_line_drops_turn_part_when_turn_is_zero() {
        // The legacy renderer hides the `+turn` suffix when the
        // turn contributed nothing — a cached read of an existing
        // tool result, for example. Pin that behaviour so we don't
        // start showing `+0` rows for routine cache hits.
        let usage = token_usage([0, 0, 0, 0], [200, 80, 0, 14]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 200 | Output: 80 | Cache Creation: 0 | Cache Read: 14",
        );
    }

    #[test]
    fn format_turn_usage_line_prefixes_sub_agent_id() {
        // Sub-agents share the parent's bus, so their per-turn
        // usage line is tagged with `(sub agent N)` to keep the
        // rows distinguishable in the shared scrollback. This is
        // the sub-agent's first turn, so the accumulator is still
        // zero (each field prints `0+turn`); cache_read's turn
        // delta is zero so the `+turn` suffix drops off there.
        let usage = token_usage([10, 5, 1, 0], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Sub(2), &usage);
        assert_eq!(
            line,
            "(sub agent 2) Token Usage - Input: 0+10 | Output: 0+5 | Cache Creation: 0+1 | Cache Read: 0",
        );
    }

    /// Build a fresh `Tui` + layout pair for event-pump tests.
    /// Returns the populated `Tui` and a paired `EventPump` so the
    /// caller can dispatch events and inspect the chat container's
    /// contents afterwards. The pump is seeded with an empty
    /// catalog; tests exercising window resolution use
    /// [`fresh_tui_with_catalog`].
    fn fresh_tui_with_layout() -> (aj_tui::tui::Tui, EventPump, ChatTheme) {
        fresh_tui_with_catalog(Vec::new())
    }

    /// [`fresh_tui_with_layout`] with a caller-supplied model
    /// catalog for the pump's window resolution.
    fn fresh_tui_with_catalog(
        catalog: Vec<aj_models::registry::ModelInfo>,
    ) -> (aj_tui::tui::Tui, EventPump, ChatTheme) {
        let mut tui = aj_tui::tui::Tui::new(Box::new(ProcessTerminal::new()));
        let theme = ThemeHandle::new(crate::config::theme::Theme::bundled_dark());
        build_layout(&mut tui, &theme, true);
        let chat = chat_theme(&theme, true);
        // Test-only: 200k matches the canonical Sonnet window so
        // any incidental "context_window" expectations in future
        // tests don't need to know about a synthetic value.
        let pump = EventPump::new(
            chat.clone(),
            RenderSettings::new(false, false, true),
            main_settings(),
            200_000,
            Arc::new(catalog),
            MessageQueues::default(),
        );
        (tui, pump, chat)
    }

    /// The Main agent's seed settings for the test pump.
    fn main_settings() -> AgentSettings {
        AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-main".into(),
            thinking: "off".into(),
            speed: "standard".into(),
        }
    }

    /// Build a `Tui` + `EventPump` sharing `queues`, for pending-box
    /// tests that need to enqueue on the same handle the pump reads.
    fn fresh_tui_with_queues(queues: MessageQueues) -> (aj_tui::tui::Tui, EventPump) {
        let mut tui = aj_tui::tui::Tui::new(Box::new(ProcessTerminal::new()));
        let theme = ThemeHandle::new(crate::config::theme::Theme::bundled_dark());
        build_layout(&mut tui, &theme, true);
        let pump = EventPump::new(
            chat_theme(&theme, true),
            RenderSettings::new(false, false, true),
            main_settings(),
            200_000,
            Arc::new(Vec::new()),
            queues,
        );
        (tui, pump)
    }

    fn render_pending(tui: &mut aj_tui::tui::Tui) -> Vec<String> {
        tui.get_mut_as::<PendingMessage>(SlotIndex::Pending.idx())
            .map(|p| p.render(80))
            .unwrap_or_default()
    }

    /// A `QueueUpdate` for the viewed agent repaints the box from the
    /// live snapshot; an empty queue leaves it blank.
    #[test]
    fn queue_update_paints_pending_box_for_active_view() {
        let queues = MessageQueues::default();
        let (mut tui, mut pump) = fresh_tui_with_queues(queues.clone());

        pump.sync_pending(&mut tui);
        assert!(
            render_pending(&mut tui).is_empty(),
            "no message → empty box"
        );

        queues.append_follow_up(AgentId::Main, "clean up later");
        pump.handle(
            &mut tui,
            &AgentEvent::QueueUpdate {
                agent_id: AgentId::Main,
                steering: Vec::new(),
                follow_up: Vec::new(),
            },
        );
        let lines = render_pending(&mut tui);
        assert!(lines.iter().any(|l| l.contains("clean up later")));
        assert!(lines.iter().any(|l| l.contains("queued")));
    }

    /// The box is view-scoped: a queue change for a non-viewed agent
    /// doesn't paint it, but switching to that agent's view does.
    #[test]
    fn pending_box_is_view_scoped() {
        let queues = MessageQueues::default();
        let (mut tui, mut pump) = fresh_tui_with_queues(queues.clone());

        // Queue for Sub(1) while viewing Main.
        queues.append_steering(AgentId::Sub(1), "do this now");
        pump.handle(
            &mut tui,
            &AgentEvent::QueueUpdate {
                agent_id: AgentId::Sub(1),
                steering: Vec::new(),
                follow_up: Vec::new(),
            },
        );
        assert!(
            render_pending(&mut tui).is_empty(),
            "Sub(1)'s pending message must not show while viewing Main"
        );

        pump.set_active_view(&mut tui, AgentId::Sub(1));
        let lines = render_pending(&mut tui);
        assert!(lines.iter().any(|l| l.contains("do this now")));
        assert!(lines.iter().any(|l| l.contains("steering")));

        pump.set_active_view(&mut tui, AgentId::Main);
        assert!(
            render_pending(&mut tui).is_empty(),
            "switching back to Main hides Sub(1)'s pending message"
        );
    }

    /// Catalog row with only the identity + window fields the
    /// pump's resolution reads.
    fn catalog_model(
        provider: &str,
        id: &str,
        context_window: u64,
    ) -> aj_models::registry::ModelInfo {
        aj_models::registry::ModelInfo {
            id: id.into(),
            name: id.into(),
            api: "anthropic-messages".into(),
            provider: provider.into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            supports_adaptive_thinking: false,
            input: vec![aj_models::registry::InputModality::Text],
            cost: aj_models::registry::ModelCost::default(),
            context_window,
            max_tokens: 100,
            headers: None,
        }
    }

    /// `SubAgentStart` for `Sub(n)` carrying the given settings
    /// identity (thinking "off", speed "standard").
    fn sub_agent_start(n: usize, provider: &str, model_id: &str) -> AgentEvent {
        AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(n),
            task: format!("task {n}"),
            settings: AgentSettings {
                provider: provider.into(),
                model_id: model_id.into(),
                thinking: "off".into(),
                speed: "standard".into(),
            },
        }
    }

    /// Render the footer and return the rendered row as a single
    /// string, ANSI escape codes stripped. Test helper for the
    /// context-occupancy assertions below.
    fn rendered_footer(tui: &mut Tui) -> String {
        use crate::modes::interactive::components::footer::Footer;
        let footer = tui
            .get_mut_as::<Footer>(SlotIndex::Footer.idx())
            .expect("footer slot");
        let lines = footer.render(120);
        let joined = lines.join("\n");
        // Strip the SGR escape sequences so tests assert against
        // visible content only. Matches the pattern used elsewhere
        // in this module ("\x1b[2m" + "\x1b[22m" dim wrap).
        let mut s = joined;
        for code in [
            "\x1b[2m", "\x1b[22m", "\x1b[33m", "\x1b[31m", "\x1b[39m", "\x1b[0m",
        ] {
            s = s.replace(code, "");
        }
        s
    }

    #[test]
    fn main_agent_turn_usage_drives_the_footer_indicator() {
        // Sum of the prompt-side fields is the numerator:
        // 1_000 + 200 + 50 = 1_250 tokens; window is 200k -> 0.6%.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.sync_footer(&mut tui);
        // Before any turn lands the indicator is "?/200k".
        assert!(rendered_footer(&mut tui).contains("?/200k"));

        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage: token_usage([1_000, 999, 50, 200], [0, 0, 0, 0]),
            },
        );

        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("1.2k/200k"),
            "expected `1.2k/200k` numerator/denominator in {line:?}",
        );
        assert!(
            line.contains("(0.6%)"),
            "expected `(0.6%)` percentage in {line:?}",
        );
    }

    #[test]
    fn sub_agent_turn_usage_leaves_the_footer_alone() {
        // The footer tracks the *viewed* agent; Main is viewed
        // here, so a sub-agent's usage folds into the sub's own
        // entry without moving the rendered footer.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.sync_footer(&mut tui);
        let before = rendered_footer(&mut tui);

        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Sub(1),
                usage: token_usage([5_000, 1_000, 200, 100], [0, 0, 0, 0]),
            },
        );

        assert_eq!(
            rendered_footer(&mut tui),
            before,
            "sub-agent TurnUsage should not move the main footer indicator",
        );
    }

    #[test]
    fn note_agent_settings_repaints_model_line_and_denominator() {
        // A settings change for the viewed agent updates both the
        // model line and the indicator's denominator immediately
        // rather than waiting for the next turn.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage: token_usage([10_000, 0, 0, 0], [0, 0, 0, 0]),
            },
        );
        let line = rendered_footer(&mut tui);
        assert!(line.contains("10k/200k"));
        assert!(line.contains("claude-main off"));

        pump.note_agent_settings(
            &mut tui,
            AgentId::Main,
            AgentSettings {
                provider: "anthropic".into(),
                model_id: "claude-next".into(),
                thinking: "high".into(),
                speed: "standard".into(),
            },
            100_000,
        );
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("10k/100k"),
            "denominator should follow the new context window; got {line:?}",
        );
        assert!(
            line.contains("claude-next high"),
            "model line should follow the new settings; got {line:?}",
        );
    }

    #[test]
    fn switching_views_swaps_the_footer_between_agents() {
        // SubAgentStart seeds the sub's entry (window from the
        // catalog); viewing it shows its model line + `?/<window>`,
        // its usage updates live, and switching back restores
        // main's line and usage.
        let (mut tui, mut pump, _theme) =
            fresh_tui_with_catalog(vec![catalog_model("openai", "gpt-sub", 400_000)]);
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage: token_usage([10_000, 0, 0, 0], [0, 0, 0, 0]),
            },
        );
        pump.handle(&mut tui, &sub_agent_start(1, "openai", "gpt-sub"));

        pump.set_active_view(&mut tui, AgentId::Sub(1));
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("gpt-sub off") && line.contains("?/400k"),
            "sub view should show the sub's line and seeded window; got {line:?}",
        );

        // A sub turn updates the viewed footer live.
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Sub(1),
                usage: token_usage([4_000, 0, 0, 0], [0, 0, 0, 0]),
            },
        );
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("4.0k/400k"),
            "viewed sub's usage should move the footer; got {line:?}",
        );

        // Switching back restores main's line and usage.
        pump.set_active_view(&mut tui, AgentId::Main);
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("claude-main off") && line.contains("10k/200k"),
            "main view should restore main's line and usage; got {line:?}",
        );
    }

    #[test]
    fn window_resolution_catalog_hit_identity_match_and_full_miss() {
        let (mut tui, mut pump, _theme) =
            fresh_tui_with_catalog(vec![catalog_model("openai", "gpt-sub", 400_000)]);

        // Catalog hit.
        pump.handle(&mut tui, &sub_agent_start(1, "openai", "gpt-sub"));
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        assert!(rendered_footer(&mut tui).contains("?/400k"));

        // Catalog miss, identity equals Main's settings: Main's
        // window.
        pump.handle(&mut tui, &sub_agent_start(2, "anthropic", "claude-main"));
        pump.set_active_view(&mut tui, AgentId::Sub(2));
        assert!(rendered_footer(&mut tui).contains("?/200k"));

        // Full miss: window 0 suppresses the indicator.
        pump.handle(&mut tui, &sub_agent_start(3, "mystery", "unknown-model"));
        pump.set_active_view(&mut tui, AgentId::Sub(3));
        let line = rendered_footer(&mut tui);
        assert!(
            !line.contains('/'),
            "unknown window should suppress the indicator; got {line:?}",
        );
        assert!(line.contains("unknown-model off"));
    }

    #[test]
    fn note_main_settings_while_viewing_a_sub_does_not_repaint() {
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(&mut tui, &sub_agent_start(1, "anthropic", "claude-main"));
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        let before = rendered_footer(&mut tui);

        pump.note_agent_settings(
            &mut tui,
            AgentId::Main,
            AgentSettings {
                provider: "anthropic".into(),
                model_id: "claude-next".into(),
                thinking: "max".into(),
                speed: "standard".into(),
            },
            100_000,
        );
        assert_eq!(
            rendered_footer(&mut tui),
            before,
            "a Main settings change must not repaint a sub view",
        );

        // The new line shows after switching back to main.
        pump.set_active_view(&mut tui, AgentId::Main);
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("claude-next max"),
            "main view should show the new line; got {line:?}",
        );
    }

    #[test]
    fn replayed_spawn_and_usage_populate_the_sub_views_footer() {
        // Replay synthesizes `SubAgentStart` with the recorded
        // settings followed by per-agent `TurnUsage` events; pumping
        // them through `handle` must populate the sub view's footer
        // like a live run.
        let (mut tui, mut pump, _theme) =
            fresh_tui_with_catalog(vec![catalog_model("openai", "gpt-sub", 400_000)]);
        pump.handle(&mut tui, &sub_agent_start(1, "openai", "gpt-sub"));
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Sub(1),
                usage: token_usage([4_000, 0, 0, 0], [0, 0, 0, 0]),
            },
        );

        pump.set_active_view(&mut tui, AgentId::Sub(1));
        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("gpt-sub off") && line.contains("4.0k/400k"),
            "resumed sub view should carry its recorded footer state; got {line:?}",
        );
    }

    #[test]
    fn replay_tool_result_without_start_does_not_steal_next_assistant_message() {
        // Regression for the resume-fidelity reorder bug. On a
        // resumed session the disk shape is:
        //   user prompt -> assistant (thinking + tool_use) ->
        //   tool_result -> assistant (final text)
        // Replay walks these in append order and (today) emits a
        // `ToolExecutionEnd` without a matching
        // `ToolExecutionStart`, so this listener falls into the
        // build-on-miss branch of `update_tool_execution_result`.
        // That branch must clear `current_assistant`, otherwise
        // the *next* assistant message's `StreamChunk(Text, ...)`
        // attaches to the assistant component that was already
        // appended for the thinking block (which sits *above* the
        // tool in the chat container), and the visible scrollback
        // ends up with the final assistant text rendered *before*
        // the tool execution. Pin the correct order with a
        // canonical replay event sequence.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        let chat_len = |tui: &mut Tui| -> usize {
            tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .expect("chat slot")
                .container_mut()
                .len()
        };
        let chat_baseline = chat_len(&mut tui);

        // User prompt → user message component.
        pump.handle(&mut tui, &user_message_end_event("please run a tool"));

        // Assistant message lifecycle: MessageStart opens the slot.
        pump.handle(&mut tui, &assistant_message_start_event());

        // Assistant thinking block → drives the slot.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "let me think".into(),
                partial: empty_assistant_partial(),
            }),
        );

        // Tool result lands without a preceding ToolExecutionStart
        // (replay path). The build-on-miss branch creates the
        // tool component AND must drop `current_assistant`.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: "bash".into(),
                    body: "hello from aj".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // Final assistant text (next persisted assistant message).
        // After the fix this must open a *fresh* assistant
        // component appended *after* the tool component, not
        // reuse the thinking-block component that was appended
        // before the tool.
        pump.handle(&mut tui, &assistant_message_start_event());
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Done. Anything specific?".into(),
                partial: empty_assistant_partial(),
            }),
        );

        // Walk the chat container and verify (a) the last child
        // is the final assistant text, and (b) a
        // `ToolExecutionComponent` sits strictly before it. With
        // the regression in place the tool would be the last
        // child instead.
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut();
        let total = chat.len();
        assert!(total > chat_baseline + 6, "got {total} children");
        let last_idx = total - 1;
        let last_is_assistant = chat
            .get_mut_as::<AssistantMessageComponent>(last_idx)
            .is_some();
        assert!(
            last_is_assistant,
            "last chat child should be the final assistant message; \
             a regression would leave the tool execution at the tail",
        );
        let mut tool_idx: Option<usize> = None;
        for i in chat_baseline..last_idx {
            if chat.get_mut_as::<ToolExecutionComponent>(i).is_some() {
                tool_idx = Some(i);
                break;
            }
        }
        assert!(
            tool_idx.is_some(),
            "expected a ToolExecutionComponent before the final \
             assistant message; got chat layout with {total} children",
        );
    }

    #[test]
    fn tool_use_only_turn_does_not_leave_an_empty_assistant_slot() {
        // Regression: a turn where the model emitted only a
        // `tool_use` block (no text, no thinking) used to leave
        // an empty `AssistantMessageComponent` in the chat
        // container. The component rendered zero lines but the
        // leading auto-spacer that `push_chat_child` inserts
        // before every new child stuck around and doubled up
        // with the next sibling's leading spacer, producing two
        // visible blank rows where one was intended.
        //
        // The lifecycle below mirrors what the agent actually
        // emits for a tool-use-only turn: `MessageStart{Assistant}`
        // → `ToolCallStart/Delta/End` `MessageUpdate`s →
        // `MessageEnd{Assistant}` carrying a single `ToolCall`
        // block → `ToolExecutionStart` → `ToolExecutionEnd`. None
        // of those events should materialise an
        // `AssistantMessageComponent`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        pump.handle(&mut tui, &user_message_end_event("please run a tool"));
        pump.handle(&mut tui, &assistant_message_start_event());

        // Tool-call deltas; none of these paint into the assistant
        // component, so the slot must NOT be materialised here.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"cmd\":\"ls\"}".into(),
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: aj_models::types::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                },
                partial: empty_assistant_partial(),
            }),
        );

        // `MessageEnd` carrying a tool-only assistant payload. The
        // replay-synthesis branch must skip slot materialisation
        // because there's no Text / Thinking content to paint.
        let assistant_tool_only = aj_models::types::AssistantMessage {
            content: vec![AssistantContent::ToolCall(aj_models::types::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"cmd": "ls"}),
            })],
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: aj_models::types::Usage::default(),
            stop_reason: aj_models::types::StopReason::ToolUse,
            error: None,
            timestamp: 0,
        };
        pump.handle(
            &mut tui,
            &AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::Assistant(assistant_tool_only)),
            },
        );

        // Tool runs.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({"cmd": "ls"}),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: "bash".into(),
                    body: "ok".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // No `AssistantMessageComponent` anywhere in the chat
        // container — the regression would put an empty one
        // between the user message and the tool execution.
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut();
        let assistant_count = (0..chat.len())
            .filter(|&i| chat.get_mut_as::<AssistantMessageComponent>(i).is_some())
            .count();
        assert_eq!(
            assistant_count,
            0,
            "tool-use-only turn must not leave an AssistantMessageComponent in chat; \
             chat has {} children",
            chat.len(),
        );
    }

    #[test]
    fn thinking_stream_survives_empty_snapshot_stop_event() {
        // Regression: the agent emits `ThinkingStop` with an
        // empty snapshot (the streaming layer doesn't accumulate
        // a canonical thinking-channel snapshot the way it does
        // for text). A naive pump would feed that empty string
        // back through `set_thinking_snapshot` and wipe the
        // accumulated deltas, making the entire thinking block
        // disappear the instant streaming finished.
        //
        // We assert that the rendered output still contains the
        // streamed thinking content after the Stop event lands.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // Open a thinking stream and append a non-trivial body.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "first let me reason about the".to_string(),
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: " inputs carefully".to_string(),
                partial: empty_assistant_partial(),
            }),
        );
        // The empty-content ThinkingEnd is the exact shape the
        // agent emits when the provider finalizes a thinking block
        // without an authoritative snapshot. With the regression in
        // place this wiped the buffer.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: String::new(),
                partial: empty_assistant_partial(),
            }),
        );

        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut();
        let last = chat.len() - 1;
        let assistant = chat
            .get_mut_as::<AssistantMessageComponent>(last)
            .expect("assistant message at chat tail after thinking stream");
        let rendered = assistant.render(80).join("\n");
        assert!(
            rendered.contains("first let me reason about the inputs carefully"),
            "expected accumulated thinking text to survive the empty-snapshot \
             Stop event; got rendered:\n{rendered}"
        );
    }

    #[test]
    fn turn_usage_event_appends_one_chat_row() {
        // End-to-end: dispatch a `TurnUsage` event and verify a
        // new child landed in the chat container with the
        // formatted line inside it (the dim escape wraps the
        // visible body verbatim, so `.contains` is enough).
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        let chat_len_before = {
            let chat = tui
                .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .expect("chat slot")
                .container_mut();
            chat.len()
        };
        let usage = token_usage([42, 17, 0, 3], [0, 0, 0, 0]);
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage,
            },
        );
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut();
        assert_eq!(
            chat.len(),
            chat_len_before + 1,
            "TurnUsage should append exactly one chat row",
        );
        let row = chat
            .get_mut_as::<aj_tui::components::text::Text>(chat_len_before)
            .expect("the appended row should be a Text component");
        // `render` walks the styled string; the dim escape sequence
        // brackets the body, so a `contains` check on the visible
        // payload is robust to trailing/leading ANSI bytes.
        let lines = row.render(120);
        let joined = lines.join("\n");
        assert!(
            joined.contains(
                "Token Usage - Input: 0+42 | Output: 0+17 | Cache Creation: 0 | Cache Read: 0+3"
            ),
            "row should carry the formatted usage line, got: {joined:?}",
        );
        assert!(
            joined.contains("\x1b[2m"),
            "row should be wrapped in the dim ANSI escape",
        );
    }

    #[test]
    fn nested_subagent_lifecycle_keeps_loader_running_until_main_ends() {
        // The loader's only periodic source of `request_render`
        // calls is its animation pump. The status spinner is scoped
        // to the *viewed* agent; the default view is Main, so while
        // the main turn runs the spinner stays on regardless of a
        // sub-agent starting and ending inside it. Pin that: viewing
        // Main, the loader is active from Main's `AgentStart` through
        // the nested sub start/end and only stops on Main's
        // `AgentEnd`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Fresh pump: loader is idle.
        assert!(!is_loader_active(&mut tui));

        // Main turn starts.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(is_loader_active(&mut tui), "loader should start on main");

        // Sub-agent starts inside the main turn.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        assert!(
            is_loader_active(&mut tui),
            "loader should stay active across the sub-agent start",
        );

        // Sub-agent ends — main is still running.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert!(
            is_loader_active(&mut tui),
            "loader must NOT stop on the sub-agent's AgentEnd: \
             the main turn is still running and the user expects \
             a spinner during the resume",
        );

        // Main turn ends.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        assert!(
            !is_loader_active(&mut tui),
            "loader should stop once the main agent ends",
        );
    }

    #[test]
    fn unmatched_subagent_end_does_not_stop_the_loader() {
        // Defensive: a stray `AgentEnd` for a sub-agent we never saw
        // start removes only that (absent) id from the set, so the
        // viewed Main agent stays running and its spinner stays on.
        // Per-view scoping means an unrelated sub's lifecycle can
        // never toggle the Main view's spinner.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(7),
                messages: Vec::new(),
            },
        );
        let status = tui
            .get_mut_as::<Container>(SlotIndex::Status.idx())
            .expect("status slot");
        let loader = status.get_mut_as::<LoaderStatus>(0).expect("loader status");
        assert!(
            loader.is_active(),
            "a sub-agent AgentEnd must not stop the loader while \
             the main agent is still active",
        );
    }

    #[test]
    fn main_end_stops_loader_when_viewing_main_despite_leaked_sub() {
        // Per-view scoping: the status spinner reflects the *viewed*
        // agent. The default active view is Main, so Main's
        // `AgentEnd` turns the spinner off even though a sub-agent's
        // own `AgentEnd` was dropped earlier in the turn (typical
        // cause: the parent's `spawn_agent` future was cancelled
        // mid-await, so the sub's `AgentEnd` emit never fired).
        //
        // The pump keeps `running_agents` as literal truth: the
        // leaked `Sub(1)` is *still* in the set after Main's end.
        // Reconciling it is the binary's job on main-turn completion
        // (it drains running subs it isn't independently driving via
        // `mark_idle`), not the pump's — so the spinner-off here is
        // purely the per-view scoping, not a drain.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Main turn starts and spawns a sub-agent.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        assert!(is_loader_active(&mut tui));

        // Note: no `AgentEnd(Sub(1))` — simulates the dropped emit.
        // Main's `AgentEnd` fires.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );

        assert!(
            !is_loader_active(&mut tui),
            "loader must stop on Main's AgentEnd while viewing Main: \
             the spinner is scoped to the viewed agent",
        );
        assert!(
            pump.is_running(AgentId::Sub(1)),
            "the leaked sub stays in the set: the binary, not the \
             pump, reconciles it on main-turn completion",
        );
    }

    #[test]
    fn second_main_turn_recovers_loader_after_a_leaked_subagent() {
        // Per-view scoping holds across turns: with the default view
        // on Main, the status spinner tracks Main's running state
        // regardless of a leaked sub still lingering in the set. A
        // second main turn drives the spinner on/off cleanly because
        // it is keyed on Main's own `AgentStart`/`AgentEnd`, not on
        // the (still non-empty) set as a whole.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Turn 1: main starts, sub-agent starts, but the sub's
        // `AgentEnd` never arrives (leak). Main eventually ends.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(3),
            },
        );
        // Note: no AgentEnd(Sub(3)) here.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        // Viewing Main, which is now idle: the spinner stops even
        // though the leaked sub is still in the set.
        assert!(
            !is_loader_active(&mut tui),
            "loader stops when the viewed Main agent ends, per-view scoping",
        );

        // Turn 2: a fresh `AgentStart(Main)` makes Main running
        // again; the spinner follows the viewed agent on, then off
        // on Main's end. The leaked `Sub(3)` never affects the Main
        // view's spinner.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(is_loader_active(&mut tui));
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        assert!(
            !is_loader_active(&mut tui),
            "second main turn ends with the loader stopped",
        );
    }

    /// Shared loader-state probe for the per-view spinner tests.
    fn loader_active(tui: &mut Tui) -> bool {
        tui.get_mut_as::<Container>(SlotIndex::Status.idx())
            .expect("status slot")
            .get_mut_as::<LoaderStatus>(0)
            .expect("loader status")
            .is_active()
    }

    #[test]
    fn spinner_follows_viewed_agent() {
        // The status spinner reflects the *viewed* agent, not global
        // activity. With Main idle and only `Sub(1)` running, the
        // spinner is on iff the sub is the active view; switching
        // back to the idle Main turns it off; and once the sub ends,
        // viewing it shows no spinner.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // Create the sub box, then start its turn (sub running).
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "explore".into(),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );

        // Viewing Main (the default): Main isn't running, so off.
        assert!(
            !loader_active(&mut tui),
            "viewing the idle Main agent, the spinner must be off \
             even though a sub is running",
        );

        // Switch to the running sub: spinner on.
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        assert!(
            loader_active(&mut tui),
            "viewing the running sub, the spinner must be on",
        );

        // Back to idle Main: off again.
        pump.set_active_view(&mut tui, AgentId::Main);
        assert!(!loader_active(&mut tui));

        // The sub finishes; viewing it now shows no spinner.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        assert!(
            !loader_active(&mut tui),
            "viewing the finished sub, the spinner must be off",
        );
    }

    #[test]
    fn footer_count_is_aggregate_across_running_subs() {
        // The footer's `N agent(s)` indicator is an aggregate over
        // running `Sub(_)` ids — independent of the active view — so
        // background activity stays visible while viewing an idle
        // agent.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        for n in [1usize, 2] {
            pump.handle(
                &mut tui,
                &AgentEvent::SubAgentStart {
                    parent: AgentId::Main,
                    child: AgentId::Sub(n),
                    task: format!("task {n}"),
                    settings: AgentSettings {
                        provider: "scripted".into(),
                        model_id: "scripted-model".into(),
                        thinking: "off".into(),
                        speed: "standard".into(),
                    },
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(n),
                },
            );
        }

        // Viewing Main: the aggregate still reads two.
        assert!(
            rendered_footer(&mut tui).contains("2 agents"),
            "footer should aggregate both running subs; got {:?}",
            rendered_footer(&mut tui),
        );

        // Switching the view doesn't change the aggregate.
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        assert!(
            rendered_footer(&mut tui).contains("2 agents"),
            "footer count is view-independent; got {:?}",
            rendered_footer(&mut tui),
        );
    }

    // ---- Background tasks --------------------------------------------------

    /// Full ANSI-strip (SGR sequences), unlike `rendered_footer`'s
    /// fixed-code replacement: tool bubbles paint with arbitrary
    /// truecolor backgrounds.
    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    /// `ToolDetails::Bash` payload with the given task id and
    /// stdout; the other fields stay at their background-launch
    /// defaults.
    fn bash_task_details(stdout: &str, task_id: Option<usize>) -> aj_agent::tool::ToolDetails {
        aj_agent::tool::ToolDetails::Bash {
            command: "sleep 5".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id,
        }
    }

    fn task_start(owner: AgentId, task_id: usize, call_id: &str, kind: TaskKind) -> AgentEvent {
        AgentEvent::TaskStart {
            agent_id: owner,
            task_id,
            call_id: call_id.into(),
            kind,
            label: "sleep 5".into(),
        }
    }

    fn task_end(owner: AgentId, task_id: usize, call_id: &str, status: TaskStatus) -> AgentEvent {
        AgentEvent::TaskEnd {
            agent_id: owner,
            task_id,
            call_id: call_id.into(),
            status,
            label: "sleep 5".into(),
        }
    }

    /// Drive a background-bash launch into the pump: the tool call's
    /// cell, the task registration, and the immediately-returning
    /// started result (carrying the task id).
    fn launch_bash_task(tui: &mut Tui, pump: &mut EventPump, task_id: usize, call_id: &str) {
        pump.handle(
            tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: call_id.into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "sleep 5"}),
            },
        );
        pump.handle(
            tui,
            &task_start(
                AgentId::Main,
                task_id,
                call_id,
                TaskKind::Bash {
                    command: "sleep 5".into(),
                },
            ),
        );
        pump.handle(
            tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: call_id.into(),
                tool: "bash".into(),
                result: bash_task_details("", Some(task_id)),
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );
    }

    /// Render the launch cell at `idx` in the main transcript,
    /// ANSI-stripped and newline-joined.
    fn rendered_cell(tui: &mut Tui, idx: usize) -> String {
        tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut()
            .get_mut_as::<ToolExecutionComponent>(idx)
            .expect("tool cell at index")
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Index of the single tool cell in the main transcript.
    fn main_tool_cell(tui: &mut Tui) -> usize {
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .container_mut();
        (0..chat.len())
            .find(|&i| chat.get_mut_as::<ToolExecutionComponent>(i).is_some())
            .expect("a tool cell in the main transcript")
    }

    #[test]
    fn footer_counts_agents_and_bash_tasks_separately() {
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // One running sub-agent: agents-only indicator.
        pump.handle(&mut tui, &sub_agent_start(1, "scripted", "scripted-model"));
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        let line = rendered_footer(&mut tui);
        assert!(line.contains("1 agent ("), "got {line:?}");
        assert!(
            !line.contains(" task (") && !line.contains(" tasks ("),
            "got {line:?}"
        );

        // A background bash task joins the indicator.
        pump.handle(
            &mut tui,
            &task_start(
                AgentId::Main,
                1,
                "c-bash",
                TaskKind::Bash {
                    command: "sleep 5".into(),
                },
            ),
        );
        let line = rendered_footer(&mut tui);
        assert!(line.contains("1 agent, 1 task ("), "got {line:?}");

        // An agent-backed task is NOT double-counted: its sub-agent
        // is what drives the agent count.
        pump.handle(
            &mut tui,
            &task_start(
                AgentId::Main,
                2,
                "c-agent",
                TaskKind::Agent {
                    agent_id: 1,
                    task: "explore".into(),
                },
            ),
        );
        let line = rendered_footer(&mut tui);
        assert!(line.contains("1 agent, 1 task ("), "got {line:?}");

        // The bash task ends: back to agents-only.
        pump.handle(
            &mut tui,
            &task_end(AgentId::Main, 1, "c-bash", TaskStatus::Exited(Some(0))),
        );
        let line = rendered_footer(&mut tui);
        assert!(line.contains("1 agent ("), "got {line:?}");
        assert!(
            !line.contains(" task (") && !line.contains(" tasks ("),
            "got {line:?}"
        );

        // The sub ends too: the indicator clears (the agent-kind
        // task entry never counts).
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        let line = rendered_footer(&mut tui);
        assert!(!line.contains("agent ("), "got {line:?}");
        assert!(
            !line.contains(" task (") && !line.contains(" tasks ("),
            "got {line:?}"
        );
    }

    #[test]
    fn task_output_live_tails_the_cell_and_task_end_freezes_it() {
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        launch_bash_task(&mut tui, &mut pump, 1, "c1");
        let cell = main_tool_cell(&mut tui);
        assert!(rendered_cell(&mut tui, cell).contains("[task #1]"));

        // The owning turn ends — this wipes the agent's tool_index,
        // so subsequent routing exercises the TaskStart snapshot.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );

        // Live tail lands in the existing cell.
        pump.handle(
            &mut tui,
            &AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("LIVETAIL", Some(1)),
            },
        );
        let body = rendered_cell(&mut tui, cell);
        assert!(body.contains("LIVETAIL"), "got:\n{body}");

        // TaskEnd freezes the cell with the terminal-status badge;
        // a straggling snapshot no longer lands.
        pump.handle(
            &mut tui,
            &task_end(AgentId::Main, 1, "c1", TaskStatus::Exited(Some(0))),
        );
        let body = rendered_cell(&mut tui, cell);
        assert!(body.contains("[task #1 · exited 0]"), "got:\n{body}");
        pump.handle(
            &mut tui,
            &AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("AFTERFREEZE", Some(1)),
            },
        );
        let body = rendered_cell(&mut tui, cell);
        assert!(!body.contains("AFTERFREEZE"), "got:\n{body}");
        assert!(body.contains("LIVETAIL"), "got:\n{body}");
    }

    #[test]
    fn tasks_snapshot_lists_bash_tasks_with_status_and_owner() {
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &task_start(
                AgentId::Main,
                1,
                "c1",
                TaskKind::Bash {
                    command: "sleep 5".into(),
                },
            ),
        );
        pump.handle(
            &mut tui,
            &task_start(
                AgentId::Sub(2),
                2,
                "c2",
                TaskKind::Agent {
                    agent_id: 2,
                    task: "explore".into(),
                },
            ),
        );

        // Only the bash task surfaces as a picker entry; the agent
        // task is represented by its sub-agent entry instead.
        let tasks = pump.tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].label, "sleep 5");
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(pump.task_owner(1), Some(AgentId::Main));
        assert_eq!(pump.task_owner(2), Some(AgentId::Sub(2)));
        assert_eq!(pump.task_owner(99), None);

        // TaskEnd flips the snapshot's status but keeps the entry
        // so the picker's "all" scope can list it.
        pump.handle(
            &mut tui,
            &task_end(AgentId::Main, 1, "c1", TaskStatus::Killed),
        );
        let tasks = pump.tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Killed);
    }

    #[test]
    fn resumed_launch_cell_renders_badge_without_task_tracking() {
        // Resume fidelity: task events are transient, so replay only
        // delivers the persisted launch result (a `ToolExecutionEnd`
        // without a preceding Start). The cell must still carry its
        // task-id badge, while the pump tracks no task — nothing may
        // assume a registry or a `tasks` entry exists for a badged
        // cell.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "c-resumed".into(),
                tool: "bash".into(),
                result: bash_task_details("", Some(7)),
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );
        let cell = main_tool_cell(&mut tui);
        assert!(rendered_cell(&mut tui, cell).contains("[task #7]"));
        assert!(pump.tasks().is_empty(), "no tracked tasks on resume");
        assert_eq!(pump.task_owner(7), None);
        // Task events without a prior `TaskStart` (impossible live,
        // conceivable on weird replays) must be inert: no panic, no
        // tracking, no footer indicator.
        pump.handle(
            &mut tui,
            &task_end(AgentId::Main, 7, "c-resumed", TaskStatus::Exited(Some(0))),
        );
        assert!(pump.tasks().is_empty(), "unknown TaskEnd tracks nothing");
        let line = rendered_footer(&mut tui);
        assert!(
            !line.contains(" task (") && !line.contains(" tasks ("),
            "no task indicator; got {line:?}"
        );
    }

    #[test]
    fn mark_idle_removes_agent_and_stops_spinner_when_viewed() {
        // `mark_idle` is the binary's reconciliation hook: it forces
        // an agent out of the running set and reconciles everything
        // derived from it. While viewing that agent the spinner must
        // stop, and `is_running` must report `false`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        assert!(loader_active(&mut tui), "viewing the running sub: on");

        pump.mark_idle(&mut tui, AgentId::Sub(1));
        assert!(
            !loader_active(&mut tui),
            "mark_idle stops the spinner of the viewed agent",
        );
        assert!(
            !pump.is_running(AgentId::Sub(1)),
            "mark_idle removes the agent from the running set",
        );
    }

    #[test]
    fn continuation_restarts_then_finishes_box_status() {
        // A continuation re-prompt emits no `SubAgentStart`/`End`, so
        // the box status is driven purely by `AgentStart(Sub n)` →
        // `Running` and `AgentEnd(Sub n)` → `Done`. Set up an
        // initial spawn that finishes `Done`, then feed a fresh
        // start/end pair and assert the box cycles `Running` → `Done`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        let box_status = |tui: &mut Tui| -> SubAgentStatus {
            tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .expect("chat slot")
                .sub_box_mut(1)
                .expect("sub box")
                .status()
        };

        // Initial spawn: box created, runs, finishes Done.
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "explore".into(),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert_eq!(box_status(&mut tui), SubAgentStatus::Done);

        // Continuation: a bare `AgentStart(Sub 1)` flips the box
        // back to Running.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        assert_eq!(
            box_status(&mut tui),
            SubAgentStatus::Running,
            "a fresh AgentStart re-runs the box",
        );

        // The continuation's `AgentEnd` flips it back to Done.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert_eq!(box_status(&mut tui), SubAgentStatus::Done);
    }

    #[test]
    fn subagent_events_route_into_box_and_agent_tool_is_skipped() {
        // The parent's `agent` tool call is represented by the
        // sub-agent box, not a tool bubble, so its `ToolExecution*`
        // events are skipped. The sub-agent's own events route into
        // the box's inner transcript (its tools render header-only).
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // Main fires the `agent` tool: no bubble in the main view.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "call-agent".into(),
                tool: "agent".into(),
                args: serde_json::json!({"task": "summarize"}),
            },
        );
        // The spawn correlation creates the box in the main transcript.
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "summarize".into(),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            },
        );
        // A sub-agent tool call routes into the box.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Sub(1),
                call_id: "call-bash".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Sub(1),
                call_id: "call-bash".into(),
                tool: "bash".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: String::new(),
                    body: "ok".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done".into(),
            },
        );

        // The main transcript carries no tool bubble for the `agent`
        // call.
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot");
        let main = chat.container_mut();
        let main_tools = (0..main.len())
            .filter(|&i| main.get_mut_as::<ToolExecutionComponent>(i).is_some())
            .count();
        assert_eq!(
            main_tools, 0,
            "the `agent` tool call must not create a bubble in the main transcript",
        );

        // The sub-agent's tool routed into its box.
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot");
        let inner = chat
            .agent_container_mut(AgentId::Sub(1))
            .expect("sub-agent box inner container");
        let sub_tools = (0..inner.len())
            .filter(|&i| inner.get_mut_as::<ToolExecutionComponent>(i).is_some())
            .count();
        assert_eq!(
            sub_tools, 1,
            "the sub-agent's tool should route into its box"
        );
    }

    #[test]
    fn observing_a_subagent_shows_full_tool_bodies_then_hides_them_on_main() {
        // Decision: tools render header-only inside the compact box,
        // but full (with bodies) in the switched-to view. A tool that
        // arrives *while observing* the sub-agent must render full, and
        // collapse back to header-only when the user returns to main.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "explore".into(),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            },
        );
        // Observe the sub-agent (its box becomes the full view).
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Sub(1),
                call_id: "c1".into(),
                tool: "read_file".into(),
                args: serde_json::json!({}),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Sub(1),
                call_id: "c1".into(),
                tool: "read_file".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: String::new(),
                    body: "BODYMARKER".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // Full view: the tool body is visible.
        let full = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .render(80)
            .join("\n");
        assert!(
            full.contains("BODYMARKER"),
            "full view should show the tool body; got:\n{full}",
        );

        // Back to main: the compact box renders the tool header-only.
        pump.set_active_view(&mut tui, AgentId::Main);
        let main = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .render(80)
            .join("\n");
        assert!(
            !main.contains("BODYMARKER"),
            "compact box should hide the tool body; got:\n{main}",
        );
    }

    #[test]
    fn switching_to_observe_a_subagent_expands_tools_collected_on_main() {
        // Mirror of the test above, pinning the other direction: a
        // sub-agent tool collected *while on main* arrives header-only
        // (it lives in the compact box). Switching to observe that
        // sub-agent must flip the already-collected tool to its full
        // body, and returning to main must collapse it again. This
        // exercises the view-switch re-flip (`ChatView::set_active` ->
        // `SubAgentBox::set_mode`), independent of the append-time
        // initial state.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "explore".into(),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            },
        );
        // Stay on main: the tool is collected header-only.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Sub(1),
                call_id: "c1".into(),
                tool: "read_file".into(),
                args: serde_json::json!({}),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Sub(1),
                call_id: "c1".into(),
                tool: "read_file".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: String::new(),
                    body: "BODYMARKER".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // Main view: the compact box hides the tool body.
        let main = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .render(80)
            .join("\n");
        assert!(
            !main.contains("BODYMARKER"),
            "compact box should hide the tool body; got:\n{main}",
        );

        // Switch to observe: the already-collected tool expands to
        // its full body.
        pump.set_active_view(&mut tui, AgentId::Sub(1));
        let full = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .render(80)
            .join("\n");
        assert!(
            full.contains("BODYMARKER"),
            "observing should expand the collected tool; got:\n{full}",
        );

        // Return to main: it collapses back to header-only.
        pump.set_active_view(&mut tui, AgentId::Main);
        let main_again = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .render(80)
            .join("\n");
        assert!(
            !main_again.contains("BODYMARKER"),
            "returning to main should re-collapse the tool; got:\n{main_again}",
        );
    }

    /// Runtime regression for a CPU-pegging bug: orphaned loader
    /// animation pumps. The `#[test]`s above pin the bookkeeping
    /// — `running_agents` membership, `Loader::start` / `stop`
    /// calls, `tool_index` lifetime — but they don't exercise the
    /// thing the user actually feels: the loader's animation pump
    /// spawning a fresh tokio task on every `Loader::start` and
    /// only cancelling it on the matching `stop`.
    ///
    /// Under per-view scoping with the default view on Main, a
    /// nested sub-agent's `AgentStart`/`AgentEnd` never toggles the
    /// Main-view spinner (Main stays running throughout), so
    /// `Loader::start` fires exactly once per main turn regardless
    /// of how many sub-agent starts / ends nest inside it. The
    /// render channel should therefore be driven at the loader's
    /// own 80 ms interval rather than at the throttle's 16 ms cap.
    ///
    /// The assertion below counts `TuiEvent::Render`s the
    /// throttle releases over a fixed window after many nested
    /// cycles. A single active pump produces roughly
    /// `window / 80 ms` renders; a regression that leaks pumps
    /// would push the count toward `window / 16 ms`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_subagent_cycles_do_not_accumulate_animation_pumps() {
        use std::time::Duration;

        use aj_tui::tui::TuiEvent;
        use tokio::time::Instant as TokioInstant;

        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        // Suppress the implicit bootstrap render so the
        // post-cycle counting window only sees renders from the
        // loader's animation pump.
        tui.set_initial_render(false);

        // Drive many nested cycles synchronously. Each cycle
        // brackets one main turn around one sub-agent turn:
        //
        //   Main start → Sub start → Sub end → Main end.
        //
        // Viewing Main, this produces 100 paired
        // `Loader::start` / `Loader::stop` calls and the same
        // number of animation-pump task spawns / cancellations
        // — but at any instant only one pump should be alive,
        // because each `Loader::start` cancels the prior token
        // before spawning, and each `Loader::stop` cancels
        // unconditionally.
        for _ in 0..100 {
            pump.handle(
                &mut tui,
                &AgentEvent::AgentStart {
                    agent_id: AgentId::Main,
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    messages: Vec::new(),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentEnd {
                    agent_id: AgentId::Main,
                    messages: Vec::new(),
                },
            );
        }

        // Final main turn — the loader's pump is now running. If
        // any cycle-phase pumps leaked, this is the moment their
        // background `request_render` ticks would still be
        // landing on the channel.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );

        // Drain the synchronous `request_repaint` carry-over
        // from the cycle phase + the final start. We give it a
        // generous 80 ms (one loader interval) so leaked pumps
        // — if any — get at least one chance to tick before
        // the counting window starts. Without this, the
        // measurement would race against the throttle's lazy
        // initialisation on the first `next_event`.
        let drain_until = TokioInstant::now() + Duration::from_millis(80);
        while let Ok(maybe) = tokio::time::timeout_at(drain_until, tui.next_event()).await {
            // Discard whatever we drain; we only care about the
            // *rate* during the measurement window below.
            if maybe.is_none() {
                break;
            }
        }

        // Measure: how many renders does the loader drive over a
        // 320 ms window?
        //
        // - One healthy pump at 80 ms interval ≈ 4 renders.
        // - The throttle's 16 ms floor caps the worst-case
        //   regression at ≈ 20 renders.
        //
        // Assert well below the regression cap (and well above
        // the healthy baseline) so CI scheduler jitter doesn't
        // flake the test in either direction.
        let window = Duration::from_millis(320);
        let deadline = TokioInstant::now() + window;
        let mut renders = 0usize;
        while let Ok(maybe) = tokio::time::timeout_at(deadline, tui.next_event()).await {
            match maybe {
                Some(TuiEvent::Render) => renders += 1,
                Some(_) => {}
                None => break,
            }
        }

        // Tidy up so the loader's pump is cancelled before the
        // test exits (otherwise the task survives the `Tui`
        // drop until the runtime tears down — harmless, but
        // noisier than necessary).
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );

        assert!(
            renders <= 10,
            "loader produced {renders} renders in {} ms; expected ≈4 \
             from a single 80 ms-interval animation pump. A count \
             approaching {} (the throttle cap of {} ms-interval \
             ticks) suggests orphaned animation pumps leaking \
             through the refcount fix.",
            window.as_millis(),
            window.as_millis() / 16,
            16,
        );
    }

    #[test]
    fn set_tools_expanded_flips_every_existing_tool_component() {
        // The pump starts in collapsed mode; appending two
        // finalized tool executions should give us two collapsed
        // components. Flipping the pump must walk the chat
        // container and expand each one.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        assert!(!pump.tools_expanded());

        // Drive two finished tool calls into the chat slot.
        for (i, body_line_count) in [(0usize, 30usize), (1, 25)] {
            let body = (0..body_line_count)
                .map(|n| format!("tool-{i} line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            pump.handle(
                &mut tui,
                &AgentEvent::ToolExecutionStart {
                    agent_id: AgentId::Main,
                    call_id: format!("call-{i}"),
                    tool: "read_file".into(),
                    args: serde_json::json!({}),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::ToolExecutionEnd {
                    agent_id: AgentId::Main,
                    call_id: format!("call-{i}"),
                    tool: "read_file".into(),
                    result: aj_agent::tool::ToolDetails::Text {
                        summary: String::new(),
                        body,
                    },
                    content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                    is_error: false,
                },
            );
        }

        // Helper: count tool components in the chat slot whose
        // rendered body contains the line that's *only* visible in
        // expanded mode (line index >= TEXT_COLLAPSED_LINES = 10).
        let count_expanded = |tui: &mut Tui| -> usize {
            let chat = tui
                .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .expect("chat slot")
                .container_mut();
            let mut n = 0;
            for i in 0..chat.len() {
                if let Some(tool) = chat.get_mut_as::<ToolExecutionComponent>(i) {
                    let lines = tool.render(80);
                    let has_late_line = lines
                        .iter()
                        .any(|l| l.contains("line 20") || l.contains("line 24"));
                    if has_late_line {
                        n += 1;
                    }
                }
            }
            n
        };

        // Sanity: collapsed, no late lines visible.
        assert_eq!(count_expanded(&mut tui), 0);

        pump.set_tools_expanded(&mut tui, true);
        assert!(pump.tools_expanded());
        assert_eq!(
            count_expanded(&mut tui),
            2,
            "both tool components should expose their hidden tail after expand",
        );

        // A redundant set is a no-op (no panic, no rebuild churn).
        pump.set_tools_expanded(&mut tui, true);
        assert!(pump.tools_expanded());

        // And we can flip back.
        pump.set_tools_expanded(&mut tui, false);
        assert!(!pump.tools_expanded());
        assert_eq!(count_expanded(&mut tui), 0);
    }
}
