//! The chat model: transcripts of typed entries plus the bookkeeping
//! the reducer maintains around them.
//!
//! Everything here is backend-neutral data. Entries are keyed by a
//! stable [`EntryId`], never a container index, and transcripts only
//! append within a session, so recorded ids stay valid for the
//! session's lifetime.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use aj_agent::events::{AgentId, AgentSettings, CompactionPhase};
use aj_agent::tool::{TaskId, TaskKind, TaskStatus, ToolDetails};
use aj_agent::types::TokenUsage;
use aj_models::registry::ModelInfo;
use aj_models::types::{AssistantMessage, UserContent};
use serde_json::Value;

use crate::footer::AgentFooters;

/// Opaque, per-transcript entry id, stable for the session.
///
/// Minted from a monotonically increasing counter, so ids within one
/// transcript are ordered by append position. Cheap and future-proof
/// against any later reordering, and it reads clearly in the
/// bookkeeping maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(u64);

/// One transcript row: a stable id plus the typed payload.
#[derive(Debug)]
pub struct Entry {
    pub id: EntryId,
    pub kind: EntryKind,
}

/// The typed payload of a transcript entry.
#[derive(Debug)]
pub enum EntryKind {
    User(UserEntry),
    Assistant(AssistantEntry),
    Tool(ToolEntry),
    SubAgent(SubAgentEntry),
    Compaction(CompactionEntry),
    Notice(NoticeEntry),
    TurnUsage(TurnUsageEntry),
}

/// A user message appended from `MessageEnd { User }`.
#[derive(Debug)]
pub struct UserEntry {
    /// The authoritative wire content blocks.
    pub content: Vec<UserContent>,
    /// True for harness task-completion notices (text starts with the
    /// task-notification tag). The view renders these collapsible
    /// under the tools-expand flag instead of dumping the whole
    /// notice.
    pub collapsible: bool,
}

impl UserEntry {
    /// The entry's text blocks joined with `\n`, so legacy multi-block
    /// user messages render as one blob. Image blocks are skipped.
    pub fn joined_text(&self) -> String {
        joined_user_text(&self.content)
    }
}

/// Join a user message's text blocks with `\n`, skipping images.
pub(crate) fn joined_user_text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An assistant message, streamed or replayed.
#[derive(Debug)]
pub struct AssistantEntry {
    /// The latest `AssistantMessage` snapshot. `MessageUpdate` carries
    /// a cumulative `partial: AssistantMessage`, so the reducer stores
    /// the snapshot rather than replaying deltas. On `MessageEnd` this
    /// is the finalized message.
    ///
    /// Redacted thinking needs no reducer-side transformation: the
    /// snapshot carries each `ThinkingContent` with its `redacted`
    /// flag, and the view formats the placeholder at draw time.
    pub message: AssistantMessage,
    pub finalized: bool,
}

/// Execution status of a tool cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done { is_error: bool },
}

/// One tool call's transcript cell, from start (or a replay's
/// build-on-miss) through its finalized result.
#[derive(Debug)]
pub struct ToolEntry {
    /// `tool_use_id` correlating the streaming tool events.
    pub call_id: String,
    pub tool: String,
    /// Validated call arguments. An empty object on the
    /// replay/build-on-miss path, where the `End` event carries none.
    pub args: Value,
    pub status: ToolStatus,
    /// Structured payload: a cumulative partial while running, the
    /// final result once done. `None` until the first update or the
    /// result lands (a freshly started call has only `tool` + `args`).
    pub details: Option<ToolDetails>,
    /// Wire content blocks (for inline images). Shared with the event
    /// payload, so storing it is a refcount bump, not a deep copy of
    /// image bytes.
    pub content: Arc<[UserContent]>,
    /// Set when a background task attaches to this cell. The badge's
    /// terminal status comes from [`ChatState::tasks`] when tracked,
    /// or from the `ToolDetails::Bash` payload's `task_id` on a
    /// resumed cell with no task tracking.
    pub task: Option<TaskId>,
    /// Sub-agent tools render header-only unless the sub is being
    /// observed. A display hint the view reads. The reducer sets it
    /// from `active_view` at append time and
    /// [`ChatState::set_active_view`] reconciles it on a view switch.
    pub header_only: bool,
}

/// Run status of a sub-agent box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
}

/// The parent-transcript box representing a sub-agent run. The sub's
/// own entries live in the `Sub(child)` transcript.
#[derive(Debug)]
pub struct SubAgentEntry {
    /// The `n` in `Sub(n)`. The transcript key is `AgentId::Sub(child)`.
    pub child: usize,
    /// Task description supplied by the parent.
    pub task: String,
    pub status: SubAgentStatus,
    /// Final report, set on `SubAgentEnd`.
    pub report: Option<String>,
}

/// A completed compaction's summary row.
#[derive(Debug)]
pub struct CompactionEntry {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summary: String,
}

/// Severity of a transient notice row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// A transient notice line (info, warning, error, retry cadence).
#[derive(Debug)]
pub struct NoticeEntry {
    pub level: NoticeLevel,
    pub text: String,
}

/// Per-turn token usage row, stored structured so views format it
/// without reparsing.
#[derive(Debug)]
pub struct TurnUsageEntry {
    /// The emitting agent, for the sub-agent line prefix.
    pub agent_id: AgentId,
    pub usage: TokenUsage,
}

impl TurnUsageEntry {
    /// Render the `Token Usage - ...` line for this turn. Sub-agents
    /// get a leading `(sub agent N)` tag so their per-turn counts stay
    /// distinguishable in a shared scrollback.
    pub fn line(&self) -> String {
        format_turn_usage_line(self.agent_id, &self.usage)
    }
}

/// Render the `Token Usage - ...` line for a single `UsageUpdate`.
fn format_turn_usage_line(agent_id: AgentId, usage: &TokenUsage) -> String {
    // `format_tokens` renders the accumulated total bare when the turn
    // contributed nothing (e.g. a cached read of an existing tool
    // result), or `acc+turn` so the per-turn delta is visible at a
    // glance.
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

/// One agent's transcript: an append-only list of entries.
#[derive(Debug, Default)]
pub struct Transcript {
    pub(crate) entries: Vec<Entry>,
    /// Mints the next [`EntryId`].
    next_id: u64,
}

impl Transcript {
    /// The entries in append order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Append `kind` and return the minted id.
    pub fn append(&mut self, kind: EntryKind) -> EntryId {
        let id = EntryId(self.next_id);
        self.next_id += 1;
        self.entries.push(Entry { id, kind });
        id
    }

    /// Entry by id, if present.
    pub fn get(&self, id: EntryId) -> Option<&Entry> {
        // Ids are minted monotonically and entries only append, so the
        // vector is sorted by id and a binary search suffices.
        self.entries
            .binary_search_by_key(&id, |e| e.id)
            .ok()
            .map(|i| &self.entries[i])
    }

    /// Mutable entry by id, if present.
    pub fn get_mut(&mut self, id: EntryId) -> Option<&mut Entry> {
        self.entries
            .binary_search_by_key(&id, |e| e.id)
            .ok()
            .map(|i| &mut self.entries[i])
    }
}

/// Per-agent streaming bookkeeping. One per agent, so streaming events
/// route to the right entry inside that agent's own transcript.
#[derive(Debug, Default)]
pub struct AgentRender {
    /// The in-flight assistant entry for this agent, or `None` between
    /// turns.
    pub(crate) current_assistant: Option<EntryId>,
    /// `tool_use_id` -> the tool entry it maps to, in this agent's
    /// transcript.
    pub(crate) tool_index: HashMap<String, EntryId>,
}

/// One background task tracked from `TaskStart` / `TaskEnd`. Drives a
/// footer's task count, a picker's task rows, and the routing of task
/// events to the launching tool call's transcript cell.
#[derive(Debug)]
pub struct TaskInfo {
    pub kind: TaskKind,
    /// Display label: the command line for bash tasks, the task
    /// description for agent-backed ones.
    pub label: String,
    /// The agent that launched the task. Its transcript holds the
    /// launch cell.
    pub owner: AgentId,
    /// `tool_use_id` of the originating tool call, correlating task
    /// events with the cell.
    pub call_id: String,
    pub status: TaskStatus,
    /// When the reducer saw `TaskStart`, for a picker's runtime column.
    pub started_at: Instant,
    /// When the reducer saw `TaskEnd`, freezing the displayed
    /// runtime.
    pub finished_at: Option<Instant>,
    /// The launch cell in the owner's transcript, snapshotted at
    /// `TaskStart`. The owner's `tool_index` is cleared on its
    /// `AgentEnd`, but a background task outlives the turn, and this
    /// snapshot is what keeps `TaskOutput` / `TaskEnd` routable
    /// afterwards. `None` when the launching call has no cell (the
    /// `agent` tool renders as a sub-agent box, not a tool cell).
    pub cell: Option<EntryId>,
}

/// The chat view's data model. Mutated only by [`crate::chat::reduce`]
/// and the explicit setters here. Views read it at draw time.
pub struct ChatState {
    /// Per-agent transcripts. Main is always present, a `Sub(n)`
    /// transcript is created on `SubAgentStart`. A sub-agent's entries
    /// live in its own transcript. The parent transcript holds a
    /// [`SubAgentEntry`] pointing at it.
    pub(crate) transcripts: HashMap<AgentId, Transcript>,
    /// Which agent's transcript the chat view currently shows.
    pub(crate) active_view: AgentId,
    /// Per-agent streaming bookkeeping.
    pub(crate) render: HashMap<AgentId, AgentRender>,
    /// Background tasks, keyed by task id. Entries are kept (with
    /// their terminal status) after `TaskEnd` so a picker's "all"
    /// scope can list finished tasks. Task events are transient, so a
    /// resumed session starts with an empty map.
    pub(crate) tasks: BTreeMap<TaskId, TaskInfo>,
    /// Per-agent footer store (model line + context occupancy).
    pub(crate) footers: AgentFooters,
    /// Model catalog, for resolving a settings identity's context
    /// window.
    pub(crate) catalog: Arc<Vec<ModelInfo>>,
    /// Locates the `Sub(n)` box entry: the parent transcript that
    /// holds it plus the entry id inside it.
    pub(crate) sub_boxes: HashMap<usize, (AgentId, EntryId)>,
    /// Last reported phase of an in-flight compaction, present only
    /// after the first `CompactionProgress`. Whether a compaction is
    /// in flight at all is the lifecycle's `compacting` set. A
    /// compacting agent with no entry here is still in its starting
    /// phase.
    pub(crate) compaction_phase: HashMap<AgentId, CompactionPhase>,

    /// Display flags the view reads at draw time. Flipping one is a
    /// redraw, not a walk of entries.
    pub hide_thinking_block: bool,
    pub tools_expanded: bool,
    pub show_image_in_terminal: bool,
}

impl ChatState {
    /// Build a fresh model seeded with the Main agent's settings and
    /// context window (the footer seed) and the model catalog used to
    /// resolve sub-agent context windows. Display flags start at their
    /// defaults. The host overrides them from config.
    pub fn new(
        main_settings: AgentSettings,
        main_context_window: u64,
        catalog: Arc<Vec<ModelInfo>>,
    ) -> Self {
        let mut transcripts = HashMap::new();
        transcripts.insert(AgentId::Main, Transcript::default());
        Self {
            transcripts,
            active_view: AgentId::Main,
            render: HashMap::new(),
            tasks: BTreeMap::new(),
            footers: AgentFooters::new(main_settings, main_context_window),
            catalog,
            sub_boxes: HashMap::new(),
            compaction_phase: HashMap::new(),
            hide_thinking_block: false,
            tools_expanded: false,
            show_image_in_terminal: true,
        }
    }

    /// The transcript for `id`, if one exists.
    pub fn transcript(&self, id: AgentId) -> Option<&Transcript> {
        self.transcripts.get(&id)
    }

    /// The agent whose transcript the chat view currently shows.
    pub fn active_view(&self) -> AgentId {
        self.active_view
    }

    /// Switch the viewed transcript to `id`, reconciling the
    /// `header_only` hints: a sub-agent's tool entries render
    /// header-only exactly when that sub is not the active full view.
    pub fn set_active_view(&mut self, id: AgentId) {
        let previous = self.active_view;
        self.active_view = id;
        for agent in [previous, id] {
            if !matches!(agent, AgentId::Sub(_)) {
                continue;
            }
            let header_only = agent != id;
            if let Some(transcript) = self.transcripts.get_mut(&agent) {
                for entry in &mut transcript.entries {
                    if let EntryKind::Tool(tool) = &mut entry.kind {
                        tool.header_only = header_only;
                    }
                }
            }
        }
    }

    /// The tracked background tasks, in id order.
    pub fn tasks(&self) -> &BTreeMap<TaskId, TaskInfo> {
        &self.tasks
    }

    /// The per-agent footer store.
    pub fn footers(&self) -> &AgentFooters {
        &self.footers
    }

    /// Mutable footer store, for host-side reconciliation (settings
    /// changes flow from the host, not the event stream).
    pub fn footers_mut(&mut self) -> &mut AgentFooters {
        &mut self.footers
    }

    /// Last reported phase of `id`'s in-flight compaction, `None`
    /// while it is still in its starting phase (or not compacting at
    /// all, check the lifecycle's `compacting` set for that).
    pub fn compaction_phase(&self, id: AgentId) -> Option<CompactionPhase> {
        self.compaction_phase.get(&id).copied()
    }

    /// Resolve the context window for a settings identity known only
    /// as `(provider, model_id)` strings:
    ///
    /// 1. Catalog scan. The catalog is the authoritative source and
    ///    is loaded once at startup.
    /// 2. On a miss, an identity equal to the Main entry's settings
    ///    resolves to Main's window. This covers scripted runs and
    ///    `--model-url` bundles absent from the catalog: sub-agents
    ///    inherit the parent's bundle, so the identity match is exact
    ///    in practice.
    /// 3. Otherwise `0`, which suppresses the indicator.
    pub fn resolve_window(&self, settings: &AgentSettings) -> u64 {
        if let Some(info) = self
            .catalog
            .iter()
            .find(|m| m.provider == settings.provider && m.id == settings.model_id)
        {
            return info.context_window;
        }
        if let Some(main) = self.footers.settings(AgentId::Main)
            && main.provider == settings.provider
            && main.model_id == settings.model_id
        {
            return self.footers.context_usage(AgentId::Main).context_window;
        }
        0
    }

    /// Whether a freshly appended tool entry for `agent_id` should
    /// render header-only: sub-agent tools live inside the compact
    /// box unless that sub is the observed full view.
    pub(crate) fn header_only_for(&self, agent_id: AgentId) -> bool {
        matches!(agent_id, AgentId::Sub(_)) && self.active_view != agent_id
    }

    /// Mutable access to the `Sub(n)` box entry, if one exists.
    pub(crate) fn sub_box_mut(&mut self, n: usize) -> Option<&mut SubAgentEntry> {
        let &(parent, id) = self.sub_boxes.get(&n)?;
        match &mut self.transcripts.get_mut(&parent)?.get_mut(id)?.kind {
            EntryKind::SubAgent(entry) => Some(entry),
            _ => None,
        }
    }

    /// Mutable access to the tool entry at `id` in `owner`'s
    /// transcript, if present and actually a tool cell.
    pub(crate) fn tool_entry_mut(&mut self, owner: AgentId, id: EntryId) -> Option<&mut ToolEntry> {
        match &mut self.transcripts.get_mut(&owner)?.get_mut(id)?.kind {
            EntryKind::Tool(entry) => Some(entry),
            _ => None,
        }
    }

    /// Resolve task `id`'s launch cell: the owner plus the cell's
    /// entry id in the owner's transcript. Prefers the live
    /// `tool_index` (by `call_id`) and falls back to the id
    /// snapshotted at `TaskStart`, which is what survives the owner's
    /// `AgentEnd` clearing its tool bookkeeping.
    pub(crate) fn task_cell(&self, id: TaskId) -> Option<(AgentId, EntryId)> {
        let info = self.tasks.get(&id)?;
        let cell = self
            .render
            .get(&info.owner)
            .and_then(|r| r.tool_index.get(&info.call_id))
            .copied()
            .or(info.cell)?;
        Some((info.owner, cell))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a TokenUsage carrying the supplied per-turn deltas and
    /// the running accumulator state observed before this turn was
    /// folded in (`already`), matching the wire semantic on
    /// `AgentEvent::UsageUpdate`.
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

    #[test]
    fn format_turn_usage_line_emits_acc_plus_turn_for_main_agent() {
        // First turn: the accumulator is still zero, so each field
        // prints `0+turn`.
        let usage = token_usage([100, 50, 30, 5], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 0+100 | Output: 0+50 | Cache Creation: 0+30 | Cache Read: 0+5",
        );
    }

    #[test]
    fn format_turn_usage_line_drops_turn_part_when_turn_is_zero() {
        // The `+turn` suffix is hidden when the turn contributed
        // nothing, so routine cache hits don't show `+0` rows.
        let usage = token_usage([0, 0, 0, 0], [200, 80, 0, 14]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 200 | Output: 80 | Cache Creation: 0 | Cache Read: 14",
        );
    }

    #[test]
    fn format_turn_usage_line_prefixes_sub_agent_id() {
        let usage = token_usage([10, 5, 1, 0], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Sub(2), &usage);
        assert_eq!(
            line,
            "(sub agent 2) Token Usage - Input: 0+10 | Output: 0+5 | Cache Creation: 0+1 | Cache Read: 0",
        );
    }

    #[test]
    fn transcript_lookup_by_id_survives_appends() {
        let mut t = Transcript::default();
        let a = t.append(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Info,
            text: "one".into(),
        }));
        let b = t.append(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Warning,
            text: "two".into(),
        }));
        assert_ne!(a, b);
        match &t.get(a).expect("entry a").kind {
            EntryKind::Notice(n) => assert_eq!(n.text, "one"),
            other => panic!("unexpected kind: {other:?}"),
        }
        match &t.get_mut(b).expect("entry b").kind {
            EntryKind::Notice(n) => assert_eq!(n.text, "two"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }
}
