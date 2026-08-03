//! The chat model: transcripts of typed entries plus the bookkeeping
//! the reducer maintains around them.
//!
//! Everything here is backend-neutral data. Entries are keyed by a
//! stable [`EntryId`], never a container index. Ids are minted
//! monotonically and never reused, so a recorded id either names the
//! entry it was minted for or nothing at all ([`ChatState::quiesce`]
//! drops the unfinalized streaming entries).
//!
//! Durable identity is spelled `Option<String>` throughout: the log
//! entry an entry derives from, or `None` for a row with no durable
//! origin. Nothing here uses an empty string for "no identity", because
//! several rows can carry one and keying on it would alias them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aj_agent::events::{AgentId, AgentSettings, CompactionPhase};
use aj_agent::message::{TaskNotificationKind, TaskOutcome};
use aj_agent::tool::{TaskId, TaskKind, TaskStatus, ToolDetails};
use aj_agent::types::TokenUsage;
use aj_models::registry::ModelInfo;
use aj_models::types::{AssistantMessage, UserContent};
use serde::Serialize;
use serde_json::Value;

use crate::footer::AgentFooters;
use crate::session::AgentLifecycle;

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
    /// A background task's completion notice (see
    /// [`TaskNotificationEntry`]). Distinct from [`EntryKind::Notice`],
    /// which is an unrelated transient UI notice (level + text).
    TaskNotification(TaskNotificationEntry),
}

/// A user message appended from `MessageEnd { User }`.
#[derive(Debug)]
pub struct UserEntry {
    /// Id of the originating user `AgentMessage`, which is also its log
    /// entry id, used to anchor branch operations. `Some` within the
    /// TUI: live messages mint it, replayed messages are backfilled on
    /// resume.
    pub message_id: Option<String>,
    /// The authoritative wire content blocks.
    pub content: Vec<UserContent>,
}

impl UserEntry {
    /// The entry's text blocks joined with `\n`, so legacy multi-block
    /// user messages render as one blob. Image blocks are skipped.
    pub fn joined_text(&self) -> String {
        joined_user_text(&self.content)
    }
}

/// A background task's completion notice, appended from
/// `MessageEnd { TaskNotification }`.
///
/// Carries the structured fields for rich rendering (an outcome-tinted
/// bubble). The [`Self::body`] is the pre-rendered notice text, the
/// same text projected to the model.
#[derive(Debug)]
pub struct TaskNotificationEntry {
    /// Id of the originating notice `AgentMessage`, which is also its
    /// log entry id.
    pub message_id: Option<String>,
    /// Command line (bash) or task description (agent).
    pub label: String,
    /// What kind of work ran.
    pub kind: TaskNotificationKind,
    /// Terminal outcome, which drives the bubble tint.
    pub outcome: TaskOutcome,
    /// Pre-rendered notice body (exit status + output tail, or the
    /// agent report).
    pub body: String,
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
    /// Id of the originating assistant `AgentMessage`, which is also its
    /// log entry id: the durable identity a re-applied `MessageEnd`
    /// updates in place. `None` while the entry is still streaming, since
    /// only `MessageEnd` carries the id the log adopts.
    pub message_id: Option<String>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SubAgentStatus {
    Running,
    /// Finished cleanly.
    Done,
    /// Finished, but the final report hit the model's token cap and is
    /// partial (`SubAgentConclusion::Truncated`).
    Truncated,
    /// The run failed (error or abort). For a live run `report` holds the
    /// failure text. A resumed run shows whatever the failing turn's
    /// terminal message carried, usually empty for a pre-stream error, and
    /// the status is reconstructed from that message's stop reason.
    Failed,
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
    /// Start of the current run: set on `SubAgentStart` and reset on a
    /// continuation re-run, so the runtime measures the active run rather than
    /// the wall-clock since first spawn.
    pub started_at: Instant,
    /// When the sub finished (`SubAgentEnd` or `AgentEnd`), freezing the
    /// displayed runtime. `None` while it is still running. A continuation
    /// re-run clears it (and resets `started_at`) so timing starts fresh.
    pub finished_at: Option<Instant>,
    /// Whether the sub was spawned to run in the background, concurrent with
    /// the parent's turn, rather than blocking it. Set from `SubAgentStart`,
    /// which carries the persisted run mode, so it stays accurate after a
    /// resume.
    pub background: bool,
    /// One-line summary of the sub-agent's most recent live activity: its
    /// last assistant line, or the tool it just started. Shown under a
    /// `Running` box. It stays `None` on a resumed box, which replays no
    /// sub-agent content and shows its report instead, and a `Done` box
    /// ignores it. Never read from the sub's transcript.
    pub latest_activity: Option<String>,
}

/// A completed compaction's summary row.
#[derive(Debug)]
pub struct CompactionEntry {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summary: String,
    /// The compaction checkpoint log entry this row reports on:
    /// `CompactionEnd` carries no identity of its own, so the entry the
    /// event derives from is what makes a re-served checkpoint update
    /// this row instead of adding a second one. `None` for a locally
    /// emitted end (nothing re-serves it), which appends.
    pub entry: Option<String>,
}

/// Severity of a transient notice row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    /// What this notice derives from, when that has durable identity:
    /// the settings log entry behind a projected notice, or the assistant
    /// message behind an in-band error line. Keyed on so a re-served
    /// suffix updates the row it already produced. `None` for every
    /// locally raised notice (warnings, retry cadence, a compaction
    /// failure), which appends.
    pub entry: Option<String>,
}

/// Per-turn token usage row, stored structured so views format it
/// without reparsing.
#[derive(Debug)]
pub struct TurnUsageEntry {
    /// The emitting agent, for the sub-agent line prefix.
    pub agent_id: AgentId,
    pub usage: TokenUsage,
    /// The assistant message this row reports on: its durable identity.
    /// Both the live agent and the log projection emit `UsageUpdate`
    /// directly after that message's `MessageEnd`, so the row belongs to
    /// it and a re-applied update overwrites the row instead of adding
    /// one. `None` for an update that followed no identified message,
    /// which stays append-only.
    pub after_message_id: Option<String>,
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
///
/// "Append-only" holds for the reducer. [`ChatState::quiesce`] is the
/// one operation that drops entries, and it only ever drops the
/// unfinalized streaming assistant entry, whose message is re-rendered
/// from its `MessageEnd`.
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

    /// Drop every entry `keep` rejects.
    ///
    /// A removal keeps the remaining ids monotone, so the binary search
    /// in [`Self::get`] stays valid, and the counter never reuses a
    /// removed id, so a stale [`EntryId`] resolves to `None` forever
    /// rather than to a different entry.
    ///
    /// Every owner of an [`EntryId`] still owes its own bookkeeping, and
    /// that inventory is per-owner rather than one list: the reducer's
    /// own indexes (see [`ChatState::quiesce`], the only caller), and on
    /// the TUI side the image store and the render / text caches, which
    /// key on [`EntryId`] as well. Those are inert under a removal,
    /// because ids are never reused, so a stale key simply never
    /// resolves. What a removal does shift is anything keyed on the
    /// *positional* index, which is how a transcript view's focus cursor
    /// addresses rows. Reconciling that cursor is a client-side concern
    /// (phase 1d), not this method's.
    pub(crate) fn retain(&mut self, keep: impl FnMut(&Entry) -> bool) {
        self.entries.retain(keep);
    }
}

/// Per-agent streaming bookkeeping. One per agent, so streaming events
/// route to the right entry inside that agent's own transcript.
///
/// Three of the four fields are the agent's durable-identity state: the
/// two indexes and the last finalized assistant id. They outlive a turn
/// so a re-applied event finds the entry it already produced instead of
/// appending a second one. Streaming state
/// ([`Self::current_assistant`]) is per-turn and cleared on `AgentEnd`.
#[derive(Debug, Default)]
pub struct AgentRender {
    /// The in-flight assistant entry for this agent, or `None` between
    /// turns.
    ///
    /// The lazy materialization this drives (the first painting
    /// `MessageUpdate` opens the entry) rests on a stream contract: no
    /// `MessageUpdate` for a message may arrive after that message's
    /// `MessageEnd`, or after a backfill finalized it. Otherwise the
    /// update opens a second, unfinalized copy of a message that is
    /// already rendered, and nothing later removes it. Live flow honors
    /// this because the accumulator emits the updates before the end.
    /// Across an attach boundary it is the server's job: spec 6.5 drops
    /// the lossy frames that were in flight when the attach was served,
    /// exactly so a stale snapshot cannot land after the backfill.
    pub(crate) current_assistant: Option<EntryId>,
    /// `tool_use_id` -> the tool entry it maps to, in this agent's
    /// transcript.
    pub(crate) tool_index: HashMap<String, EntryId>,
    /// Durable message id -> the entry it produced, covering user,
    /// finalized assistant and task-notification rows.
    pub(crate) message_index: HashMap<String, EntryId>,
    /// Durable id of the last finalized assistant message, which is the
    /// message a following `UsageUpdate` reports on.
    pub(crate) last_finalized_assistant: Option<String>,
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
}

/// A known agent, snapshotted for the agent picker: the main agent or a
/// sub-agent with the task that spawned it and its run status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEntry {
    pub id: AgentId,
    /// The sub-agent's task description. `None` for the main agent.
    pub task: Option<String>,
    /// The sub-agent's run status. `None` for the main agent.
    pub status: Option<SubAgentStatus>,
    /// Elapsed run time, frozen at the sub's end for a finished sub.
    /// `None` for the main agent.
    pub runtime: Option<Duration>,
    /// Whether the sub runs as a background task (concurrent with the
    /// parent) rather than blocking the parent's turn. Sourced from the
    /// sub's `SubAgentStart`, so it is accurate live and after a resume.
    /// `false` for the main agent.
    pub background: bool,
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
    pub show_thinking_block: bool,
    /// Whether the inline per-turn token-usage rows are visible. Usage
    /// is always recorded, this only gates its display.
    pub show_token_usage: bool,
    /// Compact transcript: tool cells render header-only (bash keeps its
    /// command line). The `tools_expanded` override still reveals full
    /// bodies, so it stays a hidden escape hatch under this mode.
    pub compact_transcript: bool,
    pub tools_expanded: bool,
    pub show_image_in_terminal: bool,
    /// Whether fenced code blocks in rendered markdown are syntax-highlighted.
    pub syntax_highlight: bool,
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
            show_thinking_block: true,
            show_token_usage: true,
            compact_transcript: false,
            tools_expanded: false,
            show_image_in_terminal: true,
            syntax_highlight: false,
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

    /// Whether the active view holds any real conversation, meaning at least
    /// one user or assistant entry.
    ///
    /// Leading `Notice` rows (the startup config, restore, and auth
    /// diagnostics) are chrome, not conversation, so a transcript that holds
    /// only notices still reads as empty. The empty-state splash gate reads
    /// this to choose between the splash and the transcript view.
    pub fn has_conversation(&self) -> bool {
        self.transcripts.get(&self.active_view).is_some_and(|t| {
            t.entries()
                .iter()
                .any(|e| matches!(e.kind, EntryKind::User(_) | EntryKind::Assistant(_)))
        })
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

    /// Snapshot of every known agent for the agent picker: the main
    /// agent first, then each sub-agent in ascending index order with
    /// the task that spawned it and its run status.
    ///
    /// A sub-agent is "known" once its parent transcript holds its box
    /// entry (recorded in `sub_boxes` at `SubAgentStart`). The task and
    /// status are read from that [`SubAgentEntry`], which the reducer
    /// keeps current.
    pub fn agents(&self) -> Vec<AgentEntry> {
        // A single `now` for every still-running sub's elapsed time, so the
        // snapshot's runtimes are consistent with each other.
        let now = Instant::now();
        let mut out = vec![AgentEntry {
            id: AgentId::Main,
            task: None,
            status: None,
            runtime: None,
            background: false,
        }];
        let mut subs: Vec<usize> = self.sub_boxes.keys().copied().collect();
        subs.sort_unstable();
        for n in subs {
            let Some(&(parent, id)) = self.sub_boxes.get(&n) else {
                continue;
            };
            if let Some(entry) = self.transcripts.get(&parent).and_then(|t| t.get(id))
                && let EntryKind::SubAgent(sub) = &entry.kind
            {
                out.push(AgentEntry {
                    id: AgentId::Sub(n),
                    task: Some(sub.task.clone()),
                    status: Some(sub.status),
                    runtime: Some(
                        sub.finished_at
                            .unwrap_or(now)
                            .duration_since(sub.started_at),
                    ),
                    background: sub.background,
                });
            }
        }
        out
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

    /// Conclude the `Sub(n)` box after its run ended without a final
    /// conclusion event: a still-running box flips to `Done` and
    /// freezes its runtime clock. A box already carrying a
    /// `SubAgentEnd` conclusion (`Done`/`Truncated`/`Failed`) and a
    /// missing box are left untouched.
    ///
    /// A conclusion is therefore stable under a re-delivered lifecycle
    /// bracket with one exception, which is the point of that exception:
    /// `AgentStart(Sub n)` re-opens a `Done` box for a continuation
    /// re-run, and this concludes that re-run as `Done` again.
    pub fn conclude_sub_box(&mut self, n: usize) {
        if let Some(b) = self.sub_box_mut(n)
            && b.status == SubAgentStatus::Running
        {
            b.status = SubAgentStatus::Done;
            b.finished_at = Some(Instant::now());
        }
    }

    /// Re-open the `Sub(n)` box for a new run of a sub we already have a
    /// box for: a `Done` box flips back to `Running` and its runtime clock
    /// starts over. A missing box and one that is already running are left
    /// untouched.
    ///
    /// Only a `Done` box re-opens, because that is the genuine
    /// continuation case. A `Truncated` or `Failed` conclusion is terminal,
    /// and re-opening it would let the new run's plain `AgentEnd` conclude
    /// the box `Done` and quietly rewrite a failure into a success.
    pub(crate) fn reopen_sub_box(&mut self, n: usize) {
        if let Some(b) = self.sub_box_mut(n)
            && b.status == SubAgentStatus::Done
        {
            b.status = SubAgentStatus::Running;
            // The clock restarts so the runtime times the new run, not the
            // wall-clock since first spawn.
            b.started_at = Instant::now();
            b.finished_at = None;
        }
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

    /// Resolve task `id`'s launch cell: the owner plus the cell's entry
    /// id in the owner's transcript.
    ///
    /// Keyed on the launching `call_id` through the owner's `tool_index`,
    /// which outlives the turn, so a background task that runs past its
    /// turn stays routable. `None` for a task whose launching call has no
    /// cell: an agent-kind task (the `agent` tool renders as a sub-agent
    /// box), or a client that never saw the call's bracket. The lookup is
    /// live rather than snapshotted at `TaskStart`, so a cell that shows
    /// up afterwards (a late `ToolExecutionEnd` building it on miss)
    /// starts receiving the task's output.
    pub(crate) fn task_cell(&self, id: TaskId) -> Option<(AgentId, EntryId)> {
        let info = self.tasks.get(&id)?;
        let cell = self
            .render
            .get(&info.owner)?
            .tool_index
            .get(&info.call_id)
            .copied()?;
        Some((info.owner, cell))
    }

    /// Drop the transient-derived in-flight detail before a re-attach
    /// backfill is applied (spec 6.5's re-attach reconciliation).
    ///
    /// This clears transient *detail* and never durable identity or
    /// structure. The unfinalized streaming assistant entry goes, since
    /// the message it renders concludes either in the suffix or live. A
    /// running tool cell stays, keeping its `call_id`, tool name and
    /// arguments, and loses only the partial result the lossy
    /// `ToolExecutionUpdate` painted: a tool that has not finished has no
    /// log entry, so no backfill would ever regenerate the cell, and
    /// dropping it would lose its arguments for good. A running
    /// sub-agent box keeps its status, report and child transcript and
    /// loses its activity line, because concluding it would show a
    /// finished box for a live sub and dropping it would orphan the child
    /// transcript. The host is authoritative for concluding sub boxes.
    ///
    /// Deliberately kept: `last_finalized_assistant`. It is what anchors
    /// the trailing `UsageUpdate` of a re-served assistant entry, whose
    /// own durable frame the cursor invariant drops, so clearing it would
    /// grow a second usage row on every re-attach.
    ///
    /// NOTE: clearing `compaction_phase` costs the phase label of a
    /// compaction that is still running. Nothing re-seeds it: the `state`
    /// frame carries `working`, which is already true during a compaction
    /// (it runs as a tracked turn), so only the label is missing and only
    /// until the next `CompactionProgress`. Surfacing it would be a client
    /// concern, not a wire one.
    pub fn quiesce(&mut self, lifecycle: &mut AgentLifecycle) {
        for transcript in self.transcripts.values_mut() {
            // The streaming entry is the only transcript structure that
            // is purely transient: it carries no durable identity and no
            // index names it, so dropping it needs no bookkeeping beyond
            // the streaming slot below.
            transcript
                .retain(|entry| !matches!(&entry.kind, EntryKind::Assistant(a) if !a.finalized));
            for entry in &mut transcript.entries {
                if let EntryKind::Tool(tool) = &mut entry.kind
                    && tool.status == ToolStatus::Running
                {
                    tool.details = None;
                    tool.content = Arc::from(Vec::<UserContent>::new());
                }
            }
        }
        for render in self.render.values_mut() {
            render.current_assistant = None;
        }

        for n in self.sub_boxes.keys().copied().collect::<Vec<_>>() {
            if let Some(b) = self.sub_box_mut(n)
                && b.status == SubAgentStatus::Running
            {
                b.latest_activity = None;
            }
        }

        self.compaction_phase.clear();
        for agent in lifecycle.compacting_agents() {
            lifecycle.clear_compacting(agent);
        }
    }

    /// Drop everything the frame stream built, keeping only client-local
    /// view configuration.
    ///
    /// A client calls this when it adopts a different epoch for the session
    /// (a head switch to another branch, a host restart, spec 6.5).
    /// Nothing it derived from the old epoch's entries describes the
    /// history it is about to be served, so the fold restarts from the full
    /// backfill.
    ///
    /// The display flags and the model catalog survive: they come from
    /// config, not from the stream. Main's footer keeps its settings seed,
    /// which the stream never wrote either: a client reads the host's
    /// active settings off the `state` frame. Every other agent's footer
    /// goes with its transcript, and Main's occupancy numerator goes with
    /// the turns it measured.
    pub fn reset(&mut self, lifecycle: &mut AgentLifecycle) {
        self.transcripts.clear();
        self.transcripts
            .insert(AgentId::Main, Transcript::default());
        self.active_view = AgentId::Main;
        self.render.clear();
        self.tasks.clear();
        self.sub_boxes.clear();
        self.compaction_phase.clear();
        self.footers.retain_main();
        for agent in lifecycle.running_agents() {
            lifecycle.mark_idle(agent);
        }
        for agent in lifecycle.compacting_agents() {
            lifecycle.clear_compacting(agent);
        }
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
            entry: None,
        }));
        let b = t.append(EntryKind::Notice(NoticeEntry {
            level: NoticeLevel::Warning,
            text: "two".into(),
            entry: None,
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

    fn chat_state() -> ChatState {
        ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )
    }

    #[test]
    fn has_conversation_ignores_leading_notices() {
        let mut chat = chat_state();
        assert!(!chat.has_conversation(), "a fresh model is empty");

        chat.transcripts
            .get_mut(&AgentId::Main)
            .expect("main transcript")
            .append(EntryKind::Notice(NoticeEntry {
                level: NoticeLevel::Warning,
                text: "startup warning".into(),
                entry: None,
            }));
        assert!(
            !chat.has_conversation(),
            "leading notices are chrome, not conversation"
        );

        chat.transcripts
            .get_mut(&AgentId::Main)
            .expect("main transcript")
            .append(EntryKind::User(UserEntry {
                message_id: None,
                content: Vec::new(),
            }));
        assert!(
            chat.has_conversation(),
            "a user entry after the notices counts as conversation"
        );
    }
}
