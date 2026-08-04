//! Replay a persisted [`ConversationLog`](crate::log::ConversationLog)
//! as an iterator of typed [`AgentEvent`]s.
//!
//! Resuming a session should look the same to a frontend as a live
//! run: the renderer consumes a single typed event stream regardless
//! of whether the events came from a running agent or from a
//! previously recorded log on disk. `replay` is the bridge between
//! disk and that pipeline.
//!
//! The binary-side wiring: the `aj` binary opens a log, registers
//! persistence and renderer listeners on the agent, then drains
//! `replay(...)` into the renderer pipeline before entering its
//! input loop.
//!
//! ## Mapping
//!
//! Each persisted [`ConversationEntryKind`] maps to zero or more
//! [`AgentEvent`]s, tagged with an [`AgentId`] derived from the
//! entry's [`ThreadKind`] / `agent_id` framing so the bridge listener
//! routes main-agent and sub-agent activity to the right renderers:
//!
//! - [`ConversationEntryKind::SystemPrompt`]: model-facing metadata,
//!   not user-visible. No event.
//! - [`ConversationEntryKind::Message`] (assistant): one
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair
//!   wrapping the projected [`AssistantMessage`], followed by an
//!   [`AgentEvent::UsageUpdate`] carrying the per-turn `usage`
//!   recorded on the assistant message and a running
//!   accumulated total. Listeners (the TUI footer, end-of-session
//!   summaries) therefore see the same shape on resume as on a
//!   live turn. Renderers walk the finalized content blocks on
//!   `MessageEnd` to paint text/thinking/tool-call blocks; no
//!   per-block streaming events are synthesized (replay has no
//!   deltas to stream). Each tool_call updates an internal
//!   `tool_call_id ↦ (tool_name, args)` map used to label the
//!   matching tool result later.
//! - [`ConversationEntryKind::Message`] (user): one
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair.
//! - [`ConversationEntryKind::Message`] (tool_result): one
//!   [`AgentEvent::ToolExecutionStart`] / [`ToolExecutionEnd`] pair
//!   pulling the tool name & input args from the tracking map. The
//!   structured `ToolDetails` payload is resolved through the session
//!   codec. Text body references are hydrated from content, while normal
//!   details use regular deserialization. Absent or malformed details fall
//!   back to a text-only synthesis. The
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair
//!   around the tool_result is also emitted so persistence listeners
//!   replaying the stream see the same shape live runs produce.
//! - [`ConversationEntryKind::ModelChange`] /
//!   [`ConversationEntryKind::ThinkingChange`] /
//!   [`ConversationEntryKind::SpeedChange`]: one
//!   [`AgentEvent::Notice`] (`Model set to <provider>/<id>.`, etc.),
//!   but only when at least one `Message` entry precedes the entry
//!   on the same thread. This renders mid-session switches in
//!   resumed scrollback while keeping seed entries (session
//!   creation) silent — they never produced a visible notice live
//!   either.
//! - [`ConversationEntryKind::SubAgentSpawn`]: no notice; the entry
//!   feeds the sub-agent bracketing below.
//! - [`ConversationEntryKind::Compaction`]: a single
//!   [`AgentEvent::CompactionEnd`] marking the boundary, mirroring the
//!   live path so the footer occupancy drops to the reduced size (no
//!   `UsageUpdate` follows a compaction, and the retained tail's usage is
//!   stale). The summarized prefix entries still replay in order, so
//!   the scrollback shows the full history even though the model
//!   context (rebuilt via `agent_messages`) is the reduced projection.
//!
//! Sub-agent runs are bracketed with synthesized
//! [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`]
//! events. A sub thread leads with its `SubAgentSpawn` entry, which
//! carries the task and the child's settings snapshot, so the start
//! event is emitted directly from it. Legacy logs whose sub threads
//! lead with the task user message instead get the start event at
//! that first `Message` entry, with the task taken from its user
//! text and default settings (empty provider/model, thinking "off",
//! speed "standard").
//!
//! ## Live logs
//!
//! [`replay`] treats the log as dead: every sub-agent bracket still open
//! at end of log is force-closed. A live session serving a catch-up
//! backfill needs the same projection with different endings, which is
//! [`project_suffix`]: it emits only the entries after a cursor, tags
//! the durable ones with their append position, and leaves the brackets
//! of the runs the caller knows are still running open.
//!
//! ## Timestamps
//!
//! Replay does not stamp the synthesized events with a wall-clock time,
//! and intentionally so. [`AgentEvent`] carries no timestamp slot, and
//! no frontend renders a per-message "sent at" or a turn duration off
//! the event stream. The authoritative append time lives on
//! [`ConversationEntry::timestamp`] and is read straight off the log by
//! the session-listing surfaces ([`ConversationLog::stats`] and
//! [`crate::persistence::SessionPreview`]), so a resumed session reports
//! the same activity times as a live one without threading anything
//! through replay. The projected wire messages keep their own
//! `timestamp` field as persisted (today the live path leaves it `0`).
//!
//! [`ConversationEntry::timestamp`]: crate::log::ConversationEntry::timestamp
//! [`ConversationLog::stats`]: crate::log::ConversationLog::stats

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use aj_agent::events::{AgentEvent, AgentId, AgentSettings, CompactionReason, SubAgentConclusion};
use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::tool::ToolDetails;
use aj_agent::types::TokenUsage;
use aj_models::types::{AssistantContent, Message, StopReason, Usage, UserContent};
use serde_json::Value;

use crate::compaction::estimate_conversation_context;
use crate::log::{
    Conversation, ConversationEntry, ConversationEntryKind, ConversationLog, EntryId, EntryRef,
    LogSnapshot, ThreadFilter, ThreadKind,
};
use crate::tool_details::resolve_tool_details;

/// One event paired with the log entry it stands for.
///
/// Both origins produce this shape, which is what lets a host fan one
/// stream out without converting: the persisting forwarder tags an event
/// with the entry it just appended, and [`project_suffix`] tags a
/// projected event with the entry it derives from.
///
/// `entry` is `Some` exactly for the events that are durable (spec 6.4),
/// so a client can advance its cursor on the same events whether they
/// arrive live or in a backfill. It is absent for everything a live run
/// emits without persisting, for bracketing frames the projection
/// synthesizes, and for every projected event of an entry at or below the
/// cursor: their entry is already applied, and tagging them would make
/// the client's cursor invariant drop them (spec 6.5).
#[derive(Debug, Clone)]
pub struct TaggedEvent {
    pub entry: Option<EntryRef>,
    pub event: AgentEvent,
}

/// The projected durable suffix of a live log.
#[derive(Debug, Clone)]
pub struct Backfill {
    pub events: Vec<TaggedEvent>,
    /// Every sub-agent run the walk saw on the projected path, whether its
    /// bracket was closed or left open.
    ///
    /// A caller concluding the runs a backfill left unconcluded needs this
    /// rather than the log's full sub-agent set: the latter spans abandoned
    /// branches, whose runs the projection never mentions.
    pub subs: BTreeSet<usize>,
    /// The runs whose bracket the projection left open, which is exactly
    /// the `live_subs` it saw (see [`project_suffix`]). Their real
    /// `SubAgentEnd` is still coming live, so concluding them is the
    /// caller's decision, not the projection's.
    pub open_subs: BTreeSet<usize>,
}

/// Project the events of every entry after `cursor`, for a live log.
///
/// The walk starts at entry 0 regardless of the cursor, so the tool-name
/// map, the usage accumulators, the settings-notice gate and the
/// sub-agent bracketing state are complete: events of entries at or
/// below the cursor are computed and dropped, never skipped. `None`
/// projects the whole log, and so does a cursor beyond the log's
/// `last_seq`, which cannot name a position in this materialization
/// (spec 6.5 treats it as an epoch mismatch).
///
/// `live_subs` names the sub-agents the caller knows are still running.
/// Every other run's bracket is force-closed exactly as dead-log
/// [`replay`] closes it, and theirs is left open for the real
/// `SubAgentEnd` to close live (spec 6.5). A run whose bracket opened at
/// or below the cursor has its `SubAgentStart` re-synthesized before its
/// first emitted event, so the suffix is well-bracketed.
///
/// Which events carry an [`EntryRef`]: exactly the ones a live run
/// persists or synthesizes as durable, so live flow and backfill agree
/// on what a cursor covers. That is the `MessageEnd` of a `Message`
/// entry, the `SubAgentStart` of a `SubAgentSpawn` root, the
/// `CompactionEnd` of a `Compaction` entry, and the `Notice` of a
/// settings entry. At most one event per entry is tagged, which is what
/// makes the client's per-frame cursor advance well-defined.
pub fn project_suffix(
    log: &LogSnapshot,
    cursor: Option<u64>,
    live_subs: &BTreeSet<usize>,
) -> Backfill {
    // This is the one place that knows `last_seq`, so the out-of-range
    // clamp lives here rather than at every caller.
    let cursor = cursor.filter(|seq| *seq <= log.last_seq());
    let mut walk = Replay::suffix(log, cursor, live_subs.clone());
    let events: Vec<TaggedEvent> = walk.by_ref().collect();
    Backfill {
        events,
        subs: walk.seen_subs(),
        open_subs: walk.open_subs(),
    }
}

impl LogSnapshot {
    /// The [`AgentEvent::Notice`] the projection derives from a settings
    /// entry, `None` when it derives none (or when `entry` is not a
    /// settings entry on the active path).
    ///
    /// A settings entry before its thread's first message projects
    /// nothing, so a host that synthesized a notice unconditionally would
    /// emit a live frame that no backfill regenerates. Asking here is
    /// what keeps the two in agreement.
    ///
    /// This runs the projection rather than restating its rule. That is a
    /// walk of the log per call, which a settings change can afford.
    pub fn project_settings_entry(&self, entry: &EntryId) -> Option<AgentEvent> {
        // `live_subs` is irrelevant: it only affects bracketing frames,
        // and those are never tagged with a settings entry.
        project_suffix(self, None, &BTreeSet::new())
            .events
            .into_iter()
            .find_map(|tagged| {
                let tagged_entry = tagged.entry?;
                (tagged_entry.id == *entry && matches!(tagged.event, AgentEvent::Notice { .. }))
                    .then_some(tagged.event)
            })
    }
}

/// Walks `log` in append order and lazily yields its projected [`AgentEvent`]s.
///
/// The iterator borrows `log` until it is dropped. It buffers only events from
/// the current persisted entry, plus events needed to balance an open sub-agent
/// at EOF.
pub fn replay(log: &ConversationLog) -> impl Iterator<Item = AgentEvent> + '_ {
    Replay::new(log.core()).map(|projected| projected.event)
}

/// Like [`replay`], but withholds every sub-agent's projected content
/// events.
///
/// This is the full replay state machine with sub-agent content
/// projection gated off. It runs the same bracketing as [`replay`], so
/// it emits the identical sequence of [`AgentEvent::SubAgentStart`] /
/// [`AgentEvent::SubAgentEnd`] events, with identical reports. The only
/// difference: for an entry whose agent is a sub-agent it does not push
/// the projected `MessageStart`/`End`, `ToolExecution*`, `UsageUpdate`,
/// or `Notice` events. Main-agent entries project exactly as in full
/// replay. A caller reconstructs a sub-agent's withheld content on
/// demand with [`project_thread`].
///
/// This makes resuming a large session cheap. The projection clones
/// every sub-agent message and tool payload into an event, and
/// sub-agent threads dominate a big log, so deferring that work builds
/// the main transcript and the sub-agent boxes without paying for
/// transcripts the user is usually not looking at.
pub fn replay_deferring_subs(log: &ConversationLog) -> impl Iterator<Item = AgentEvent> + '_ {
    Replay::deferring_subs(log.core()).map(|projected| projected.event)
}

/// Project one already-linearized sub-agent thread into that
/// sub-agent's content events, with a fresh projection state.
///
/// `conv` must be the linearization of a single sub-agent thread
/// (`log.linearize(head, ThreadFilter::subagent(n))`) and `agent` the
/// matching [`AgentId::Sub`]. It emits that sub-agent's
/// `MessageStart`/`End`, `ToolExecution*`, `UsageUpdate`, and `Notice`
/// events, equal in order and payload to what full [`replay`] emits for
/// the same thread. It does not emit [`AgentEvent::SubAgentStart`] /
/// [`AgentEvent::SubAgentEnd`] and does not bracket, because the box
/// these events fill already exists.
///
/// Takes an owned [`Conversation`], not a `&ConversationLog`, so the
/// caller can drop the log lock before projecting. This is sound
/// because a sub-agent thread never carries a
/// [`ConversationEntryKind::Compaction`] entry (compaction runs on the
/// user thread only), which is the only projection step that needs the
/// full log.
pub fn project_thread(conv: &Conversation, agent: AgentId) -> Vec<AgentEvent> {
    // A fresh state scoped to this thread reproduces full replay's
    // per-agent projection: the usage accumulator and the
    // settings-notice gate are keyed per `AgentId`, and every entry on
    // this thread is `agent`, so the `UsageUpdate` sequence and `Notice`
    // gating match what full replay produces for this sub-agent.
    debug_assert!(
        matches!(agent, AgentId::Sub(_)),
        "project_thread projects a sub-agent thread"
    );
    let mut state = ReplayState::default();
    let mut out = VecDeque::new();
    for entry in conv.entries() {
        // No bracketing and no log handle: the box exists already, and
        // a sub-agent thread carries no `Compaction` entry.
        //
        // Guard against a misrouted conversation: every entry that carries an
        // agent id must be `agent`. Meta entries (settings, system prompt)
        // carry none and are fine on any thread.
        debug_assert!(
            agent_id_for(entry).is_none_or(|id| id == agent),
            "project_thread received an entry for a different agent"
        );
        state.project_entry(entry, None, None, &mut out);
    }
    out.into_iter().map(|projected| projected.event).collect()
}

struct Replay<'a> {
    log: &'a LogSnapshot,
    next_entry: usize,
    state: ReplayState,
    pending: VecDeque<TaggedEvent>,
    finished: bool,
    /// When set, sub-agent content events are withheld (see
    /// [`replay_deferring_subs`]). Bracketing and report capture are
    /// unaffected.
    defer_subs: bool,
    /// The entry ids to project: the active path from [`LogSnapshot::head`]
    /// plus the sub-agent threads anchored on it. Append-order entries
    /// outside this set are skipped, so sibling branches on disk don't
    /// interleave into the replayed stream. `None` disables filtering
    /// (a log with no user-thread head), preserving the whole-file
    /// behaviour.
    included: Option<HashSet<String>>,
    /// Entries at or below this append position are projected for their
    /// effect on the walk's state and their events dropped, so the walk
    /// yields only the suffix after it. `None` yields everything.
    cursor: Option<u64>,
    /// The sub-agents whose bracket is never force-closed, because the
    /// caller knows they are still running (see [`project_suffix`]).
    /// Dead-log replay leaves this empty and closes every run.
    live_subs: BTreeSet<usize>,
}

impl<'a> Replay<'a> {
    fn new(log: &'a LogSnapshot) -> Self {
        Self::with_mode(log, false)
    }

    fn deferring_subs(log: &'a LogSnapshot) -> Self {
        Self::with_mode(log, true)
    }

    fn with_mode(log: &'a LogSnapshot, defer_subs: bool) -> Self {
        Self {
            log,
            next_entry: 0,
            state: ReplayState::default(),
            pending: VecDeque::new(),
            finished: false,
            defer_subs,
            included: included_entries(log),
            cursor: None,
            live_subs: BTreeSet::new(),
        }
    }

    fn suffix(log: &'a LogSnapshot, cursor: Option<u64>, live_subs: BTreeSet<usize>) -> Self {
        Self {
            cursor,
            live_subs,
            ..Self::with_mode(log, false)
        }
    }

    /// The sub-agent runs whose bracket is still open, valid once the
    /// walk is exhausted.
    fn open_subs(&self) -> BTreeSet<usize> {
        self.state.open_runs.keys().copied().collect()
    }

    /// Every sub-agent run the walk entered, valid once it is exhausted.
    fn seen_subs(&self) -> BTreeSet<usize> {
        self.state.seen_subs.clone()
    }
}

/// Compute the set of entry ids replay should project: the head's
/// ancestor chain on the main thread, plus every sub-agent entry that
/// chains onto that path.
///
/// Returns `None` when the log has no user-thread head, which disables
/// filtering so a headless log replays in full as before.
///
/// Invariant: only sub-agent entries expand forward. Main-thread
/// inclusion is exactly the head's ancestor chain, so sibling branches
/// are excluded. A main-thread entry off the head's chain is never
/// added, which is what keeps an abandoned branch out of the replay.
///
/// Anchoring on parent chains rather than on `agent_id` is what makes
/// this correct under concurrent writers. Two writers that both resume
/// before spawning can mint the same `Sub(n)` id on different branches
/// (the counter is seeded from `max_agent_id` at resume), so the id
/// alone cannot tell an on-path run from an abandoned one. The parent
/// chain can: append order guarantees a sub entry's anchor (its
/// main-thread parent, or an earlier entry of the same run) precedes
/// it, so a single forward pass includes each on-path run completely
/// and never touches a run anchored on an excluded main entry.
///
/// Keying on the parent rather than a [`ConversationEntryKind::SubAgentSpawn`]
/// root also handles legacy logs whose sub threads lead with the task
/// user message: both shapes anchor their first entry on the user
/// thread and chain forward the same way. No transitive closure across
/// sub threads is needed, since a spawned agent has the `agent` tool
/// removed and its thread always anchors on the user thread.
fn included_entries(log: &LogSnapshot) -> Option<HashSet<String>> {
    let head = log.head()?;

    // Main path: walk parent pointers from the head, collecting the
    // head's user and meta ancestors (settings and system-prompt
    // entries chain here too, so they stay included). This is the only
    // source of main-thread inclusion, so sibling branches never enter
    // the set.
    let mut included: HashSet<String> = HashSet::new();
    let mut cursor = Some(head.clone());
    while let Some(id) = cursor {
        let Some(entry) = log.get(&id) else { break };
        included.insert(id.clone());
        cursor = entry.parent_id.clone();
    }

    // Single forward pass in append order: a sub-agent entry joins the
    // set when its parent is already included. A run's first entry
    // anchors on the main path, later entries chain onto earlier ones of
    // the same run, so one pass includes each on-path run whole. Only
    // `Subagent` entries expand here. Expanding user entries too would
    // pull in a sibling branch's first entry, whose parent is a common
    // ancestor on the main path, and leak the abandoned branch.
    for index in 0..log.len() {
        let Some(entry) = log.entry_in_append_order(index) else {
            continue;
        };
        let anchored_on_path = entry.thread == ThreadKind::Subagent
            && entry
                .parent_id
                .as_ref()
                .is_some_and(|parent| included.contains(parent));
        if anchored_on_path {
            included.insert(entry.id.clone());
        }
    }

    Some(included)
}

impl Iterator for Replay<'_> {
    type Item = TaggedEvent;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }

            if self.next_entry < self.log.len() {
                let index = self.next_entry;
                self.next_entry += 1;
                if let Some(entry) = self.log.entry_in_append_order(index) {
                    // Skip entries off the active path (sibling branches,
                    // sub threads anchored on an abandoned branch).
                    // Sub threads are excluded whole, never partially,
                    // so an excluded run is simply never opened and the
                    // bracketing stays balanced.
                    let skip = self
                        .included
                        .as_ref()
                        .is_some_and(|set| !set.contains(&entry.id));
                    if !skip {
                        let seq = u64::try_from(index).expect("log index fits u64") + 1;
                        let keep = self.cursor.is_none_or(|cursor| seq > cursor);
                        // A dropped entry's events may not be tagged: its
                        // position is already applied by whoever offered
                        // the cursor.
                        let at = keep.then(|| EntryRef {
                            seq,
                            id: entry.id.clone(),
                        });
                        let mut projected = VecDeque::new();
                        self.state.bracket_subagent(
                            entry,
                            at.clone(),
                            keep,
                            &self.live_subs,
                            &mut projected,
                        );
                        if self.defer_subs && matches!(agent_id_for(entry), Some(AgentId::Sub(_))) {
                            // Deferred mode withholds a sub-agent's content
                            // events but still advances the report that
                            // `close_run` reads, so the `SubAgentEnd`
                            // matches full replay byte for byte.
                            self.state.capture_sub_report_from_entry(entry);
                        } else {
                            self.state
                                .project_entry(entry, at, Some(self.log), &mut projected);
                        }
                        if keep {
                            self.pending.append(&mut projected);
                        }
                    }
                }
                continue;
            }

            if !self.finished {
                self.finished = true;
                self.state
                    .close_finished_runs(None, &self.live_subs, true, &mut self.pending);
                continue;
            }

            return None;
        }
    }
}

/// Bracket state of one sub-agent run the walk has opened and not yet
/// closed. Keyed per run rather than held as a single "current run":
/// a background sub-agent's entries interleave with its parent's, so
/// several runs can be open at once.
#[derive(Default)]
struct OpenRun {
    /// The run's [`AgentEvent::SubAgentStart`], kept so a suffix whose
    /// cursor falls inside the run can re-synthesize it. `None` until
    /// the run's `SubAgentSpawn` entry, or the legacy fallback at its
    /// first `Message` entry, produces one. A run that reaches its close
    /// with `None` still gets a balanced bracket from a synthesized
    /// start.
    start: Option<AgentEvent>,
    /// Whether `start` reached the output. False while the run opened on
    /// an entry the walk dropped, which is what tells the suffix
    /// projection to re-synthesize the start.
    start_delivered: bool,
    /// Concatenated text of the most recent `Sub` assistant message seen
    /// during the run. After its last assistant message this holds the
    /// final report carried on the closing [`AgentEvent::SubAgentEnd`].
    report: String,
    /// How the run concluded, carried on the closing
    /// [`AgentEvent::SubAgentEnd`]. Derived from the run's last assistant
    /// message stop reason: `Length` -> `Truncated`, `Error`/`Aborted`
    /// and an interrupted `ToolUse` terminal -> `Failed`, otherwise
    /// `Completed`. This reconstructs the conclusion on resume without
    /// any dedicated on-disk entry, because a failed, aborted, or
    /// interrupted run's terminal message is itself persisted with the
    /// matching stop reason.
    conclusion: SubAgentConclusion,
}

/// Per-walk projection state.
#[derive(Default)]
struct ReplayState {
    /// Map of `tool_call_id` ↦ (`tool_name`, `arguments`) populated
    /// from each `ToolCall` we see on assistant messages. Used to
    /// synthesize a matching [`AgentEvent::ToolExecutionStart`]
    /// (carrying the args) and label the
    /// [`AgentEvent::ToolExecutionEnd`] for the corresponding
    /// tool result later in the log.
    tool_calls: HashMap<String, (String, Value)>,
    /// Per-agent accumulated [`Usage`] running totals, used to
    /// build the `accumulated_*` fields on synthesized
    /// [`AgentEvent::UsageUpdate`] events. The map starts empty and
    /// grows on demand the first time we see an assistant message
    /// for an [`AgentId`]; the value stored at `agent_id` is the
    /// accumulator *as observed before* the next turn is emitted,
    /// matching the live agent's event order (see
    /// `aj_agent::Agent::prompt`: `UsageUpdate` carries the
    /// pre-add total, and the per-turn delta is added afterwards).
    usage_accumulators: HashMap<AgentId, Usage>,
    /// The sub-agent runs currently open, by `Sub(n)` index. Ordered so
    /// that closing several at once is deterministic.
    open_runs: BTreeMap<usize, OpenRun>,
    /// Every run the walk has entered, whether still open or already
    /// closed. Only the projected path contributes, which is what a caller
    /// concluding unconcluded runs needs (see [`Backfill::subs`]).
    seen_subs: BTreeSet<usize>,
    /// The [`AgentEvent::SubAgentStart`] each sub-agent's run was opened
    /// with, kept after the run closes.
    ///
    /// A background sub-agent's entries interleave with its parent's, so a
    /// run the walk considers finished is closed at the parent's next entry
    /// and re-opened at the sub's next one, with its spawn root well behind
    /// us. Re-opening from the message alone would lose the task, the
    /// background flag and the settings the root carries, and the client
    /// would seed the child's footer from an empty settings snapshot
    /// (spec 6.5).
    spawned: HashMap<usize, AgentEvent>,
    /// Agents for which at least one `Message` entry has been
    /// projected. Settings entries emit a [`AgentEvent::Notice`]
    /// only for agents present here; seed entries (before any
    /// message on their thread) stay silent.
    seen_message: HashSet<AgentId>,
}

/// Default settings for a synthesized [`AgentEvent::SubAgentStart`]
/// when the run carries no [`ConversationEntryKind::SubAgentSpawn`]
/// entry (legacy logs): empty provider/model, thinking "off", speed
/// "standard".
fn fallback_settings() -> AgentSettings {
    AgentSettings {
        provider: String::new(),
        model_id: String::new(),
        thinking: "off".to_string(),
        thinking_display: String::new(),
        speed: "standard".to_string(),
        verbosity: "default".to_string(),
    }
}

/// Reconstruct a sub-agent run's conclusion from its terminal message's
/// stop reason. `Length` is a token-cap truncation. A failure is `Error`,
/// `Aborted`, or a `ToolUse` terminal: a run only ends on `ToolUse` when it
/// was interrupted mid tool-loop before producing a final answer (a clean
/// run's last assistant message is always `Stop`), which matches the live
/// path mapping such an interruption to `Failed`. `Stop` is a clean
/// completion.
///
/// This is how a resumed session tells a failed, truncated, or interrupted
/// run from a clean one without a dedicated on-disk marker: the terminal
/// message is persisted carrying the matching stop reason.
fn conclusion_from_stop_reason(stop_reason: &StopReason) -> SubAgentConclusion {
    match stop_reason {
        StopReason::Length => SubAgentConclusion::Truncated,
        StopReason::Error | StopReason::Aborted | StopReason::ToolUse => SubAgentConclusion::Failed,
        StopReason::Stop => SubAgentConclusion::Completed,
    }
}

/// Build the synthesized [`AgentEvent::SubAgentStart`] for sub-agent
/// `n`. `background` is the persisted run mode (foreground for legacy
/// logs and for balancing synthetic starts, which carry no mode).
fn sub_start_event(
    n: usize,
    task: String,
    background: bool,
    settings: AgentSettings,
) -> AgentEvent {
    AgentEvent::SubAgentStart {
        parent: AgentId::Main,
        child: AgentId::Sub(n),
        task,
        background,
        settings,
    }
}

/// Tag `event` as the durable frame of the entry it derives from. `at` is
/// `None` when the entry has no known position (single-thread
/// projection) or sits at or below a suffix cursor.
fn durable(at: Option<EntryRef>, event: AgentEvent) -> TaggedEvent {
    TaggedEvent { entry: at, event }
}

/// One projected event that stands for no log entry of its own:
/// bracketing frames and everything a live run emits without persisting.
fn transient(event: AgentEvent) -> TaggedEvent {
    TaggedEvent { entry: None, event }
}

impl ReplayState {
    /// Emit [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`]
    /// correlation events around a sub-agent's run, before the entry's
    /// own events are projected.
    ///
    /// Transitions are keyed off `agent_id_for`: an entry for `Main` or a
    /// different sub closes the runs that [`Self::close_finished_runs`]
    /// considers finished. Entering a `Sub(n)` with no run open opens
    /// one. The run's [`AgentEvent::SubAgentStart`] is emitted from its
    /// `SubAgentSpawn` entry (task + settings snapshot); legacy logs
    /// whose sub threads lead with the task user message instead emit it
    /// at the run's first `Message` entry, with the task from its user
    /// text and default settings. `Meta` entries carry no agent id and
    /// never transition.
    ///
    /// `keep` is false while the walk is dropping this entry's events (it
    /// sits at or below a suffix cursor), which is what makes the run's
    /// start re-synthesizable later. `live_subs` is the set of runs whose
    /// bracket must not be force-closed here.
    fn bracket_subagent(
        &mut self,
        entry: &ConversationEntry,
        at: Option<EntryRef>,
        keep: bool,
        live_subs: &BTreeSet<usize>,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        let Some(current) = agent_id_for(entry) else {
            return;
        };

        let current_sub = match current {
            AgentId::Sub(n) => Some(n),
            AgentId::Main => None,
        };
        self.close_finished_runs(current_sub, live_subs, keep, out);

        let Some(n) = current_sub else {
            return;
        };
        self.seen_subs.insert(n);
        if self.open_runs.entry(n).or_default().start.is_some() {
            // The run may have opened on an entry this walk dropped, in
            // which case its start has to be re-synthesized here so the
            // suffix stays well-bracketed (spec 6.5).
            self.deliver_start(n, keep, out);
            return;
        }
        match &entry.entry {
            ConversationEntryKind::SubAgentSpawn {
                task,
                background,
                settings,
            } => {
                self.open_run(
                    n,
                    sub_start_event(n, task.clone(), *background, settings.clone()),
                    at,
                    keep,
                    out,
                );
            }
            ConversationEntryKind::Message { .. } => {
                // The run opens at one of its own messages, so its spawn
                // entry is behind us: either an interleaved parent entry
                // closed the bracket in between, or the log has no spawn
                // entry at all (legacy logs lead with the task user
                // message). The root's own start is the truth whenever we
                // have seen one, and the legacy fallback reads the task off
                // the user message and defaults the rest. Either way the
                // start stays untagged: this entry's durable frame is its
                // own `MessageEnd`.
                let start = self.spawned.get(&n).cloned().unwrap_or_else(|| {
                    sub_start_event(n, subagent_task(entry), false, fallback_settings())
                });
                self.open_run(n, start, None, keep, out);
            }
            // Settings entries ahead of any message don't open the
            // bracket; the first `Message` entry does. A compaction
            // marker likewise opens no bracket.
            ConversationEntryKind::ModelChange { .. }
            | ConversationEntryKind::ThinkingChange { .. }
            | ConversationEntryKind::SpeedChange { .. }
            | ConversationEntryKind::VerbosityChange { .. }
            | ConversationEntryKind::SystemPrompt { .. }
            | ConversationEntryKind::Compaction { .. } => {}
        }
    }

    /// Emit `start` as run `n`'s [`AgentEvent::SubAgentStart`] and
    /// remember it for a possible re-synthesis.
    fn open_run(
        &mut self,
        n: usize,
        start: AgentEvent,
        at: Option<EntryRef>,
        keep: bool,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        out.push_back(durable(at, start.clone()));
        self.spawned.insert(n, start.clone());
        let run = self.open_runs.entry(n).or_default();
        run.start = Some(start);
        run.start_delivered = keep;
    }

    /// Re-emit run `n`'s start when it was computed for an entry the walk
    /// dropped. Untagged: its spawn root is at or below the cursor, so a
    /// durable tag would make the client's cursor invariant drop the
    /// bracket (spec 6.5).
    fn deliver_start(&mut self, n: usize, keep: bool, out: &mut VecDeque<TaggedEvent>) {
        let Some(run) = self.open_runs.get_mut(&n) else {
            return;
        };
        if !keep || run.start_delivered {
            return;
        }
        if let Some(start) = run.start.clone() {
            out.push_back(transient(start));
            run.start_delivered = true;
        }
    }

    /// Force-close every open run except the ones in `live_subs` and,
    /// when the walk is entering `current`, that run.
    ///
    /// A sub-agent's conclusion is never persisted, so this transition
    /// heuristic is the only way a finished run's box gets concluded from
    /// a log. It is right for a run that has finished and wrong for one
    /// that is still going, which is exactly the distinction `live_subs`
    /// carries: dead-log replay passes an empty set and closes
    /// everything, while a live backfill keeps the host's running runs
    /// open for their real `SubAgentEnd`.
    fn close_finished_runs(
        &mut self,
        current: Option<usize>,
        live_subs: &BTreeSet<usize>,
        keep: bool,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        let finished: Vec<usize> = self
            .open_runs
            .keys()
            .copied()
            .filter(|n| Some(*n) != current && !live_subs.contains(n))
            .collect();
        for n in finished {
            self.close_run(n, keep, out);
        }
    }

    /// Close run `n`, emitting its [`AgentEvent::SubAgentEnd`] with the
    /// accumulated report. A run that produced neither a `SubAgentSpawn`
    /// entry nor a `Message` entry has no start yet; emit one with an
    /// empty task and default settings so the bracketing stays balanced.
    fn close_run(&mut self, n: usize, keep: bool, out: &mut VecDeque<TaggedEvent>) {
        self.deliver_start(n, keep, out);
        let Some(run) = self.open_runs.remove(&n) else {
            return;
        };
        if run.start.is_none() {
            out.push_back(transient(sub_start_event(
                n,
                String::new(),
                false,
                fallback_settings(),
            )));
        }
        out.push_back(transient(AgentEvent::SubAgentEnd {
            parent: AgentId::Main,
            child: AgentId::Sub(n),
            report: run.report,
            conclusion: run.conclusion,
        }));
    }

    /// Translate one entry into zero or more events, appending them
    /// to `out`. `log` is consulted only for a `Compaction` entry, to
    /// estimate the post-compaction occupancy of the reduced
    /// projection. Single-thread projection ([`project_thread`]) passes
    /// `None`: a sub-agent thread never carries a `Compaction` entry.
    ///
    /// `at` is the entry's position, carried onto whichever single event
    /// of this entry is durable (see [`project_suffix`]).
    fn project_entry(
        &mut self,
        entry: &ConversationEntry,
        at: Option<EntryRef>,
        log: Option<&LogSnapshot>,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        let agent_id = match agent_id_for(entry) {
            Some(id) => id,
            // [`ThreadKind::Meta`] is structural framing (system
            // prompt root) that doesn't render as a user-facing
            // event. Skip silently.
            None => return,
        };

        match &entry.entry {
            ConversationEntryKind::SystemPrompt { .. } => {
                // Model-facing metadata; not user-visible.
            }
            ConversationEntryKind::ModelChange { provider, model_id } => {
                self.settings_notice(
                    agent_id,
                    at,
                    format!("Model set to {provider}/{model_id}."),
                    out,
                );
            }
            ConversationEntryKind::ThinkingChange { level } => {
                self.settings_notice(
                    agent_id,
                    at,
                    format!("Thinking effort set to {level}."),
                    out,
                );
            }
            ConversationEntryKind::SpeedChange { speed } => {
                self.settings_notice(agent_id, at, format!("Speed set to {speed}."), out);
            }
            ConversationEntryKind::VerbosityChange { verbosity } => {
                self.settings_notice(
                    agent_id,
                    at,
                    format!("Output verbosity set to {verbosity}."),
                    out,
                );
            }
            ConversationEntryKind::SubAgentSpawn { .. } => {
                // Seed entry: projected as the synthesized
                // SubAgentStart by `bracket_subagent`, never as a
                // notice.
            }
            ConversationEntryKind::Compaction {
                tokens_before,
                summary,
                ..
            } => {
                // Mirror the live path: a compaction reduces context
                // but emits no `UsageUpdate`, and the retained tail's
                // assistant `usage` is stale, so the footer would keep
                // showing the pre-compaction occupancy without this.
                // `tokens_after` is the occupancy of the reduced
                // projection as of this boundary. The summary is the
                // durable on-disk record, so we carry it through here
                // to paint the same collapsible compaction-summary row
                // a live run shows.
                let Some(log) = log else {
                    // Only single-thread projection passes `None`, and a
                    // sub-agent thread never carries a `Compaction`
                    // entry, so this arm is unreachable there.
                    debug_assert!(
                        false,
                        "compaction entry on a thread projected without a log"
                    );
                    return;
                };
                let tokens_after =
                    estimate_conversation_context(&log.linearize(&entry.id, ThreadFilter::USER))
                        .tokens;
                out.push_back(durable(
                    at,
                    AgentEvent::CompactionEnd {
                        agent_id,
                        reason: CompactionReason::Manual,
                        tokens_before: *tokens_before,
                        tokens_after,
                        summary: Some(summary.clone()),
                        error: None,
                    },
                ));
            }
            ConversationEntryKind::Message { message: agent_msg } => {
                self.seen_message.insert(agent_id);
                match &agent_msg.kind {
                    AgentMessageKind::Wire(Message::User(_))
                    | AgentMessageKind::TaskNotification(_) => {
                        // User prompts and task notices both replay as a
                        // MessageStart/End pair around the entry, carrying the
                        // typed `AgentMessage` so the frontend rebuilds them on
                        // resume exactly as they rendered live. (The notice has
                        // no stored wire message; it acquires its framing only
                        // when it projects onto the provider.)
                        out.push_back(transient(AgentEvent::MessageStart {
                            agent_id,
                            message: agent_msg.clone(),
                        }));
                        out.push_back(durable(
                            at,
                            AgentEvent::MessageEnd {
                                agent_id,
                                message: agent_msg.clone(),
                            },
                        ));
                    }
                    AgentMessageKind::Wire(Message::Assistant(a)) => {
                        self.project_assistant(agent_id, at, agent_msg, a, out);
                    }
                    AgentMessageKind::Wire(Message::ToolResult(tr)) => {
                        self.project_tool_result(agent_id, at, agent_msg, tr, out);
                    }
                }
            }
        }
    }

    /// Emit a [`AgentEvent::Notice`] for a settings entry, but only
    /// when `agent_id`'s thread has already projected a `Message`
    /// entry — seed entries (session creation) precede any message
    /// on their thread and stay silent, since they never produced a
    /// visible notice live either.
    fn settings_notice(
        &self,
        agent_id: AgentId,
        at: Option<EntryRef>,
        text: String,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        if self.seen_message.contains(&agent_id) {
            out.push_back(durable(at, AgentEvent::Notice { agent_id, text }));
        }
    }

    /// Fold `assistant`'s text into the sub-agent run's report, and
    /// capture the run's conclusion from `assistant`'s stop reason.
    ///
    /// Both overwrite rather than accumulate: the report is the most
    /// recent sub-agent assistant message's text, and the conclusion its
    /// stop reason (see [`conclusion_from_stop_reason`]). A run starts out
    /// with an empty report and `Completed`, so after its last assistant
    /// message these hold the final report and conclusion `close_run`
    /// reads onto the `SubAgentEnd`. A no-op unless `agent_id` names an
    /// open run.
    ///
    /// Split out from projection so deferred replay can advance the
    /// report without cloning the sub-agent's messages into events. The
    /// naive alternative, skipping sub-agent projection wholesale, would
    /// also skip this and leave every resumed box with an empty report.
    fn capture_sub_report(
        &mut self,
        agent_id: AgentId,
        assistant: &aj_models::types::AssistantMessage,
    ) {
        let AgentId::Sub(n) = agent_id else { return };
        let Some(run) = self.open_runs.get_mut(&n) else {
            return;
        };
        let mut report = String::new();
        for c in &assistant.content {
            if let AssistantContent::Text(t) = c {
                report.push_str(&t.text);
            }
        }
        run.report = report;
        run.conclusion = conclusion_from_stop_reason(&assistant.stop_reason);
    }

    /// Advance the open sub-agent run's report from `entry` without
    /// projecting its content events. Deferred replay calls this for
    /// sub-agent entries: only an assistant message carries report
    /// text, every other kind is a no-op here.
    fn capture_sub_report_from_entry(&mut self, entry: &ConversationEntry) {
        let Some(agent_id) = agent_id_for(entry) else {
            return;
        };
        match &entry.entry {
            ConversationEntryKind::Message { message } => {
                if let Some(Message::Assistant(a)) = message.as_stored_wire() {
                    self.capture_sub_report(agent_id, a);
                }
            }
            _ => {}
        }
    }

    /// Project an assistant-role message into a `MessageStart`
    /// (with an empty placeholder so renderers can open the slot)
    /// followed by a `MessageEnd` carrying the finalized message.
    /// Tracks `tool_call` blocks so the matching tool_result entry
    /// later in the log can synthesize a labeled
    /// `ToolExecutionStart`/`End` pair.
    fn project_assistant(
        &mut self,
        agent_id: AgentId,
        at: Option<EntryRef>,
        agent_msg: &AgentMessage,
        assistant: &aj_models::types::AssistantMessage,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        // MessageStart carries an empty placeholder (with identity
        // stamped from the finalized message) so renderers open
        // their assistant slot without painting the content twice;
        // MessageEnd is the authoritative finalized snapshot. This
        // mirrors the live-streaming shape where MessageStart fires
        // before any content arrives.
        let empty_start = aj_models::types::AssistantMessage {
            content: Vec::new(),
            api: assistant.api.clone(),
            provider: assistant.provider.clone(),
            model: assistant.model.clone(),
            response_id: assistant.response_id.clone(),
            usage: assistant.usage.clone(),
            stop_reason: assistant.stop_reason.clone(),
            error: assistant.error.clone(),
            timestamp: assistant.timestamp,
        };
        out.push_back(transient(AgentEvent::MessageStart {
            agent_id,
            message: AgentMessage::wire(Message::Assistant(empty_start)),
        }));
        out.push_back(durable(
            at,
            AgentEvent::MessageEnd {
                agent_id,
                message: agent_msg.clone(),
            },
        ));

        // While a sub-agent run is open, record this assistant
        // message's text as the running report; after the run's last
        // assistant message it holds the final report.
        self.capture_sub_report(agent_id, assistant);

        // Synthesize the matching `UsageUpdate`. Live runs emit one
        // per assistant turn on the bus; without this resumed
        // sessions would only paint the footer's context indicator
        // (and any other usage listener) starting from the first
        // post-resume turn, even though every persisted assistant
        // message has its `usage` on disk. Ordering matches the
        // live agent: `UsageUpdate.accumulated_*` reflects the total
        // *before* this turn is folded in, then we add the per-turn
        // delta into the accumulator for the next emission.
        let acc = self.usage_accumulators.entry(agent_id).or_default();
        let turn_usage = TokenUsage {
            accumulated_input: acc.input,
            turn_input: assistant.usage.input,
            accumulated_output: acc.output,
            turn_output: assistant.usage.output,
            accumulated_cache_write: acc.cache_write,
            turn_cache_write: assistant.usage.cache_write,
            accumulated_cache_read: acc.cache_read,
            turn_cache_read: assistant.usage.cache_read,
        };
        out.push_back(transient(AgentEvent::UsageUpdate {
            agent_id,
            usage: turn_usage,
        }));
        acc.input += assistant.usage.input;
        acc.output += assistant.usage.output;
        acc.cache_write += assistant.usage.cache_write;
        acc.cache_read += assistant.usage.cache_read;

        // Track tool_call blocks so subsequent tool_result entries
        // can synthesize a matching `ToolExecutionStart` (with
        // captured args) and `ToolExecutionEnd`.
        for c in &assistant.content {
            if let aj_models::types::AssistantContent::ToolCall(tc) = c {
                self.tool_calls
                    .insert(tc.id.clone(), (tc.name.clone(), tc.arguments.clone()));
            }
        }
    }

    /// Project a tool_result message into a
    /// `ToolExecutionStart`/`End` pair (so the renderer paints the
    /// tool component) bracketed by a `MessageStart`/`End` pair (so
    /// persistence/event-tape listeners see the same shape live runs
    /// produce). The `ToolDetails` payload is recovered through the session
    /// codec, falling back to a text-only synthesis off the wire content when
    /// absent or malformed.
    fn project_tool_result(
        &self,
        agent_id: AgentId,
        at: Option<EntryRef>,
        agent_msg: &AgentMessage,
        tr: &aj_models::types::ToolResultMessage,
        out: &mut VecDeque<TaggedEvent>,
    ) {
        // Look up the tool name and input args captured from the
        // preceding assistant message's tool_call block. Missing
        // entries (truncated/legacy logs) fall back to a generic
        // name and empty args; the renderer copes with both.
        let (tool_name, args) = self
            .tool_calls
            .get(&tr.tool_call_id)
            .cloned()
            .unwrap_or_else(|| ("tool".to_string(), Value::Object(Default::default())));

        // The session codec hydrates persisted text references and uses normal
        // `ToolDetails` deserialization for every other shape. Missing or
        // malformed details fall back to the model-facing content.
        let result = match tr.details.as_ref() {
            Some(value) => resolve_tool_details(value, &tr.content)
                .unwrap_or_else(|| text_fallback(&tool_name, &tr.content)),
            None => text_fallback(&tool_name, &tr.content),
        };
        let mut normalized_message = agent_msg.clone();
        let AgentMessageKind::Wire(Message::ToolResult(normalized_result)) =
            &mut normalized_message.kind
        else {
            unreachable!("project_tool_result requires a tool-result message");
        };
        normalized_result.details =
            Some(serde_json::to_value(&result).expect("resolved ToolDetails always serialize"));

        out.push_back(transient(AgentEvent::ToolExecutionStart {
            agent_id,
            call_id: tr.tool_call_id.clone(),
            tool: tool_name.clone(),
            args,
        }));
        // MessageStart/End around the tool_result so a replay-driven
        // pump sees the same shape a live agent emits.
        out.push_back(transient(AgentEvent::MessageStart {
            agent_id,
            message: normalized_message.clone(),
        }));
        out.push_back(durable(
            at,
            AgentEvent::MessageEnd {
                agent_id,
                message: normalized_message,
            },
        ));
        out.push_back(transient(AgentEvent::ToolExecutionEnd {
            agent_id,
            call_id: tr.tool_call_id.clone(),
            tool: tool_name,
            result,
            content: std::sync::Arc::from(tr.content.clone().into_boxed_slice()),
            is_error: tr.is_error,
        }));
    }
}

/// Build a [`ToolDetails::Text`] off the wire content. The
/// summary is the resolved tool name; the body is the concatenation
/// of every [`UserContent::Text`] block in the result content, with
/// a `[image: <mime>]` placeholder line appended for each
/// [`UserContent::Image`] so replayed entries that lack a persisted
/// structured payload still surface a hint that an image was
/// attached.
fn text_fallback(tool_name: &str, content: &[UserContent]) -> ToolDetails {
    let mut body = String::new();
    for block in content {
        match block {
            UserContent::Text(t) => body.push_str(&t.text),
            UserContent::Image(img) => {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("[image: {}]", img.mime_type));
                body.push('\n');
            }
        }
    }
    // Trim a trailing newline introduced solely by an image
    // placeholder; the renderer adds its own separation.
    if body.ends_with('\n') {
        body.pop();
    }
    ToolDetails::Text {
        summary: tool_name.to_string(),
        body,
    }
}

/// Extract a sub-agent's task from its first `Message` entry, for
/// legacy logs without a `SubAgentSpawn` entry. That entry is the
/// sub-agent's user prompt, whose concatenated text is the task; any
/// other shape yields an empty task.
fn subagent_task(entry: &ConversationEntry) -> String {
    let ConversationEntryKind::Message { message } = &entry.entry else {
        return String::new();
    };
    let Some(Message::User(u)) = message.as_stored_wire() else {
        return String::new();
    };
    let mut task = String::new();
    for block in &u.content {
        if let UserContent::Text(t) = block {
            task.push_str(&t.text);
        }
    }
    task
}

/// Map an entry's [`ThreadKind`] / `agent_id` framing onto an
/// [`AgentId`]. Returns `None` for [`ThreadKind::Meta`], whose
/// entries (the system-prompt root) carry no user-visible payload.
fn agent_id_for(entry: &ConversationEntry) -> Option<AgentId> {
    match entry.thread {
        ThreadKind::User => Some(AgentId::Main),
        ThreadKind::Subagent => entry.agent_id.map(AgentId::Sub),
        ThreadKind::Meta => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView};
    use crate::persistence::ConversationPersistence;
    use aj_agent::tool::DiffDetails;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ThinkingContent, ToolCall,
        ToolResultMessage, UserMessage,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn fresh_sessions_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "aj-session-replay-test-{pid}-{tid:?}-{nanos}",
            pid = std::process::id(),
            tid = std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_msg(content: Vec<AssistantContent>) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content,
            ..AssistantMessage::empty()
        }))
    }

    fn tool_result_msg(
        id: &str,
        name: &str,
        body: &str,
        details: Option<&ToolDetails>,
    ) -> AgentMessage {
        let mut tr = ToolResultMessage::text(id, name, body, false);
        tr.details = details.and_then(|d| serde_json::to_value(d).ok());
        AgentMessage::wire(Message::ToolResult(tr))
    }

    fn replay_events_with_raw_tool_details(details: Value) -> Vec<AgentEvent> {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("system prompt");

        let mut result = ToolResultMessage::text("tu-edit", "edit_file", "edited", false);
        result.timestamp = 42;
        result.details = Some(details);
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("edit it")).expect("user message");
            view.add_message(assistant_msg(vec![AssistantContent::ToolCall(ToolCall {
                id: "tu-edit".into(),
                name: "edit_file".into(),
                arguments: json!({"path": "/tmp/x"}),
            })]))
            .expect("assistant message");
            view.add_message(AgentMessage::wire(Message::ToolResult(result)))
                .expect("tool result");
        }

        replay(&log).collect()
    }

    fn replay_raw_tool_details(details: Value) -> ToolDetails {
        replay_events_with_raw_tool_details(details)
            .into_iter()
            .find_map(|event| match event {
                AgentEvent::ToolExecutionEnd {
                    call_id, result, ..
                } if call_id == "tu-edit" => Some(result),
                _ => None,
            })
            .expect("tool execution end")
    }

    #[test]
    fn task_notification_replays_as_typed_kind() {
        use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};

        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("system prompt");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("user message");
            view.add_message(AgentMessage::task_notification(TaskNotification::new(
                "cargo build".into(),
                TaskNotificationKind::Bash,
                TaskOutcome::Succeeded,
                "exit code 0".into(),
            )))
            .expect("task notification");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // The notice replays as a MessageEnd carrying the typed kind, so
        // the frontend rebuilds it on resume as a notification, not a
        // user prompt.
        let notice = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::MessageEnd { message, .. } => match &message.kind {
                    AgentMessageKind::TaskNotification(n) => Some(n.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("notice replayed as the typed kind");
        assert_eq!(notice.label, "cargo build");
        assert_eq!(notice.body, "exit code 0");

        // The only user-role MessageEnd is the real prompt; the notice is
        // never replayed as a user message.
        let user_ends: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                    Some(Message::User(u)) => Some(
                        u.content
                            .iter()
                            .filter_map(|c| match c {
                                UserContent::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(user_ends, vec!["hi".to_string()]);
    }

    /// Build a seeded log exercising assistant text, thinking, tool
    /// use, and tool result with structured details.
    fn seeded_log() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("system prompt");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("user msg");
            view.add_message(assistant_msg(vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "let me think".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                AssistantContent::Text(TextContent {
                    text: "hello".into(),
                    text_signature: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "/tmp/x"}),
                }),
            ]))
            .expect("assistant msg");
            view.add_message(tool_result_msg("call-1", "read_file", "result body", None))
                .expect("tool result msg");
        }
        (dir, log)
    }

    #[test]
    fn replay_projects_entries_on_demand() {
        let (_dir, log) = seeded_log();
        let snapshot = log.snapshot();
        let mut events = Replay::new(&snapshot);

        assert_eq!(events.next_entry, 0);
        assert!(events.pending.is_empty());
        assert!(events.state.seen_message.is_empty());
        assert!(events.state.tool_calls.is_empty());
        assert!(events.state.usage_accumulators.is_empty());

        assert!(matches!(
            events.next().map(|projected| projected.event),
            Some(AgentEvent::MessageStart {
                agent_id: AgentId::Main,
                ..
            })
        ));
        assert_eq!(events.next_entry, 2);
        assert_eq!(events.pending.len(), 1);
        assert!(events.state.tool_calls.is_empty());
        assert!(events.state.usage_accumulators.is_empty());
    }

    #[test]
    fn replay_projects_assistant_thinking_text_and_tool_results() {
        let (_dir, log) = seeded_log();
        let events: Vec<AgentEvent> = replay(&log).collect();

        // Expected order:
        //   MessageStart(User "hi")
        //   MessageEnd(User "hi")
        //   MessageStart(Assistant empty)
        //   MessageEnd(Assistant {thinking, text, tool_call})
        //   UsageUpdate(Main)
        //   ToolExecutionStart { tool: "read_file", call_id: "call-1", args }
        //   MessageStart(ToolResult)
        //   MessageEnd(ToolResult)
        //   ToolExecutionEnd   { tool: "read_file", call_id: "call-1" }
        assert_eq!(events.len(), 9, "got events: {events:#?}");

        match &events[0] {
            AgentEvent::MessageStart { message, .. } => match message.as_stored_wire() {
                Some(Message::User(u)) => match &u.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "hi"),
                    other => panic!("expected text, got {other:?}"),
                },
                other => panic!("expected user, got {other:?}"),
            },
            other => panic!("expected user MessageStart, got {other:?}"),
        }

        // Assistant MessageEnd carries the finalized content.
        match &events[3] {
            AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                Some(Message::Assistant(a)) => {
                    assert_eq!(a.content.len(), 3);
                }
                other => panic!("expected assistant, got {other:?}"),
            },
            other => panic!("expected assistant MessageEnd, got {other:?}"),
        }

        // UsageUpdate immediately follows the assistant MessageEnd —
        // same shape and ordering the live agent uses on its bus.
        match &events[4] {
            AgentEvent::UsageUpdate { agent_id, .. } => {
                assert_eq!(*agent_id, AgentId::Main);
            }
            other => panic!("expected UsageUpdate, got {other:?}"),
        }

        match &events[5] {
            AgentEvent::ToolExecutionStart {
                agent_id,
                call_id,
                tool,
                args,
            } => {
                assert_eq!(*agent_id, AgentId::Main);
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                assert_eq!(args, &json!({"path": "/tmp/x"}));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }

        match &events[8] {
            AgentEvent::ToolExecutionEnd {
                call_id,
                tool,
                result,
                is_error,
                ..
            } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                assert!(!is_error);
                match result {
                    ToolDetails::Text { summary, body } => {
                        assert_eq!(summary, "read_file");
                        assert_eq!(body, "result body");
                    }
                    other => panic!("expected Text fallback, got {other:?}"),
                }
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    #[test]
    fn replay_skips_system_prompt_and_handles_empty_log() {
        // A log with only the system-prompt root produces zero
        // events: meta entries are structural framing.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into())
            .expect("set system prompt");

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(events.is_empty(), "got: {events:#?}");
    }

    #[test]
    fn replay_projects_structured_tool_details_on_resume() {
        // When the producer persisted structured `ToolDetails`
        // onto the tool result message, replay deserializes the
        // payload back and surfaces it on the `ToolExecutionEnd`
        // event so resumed sessions render diffs / bash output /
        // todo snapshots / sub-agent reports the same way live
        // runs do.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let diff_details = ToolDetails::Diff(DiffDetails::new("/tmp/x", "a", "b"));

        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("edit it")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::ToolCall(ToolCall {
                id: "tu-edit".into(),
                name: "edit_file".into(),
                arguments: json!({"path": "/tmp/x"}),
            })]))
            .expect("a");
            view.add_message(tool_result_msg(
                "tu-edit",
                "edit_file",
                "edited",
                Some(&diff_details),
            ))
            .expect("tr");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-edit"),
            )
            .expect("ToolExecutionEnd for tu-edit");
        match end {
            AgentEvent::ToolExecutionEnd { result, .. } => match result {
                ToolDetails::Diff(diff) => {
                    assert_eq!(diff.path(), "/tmp/x");
                    assert!(diff.lines().iter().any(|line| line.text() == "- a"));
                    assert!(diff.lines().iter().any(|line| line.text() == "+ b"));
                }
                other => panic!("expected Diff details, got {other:?}"),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn replay_deserializes_literal_legacy_diff_details() {
        let details = replay_raw_tool_details(json!({
            "kind": "diff",
            "path": "/tmp/x",
            "before": "same\nold\n",
            "after": "same\nnew\n",
        }));

        let ToolDetails::Diff(diff) = details else {
            panic!("expected legacy details to normalize to Diff");
        };
        assert_eq!(diff.path(), "/tmp/x");
        assert!(diff.lines().iter().any(|line| line.text() == "- old"));
        assert!(diff.lines().iter().any(|line| line.text() == "+ new"));
    }

    #[test]
    fn replay_normalizes_legacy_details_in_both_message_events() {
        let events = replay_events_with_raw_tool_details(json!({
            "kind": "diff",
            "path": "/tmp/x",
            "before": "same\nold\n",
            "after": "same\nnew\n",
        }));
        let projected: Vec<&ToolResultMessage> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageStart { message, .. }
                | AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                    Some(Message::ToolResult(result)) => Some(result),
                    _ => None,
                },
                _ => None,
            })
            .collect();

        assert_eq!(projected.len(), 2);
        for result in projected {
            assert_eq!(result.tool_call_id, "tu-edit");
            assert_eq!(result.tool_name, "edit_file");
            assert!(!result.is_error);
            assert_eq!(result.timestamp, 42);
            assert!(matches!(
                result.content.as_slice(),
                [UserContent::Text(text)] if text.text == "edited"
            ));
            let details = result.details.as_ref().expect("normalized details");
            assert_eq!(details["format"], "display-v1");
            assert!(details.get("before").is_none());
            assert!(details.get("after").is_none());
            assert!(
                details["lines"]
                    .as_array()
                    .is_some_and(|lines| lines.iter().all(Value::is_string))
            );
        }
    }

    #[test]
    fn replay_expands_text_references_in_messages_tool_events_and_print_json() {
        let events = replay_events_with_raw_tool_details(json!({
            "kind": "text",
            "summary": "read_file exact summary",
            "body_ref": {"source": "content_text", "append_newline": true},
        }));

        let message_results: Vec<&ToolResultMessage> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageStart { message, .. }
                | AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                    Some(Message::ToolResult(result)) => Some(result),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(message_results.len(), 2);
        for result in message_results {
            assert_eq!(
                result.details,
                Some(json!({
                    "kind": "text",
                    "summary": "read_file exact summary",
                    "body": "edited\n",
                })),
            );
        }

        let details = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolExecutionEnd { result, .. } => Some(result),
                _ => None,
            })
            .expect("tool execution end");
        match details {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "read_file exact summary");
                assert_eq!(body, "edited\n");
            }
            other => panic!("expected text details, got {other:?}"),
        }

        let print_json = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("event serializes"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!print_json.contains("body_ref"), "{print_json}");
    }

    #[test]
    fn replay_keeps_legacy_text_bodies() {
        let details = replay_raw_tool_details(json!({
            "kind": "text",
            "summary": "legacy summary",
            "body": "legacy display body",
        }));

        match details {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "legacy summary");
                assert_eq!(body, "legacy display body");
            }
            other => panic!("expected legacy text details, got {other:?}"),
        }
    }

    #[test]
    fn replay_falls_back_for_malformed_text_references() {
        let events = replay_events_with_raw_tool_details(json!({
            "kind": "text",
            "summary": "must not survive",
            "body_ref": {"source": "content_text", "append_newline": "yes"},
        }));

        let mut projected = 0;
        for event in events {
            let details = match event {
                AgentEvent::MessageStart { message, .. }
                | AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                    Some(Message::ToolResult(result)) => result.details.clone(),
                    _ => continue,
                },
                AgentEvent::ToolExecutionEnd { result, .. } => {
                    Some(serde_json::to_value(result).expect("details serialize"))
                }
                _ => continue,
            }
            .expect("fallback details");
            projected += 1;
            assert_eq!(
                details,
                json!({"kind": "text", "summary": "edit_file", "body": "edited"})
            );
        }
        assert_eq!(projected, 3, "both message events and the tool end");
    }

    #[test]
    fn replay_keeps_text_fallback_for_malformed_compact_diff() {
        let details = replay_raw_tool_details(json!({
            "kind": "diff",
            "format": "display-v1",
            "path": "/tmp/x",
            "lines": ["not a canonical line"],
        }));

        match details {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "edit_file");
                assert_eq!(body, "edited");
            }
            other => panic!("expected text fallback, got {other:?}"),
        }
    }

    #[test]
    fn replay_routes_subagent_entries_to_sub_agent_id() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let any_sub = events
            .iter()
            .any(|e| matches!(e.agent_id(), AgentId::Sub(1)));
        assert!(any_sub, "expected at least one Sub(1) event in {events:#?}");
        let any_main = events.iter().any(|e| matches!(e.agent_id(), AgentId::Main));
        assert!(any_main, "expected at least one Main event in {events:#?}");
    }

    #[test]
    fn replay_brackets_subagent_run_with_start_and_end() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        let start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present");
        let end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present");

        match &events[start_idx] {
            AgentEvent::SubAgentStart {
                parent,
                child,
                task,
                ..
            } => {
                assert_eq!(*parent, AgentId::Main);
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "subtask");
            }
            other => panic!("expected SubAgentStart, got {other:?}"),
        }
        match &events[end_idx] {
            AgentEvent::SubAgentEnd {
                parent,
                child,
                report,
                conclusion,
            } => {
                assert_eq!(*parent, AgentId::Main);
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(report, "reply");
                assert_eq!(*conclusion, SubAgentConclusion::Completed);
            }
            other => panic!("expected SubAgentEnd, got {other:?}"),
        }

        let first_sub = events
            .iter()
            .position(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("at least one Sub(1) event");
        let last_sub = events
            .iter()
            .rposition(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("at least one Sub(1) event");

        assert!(
            start_idx < first_sub,
            "SubAgentStart must precede the first Sub(1) event"
        );
        assert!(
            end_idx > last_sub,
            "SubAgentEnd must follow the last Sub(1) event"
        );
    }

    /// Build a log with one blocking sub-agent run whose final assistant
    /// message carries `stop_reason`.
    fn subagent_log_with_stop_reason(stop_reason: StopReason) -> ConversationLog {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "reply".into(),
                    text_signature: None,
                })],
                stop_reason,
                ..AssistantMessage::empty()
            })))
            .expect("a");
        }

        log
    }

    fn replayed_conclusion(events: &[AgentEvent]) -> SubAgentConclusion {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::SubAgentEnd { conclusion, .. } => Some(*conclusion),
                _ => None,
            })
            .expect("SubAgentEnd present")
    }

    /// Replay reads the run's outcome off its final message stop reason, so
    /// a token-cap `Length` terminal reads as `Truncated`.
    #[test]
    fn replay_infers_truncated_conclusion_from_the_final_stop_reason() {
        let log = subagent_log_with_stop_reason(StopReason::Length);
        let events: Vec<_> = replay(&log).collect();
        assert_eq!(replayed_conclusion(&events), SubAgentConclusion::Truncated);
    }

    /// A failed run's terminal message is persisted with `stop_reason ==
    /// Error`, so replay reconstructs `Failed` without any dedicated
    /// on-disk marker.
    #[test]
    fn replay_infers_failed_conclusion_from_an_error_terminal() {
        let log = subagent_log_with_stop_reason(StopReason::Error);
        let events: Vec<_> = replay(&log).collect();
        assert_eq!(replayed_conclusion(&events), SubAgentConclusion::Failed);
    }

    /// A run interrupted mid tool-loop leaves a `ToolUse` assistant message
    /// as its last persisted terminal (it never reached a final answer).
    /// Replay reconstructs `Failed`, matching the live path, which maps such
    /// an interruption (an aborted turn) to `Failed`.
    #[test]
    fn replay_infers_failed_conclusion_from_an_interrupted_tool_use_terminal() {
        let log = subagent_log_with_stop_reason(StopReason::ToolUse);
        let events: Vec<_> = replay(&log).collect();
        assert_eq!(replayed_conclusion(&events), SubAgentConclusion::Failed);
    }

    /// Deferred replay withholds the sub's content but must still report
    /// the same conclusion as full replay, since resume uses the deferred
    /// path. Cover the two outcomes that differ from the default.
    #[test]
    fn deferred_replay_reconstructs_the_same_conclusion_as_full_replay() {
        for stop_reason in [StopReason::Length, StopReason::Error] {
            let label = format!("{stop_reason:?}");
            let log = subagent_log_with_stop_reason(stop_reason);
            let full: Vec<_> = replay(&log).collect();
            let deferred: Vec<_> = replay_deferring_subs(&log).collect();
            assert_eq!(
                replayed_conclusion(&deferred),
                replayed_conclusion(&full),
                "deferred and full disagree for stop reason {label}",
            );
        }
    }

    /// A normal multi-turn sub-agent run has intermediate `ToolUse`
    /// assistant messages but ends on `Stop`. The conclusion tracks the
    /// last assistant message, so the trailing `Stop` wins and the run
    /// reconstructs `Completed`, not `Failed`. This pins the overwrite
    /// semantics the `ToolUse -> Failed` mapping relies on.
    #[test]
    fn replay_reconstructs_completed_for_a_tool_using_run_ending_in_stop() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            // Intermediate tool-use inference (would map to Failed if it
            // were the terminal).
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "let me check".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::empty()
            })))
            .expect("tool-use turn");
            // Final answer.
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "done".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::empty()
            })))
            .expect("final turn");
        }

        let events: Vec<_> = replay(&log).collect();
        assert_eq!(replayed_conclusion(&events), SubAgentConclusion::Completed);
    }

    #[test]
    fn replay_closes_subagent_before_main_resumes() {
        // A main turn that follows a sub-agent run must close the sub
        // (emit SubAgentEnd) before any of its own events appear. We
        // build the resuming main activity by appending to the user
        // thread head captured before the sub run.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head.clone(), 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        // Resume main activity after the sub run.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "back on main".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        let end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present");
        let last_sub = events
            .iter()
            .rposition(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("Sub(1) event present");
        // First Main event after the last Sub(1) event marks main
        // resuming. Skip the correlation events, whose `agent_id()`
        // reports the parent (Main).
        let main_resumes = events
            .iter()
            .enumerate()
            .skip(last_sub + 1)
            .find(|(_, e)| {
                matches!(e.agent_id(), AgentId::Main)
                    && !matches!(
                        e,
                        AgentEvent::SubAgentStart { .. } | AgentEvent::SubAgentEnd { .. }
                    )
            })
            .map(|(i, _)| i)
            .expect("Main resumes after sub run");

        assert!(end_idx > last_sub, "SubAgentEnd follows last Sub(1) event");
        assert!(
            end_idx < main_resumes,
            "SubAgentEnd must close the sub before main resumes"
        );
    }

    #[test]
    fn replay_falls_back_when_tool_call_id_is_not_tracked() {
        // A truncated/legacy log can carry a tool_result whose
        // tool_call_id was never seen on a preceding assistant
        // message. Replay still emits a sensible event with the
        // fallback "tool" name and an empty args object.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log);
            // Insert the tool_result with no preceding tool_call.
            view.add_message(tool_result_msg("orphan", "", "body", None))
                .expect("orphan tr");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // ToolExecutionStart, MessageStart, MessageEnd, ToolExecutionEnd.
        assert_eq!(events.len(), 4, "got: {events:#?}");
        match &events[0] {
            AgentEvent::ToolExecutionStart { tool, args, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
                assert_eq!(args, &Value::Object(Default::default()));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }
        match &events[3] {
            AgentEvent::ToolExecutionEnd { tool, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    /// Build an assistant message whose persisted `usage` carries
    /// the supplied per-turn token counts. The other identity
    /// fields are left at their defaults — the replay path only
    /// reads `content` and `usage`.
    fn assistant_msg_with_usage(
        content: Vec<AssistantContent>,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content,
            usage: aj_models::types::Usage {
                input,
                output,
                cache_read,
                cache_write,
                ..aj_models::types::Usage::default()
            },
            ..AssistantMessage::empty()
        }))
    }

    /// Two persisted main-agent assistant turns produce two
    /// synthesized `UsageUpdate` events. The first carries its
    /// per-turn deltas against a zero accumulator; the second
    /// carries its deltas against an accumulator equal to the
    /// first turn's contribution (live-agent ordering: emit
    /// before adding into the accumulator).
    #[test]
    fn replay_synthesizes_usage_update_per_assistant_message() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "first".into(),
                    text_signature: None,
                })],
                100,
                50,
                20,
                5,
            ))
            .expect("turn 1");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "second".into(),
                    text_signature: None,
                })],
                200,
                70,
                30,
                10,
            ))
            .expect("turn 2");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let turn_usages: Vec<&aj_agent::types::TokenUsage> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::UsageUpdate {
                    agent_id: AgentId::Main,
                    usage,
                } => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(
            turn_usages.len(),
            2,
            "one UsageUpdate per assistant message"
        );

        let first = turn_usages[0];
        assert_eq!(first.turn_input, 100);
        assert_eq!(first.turn_output, 50);
        assert_eq!(first.turn_cache_read, 20);
        assert_eq!(first.turn_cache_write, 5);
        assert_eq!(first.accumulated_input, 0, "pre-add accumulator");
        assert_eq!(first.accumulated_output, 0);
        assert_eq!(first.accumulated_cache_read, 0);
        assert_eq!(first.accumulated_cache_write, 0);

        let second = turn_usages[1];
        assert_eq!(second.turn_input, 200);
        assert_eq!(second.turn_output, 70);
        assert_eq!(second.turn_cache_read, 30);
        assert_eq!(second.turn_cache_write, 10);
        // After the first turn was emitted the accumulator
        // absorbed the first turn's deltas; the second UsageUpdate
        // sees those as its `accumulated_*`.
        assert_eq!(second.accumulated_input, 100);
        assert_eq!(second.accumulated_output, 50);
        assert_eq!(second.accumulated_cache_read, 20);
        assert_eq!(second.accumulated_cache_write, 5);
    }

    /// A `Compaction` entry replays as a `CompactionEnd` whose
    /// `tokens_after` reflects the reduced projection — not the
    /// retained tail's stale pre-compaction usage. This is what keeps a
    /// resumed compacted session from showing the old occupancy in the
    /// footer.
    #[test]
    fn replay_compaction_emits_compaction_end_with_reduced_after() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let first_kept = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("old request")).expect("u0");
            // The retained assistant reports a large (pre-compaction)
            // prompt; after compaction this usage is stale.
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "old reply".into(),
                    text_signature: None,
                })],
                100_000,
                10,
                0,
                0,
            ))
            .expect("a0");
            let kept = view.add_message(user_msg("recent request")).expect("u1");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "recent reply".into(),
                    text_signature: None,
                })],
                100_000,
                10,
                0,
                0,
            ))
            .expect("a1");
            kept.id
        };
        log.append_compaction(
            ThreadFilter::USER,
            "SUMMARY".into(),
            first_kept,
            100_000,
            None,
        )
        .expect("append compaction");

        let events: Vec<AgentEvent> = replay(&log).collect();

        // No Notice marks the boundary anymore; a CompactionEnd does.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice { text, .. } if text.contains("compact"))),
            "compaction should no longer replay as a Notice"
        );
        let (before, after) = events
            .iter()
            .rev()
            .find_map(|e| match e {
                AgentEvent::CompactionEnd {
                    tokens_before,
                    tokens_after,
                    summary,
                    ..
                } => {
                    // The durable on-disk summary is carried through so a
                    // resumed session paints the same collapsible row.
                    assert_eq!(summary.as_deref(), Some("SUMMARY"));
                    Some((*tokens_before, *tokens_after))
                }
                _ => None,
            })
            .expect("a CompactionEnd event");
        assert_eq!(before, 100_000);
        assert!(
            after < 10_000,
            "tokens_after should drop below the stale 100k anchor, got {after}"
        );
    }

    /// Main and sub-agent assistants keep independent
    /// accumulators. A main-agent turn that follows a sub-agent
    /// turn must not inherit the sub-agent's totals (and vice
    /// versa).
    #[test]
    fn replay_keeps_main_and_subagent_usage_accumulators_separate() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "main".into(),
                    text_signature: None,
                })],
                10,
                5,
                0,
                0,
            ))
            .expect("main turn");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "sub".into(),
                    text_signature: None,
                })],
                40,
                20,
                0,
                0,
            ))
            .expect("sub turn");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let main_turn = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::UsageUpdate {
                    agent_id: AgentId::Main,
                    usage,
                } => Some(usage),
                _ => None,
            })
            .expect("main UsageUpdate present");
        let sub_turn = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::UsageUpdate {
                    agent_id: AgentId::Sub(1),
                    usage,
                } => Some(usage),
                _ => None,
            })
            .expect("sub(1) UsageUpdate present");

        // Each agent's first turn has a zero accumulator — they
        // don't share state.
        assert_eq!(main_turn.accumulated_input, 0);
        assert_eq!(main_turn.turn_input, 10);
        assert_eq!(sub_turn.accumulated_input, 0);
        assert_eq!(sub_turn.turn_input, 40);
    }

    /// Seed settings entries (preceding any message on their
    /// thread) emit no Notice.
    #[test]
    fn replay_keeps_seed_settings_entries_silent() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");
        log.append_model_change(crate::log::ThreadFilter::USER, "anthropic", "claude-x")
            .expect("mc");
        log.append_thinking_change(crate::log::ThreadFilter::USER, "high")
            .expect("tc");
        log.append_speed_change(crate::log::ThreadFilter::USER, "fast")
            .expect("sc");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice { .. })),
            "seed settings entries must be silent, got {events:#?}"
        );
    }

    /// A settings entry recorded after a message on the same thread
    /// emits exactly one Notice with the rendered text.
    #[test]
    fn replay_emits_notice_for_mid_session_settings_entries() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
        }
        log.append_model_change(crate::log::ThreadFilter::USER, "openai", "gpt-x")
            .expect("mc");
        log.append_thinking_change(crate::log::ThreadFilter::USER, "medium")
            .expect("tc");
        log.append_speed_change(crate::log::ThreadFilter::USER, "fast")
            .expect("sc");

        let events: Vec<AgentEvent> = replay(&log).collect();
        let notices: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { agent_id, text } => {
                    assert_eq!(*agent_id, AgentId::Main);
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![
                "Model set to openai/gpt-x.",
                "Thinking effort set to medium.",
                "Speed set to fast.",
            ]
        );
    }

    /// A sub-agent run led by its `SubAgentSpawn` entry emits a
    /// SubAgentStart carrying the recorded task and settings before
    /// the first sub message, and stays silent (no notice) for the
    /// spawn entry itself.
    #[test]
    fn replay_subagent_spawn_entry_drives_sub_agent_start() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        let settings = AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-x".into(),
            thinking: "high".into(),
            thinking_display: String::new(),
            speed: "fast".into(),
            verbosity: "high".into(),
        };
        log.append_subagent_spawn(1, user_head, "subtask", true, &settings)
            .expect("spawn entry");
        {
            let sub_head = log
                .latest_leaf(crate::log::ThreadFilter::subagent(1))
                .expect("sub leaf");
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice { .. })),
            "the spawn entry must not render a notice, got {events:#?}"
        );

        let start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present");
        match &events[start_idx] {
            AgentEvent::SubAgentStart {
                child,
                task,
                background,
                settings: s,
                ..
            } => {
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "subtask");
                assert!(*background, "spawn entry's background mode replays");
                assert_eq!(*s, settings);
            }
            other => panic!("expected SubAgentStart, got {other:?}"),
        }
        // The start precedes every projected Sub(1) event.
        let first_sub = events
            .iter()
            .position(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("Sub(1) events present");
        assert!(start_idx < first_sub);
        match events
            .iter()
            .find(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present")
        {
            AgentEvent::SubAgentEnd { report, .. } => assert_eq!(report, "reply"),
            _ => unreachable!(),
        }
    }

    /// A foreground (blocking) spawn entry replays as foreground. Paired
    /// with the background case above, this pins that replay reads the
    /// entry's stored mode rather than hardcoding a value.
    #[test]
    fn replay_foreground_spawn_entry_stays_foreground() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.head().cloned().expect("head present")
        };
        let settings = AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-x".into(),
            thinking: "high".into(),
            thinking_display: String::new(),
            speed: "fast".into(),
            verbosity: "high".into(),
        };
        log.append_subagent_spawn(1, user_head, "subtask", false, &settings)
            .expect("spawn entry");
        {
            let sub_head = log
                .latest_leaf(crate::log::ThreadFilter::subagent(1))
                .expect("sub leaf");
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let start = events
            .iter()
            .find(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present");
        match start {
            AgentEvent::SubAgentStart { background, .. } => {
                assert!(
                    !background,
                    "foreground spawn entry must replay as foreground"
                );
            }
            _ => unreachable!(),
        }
    }

    /// A sub-agent run led by loose settings entries (no
    /// `SubAgentSpawn`) still brackets sanely via the legacy path:
    /// the seed entries stay silent, the start opens at the run's
    /// first message with default settings, and the end carries the
    /// report.
    #[test]
    fn replay_subagent_with_leading_settings_entries_brackets_via_legacy_path() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        // Seed the sub thread with a settings triple via raw
        // appends, then the task message and a reply.
        log.append(
            Some(user_head),
            crate::log::ThreadKind::Subagent,
            Some(1),
            ConversationEntryKind::ModelChange {
                provider: "anthropic".into(),
                model_id: "claude-x".into(),
            },
        )
        .expect("mc");
        let sub = crate::log::ThreadFilter::subagent(1);
        log.append_thinking_change(sub, "high").expect("tc");
        log.append_speed_change(sub, "fast").expect("sc");
        {
            let sub_head = log.latest_leaf(sub).expect("sub leaf");
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice { .. })),
            "sub-thread seed entries must be silent, got {events:#?}"
        );

        let start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present");
        match &events[start_idx] {
            AgentEvent::SubAgentStart {
                child,
                task,
                background,
                settings,
                ..
            } => {
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "subtask");
                assert!(!background, "a legacy log carries no mode, so foreground");
                assert_eq!(*settings, super::fallback_settings());
            }
            other => panic!("expected SubAgentStart, got {other:?}"),
        }
        // The start still precedes every projected Sub(1) event.
        let first_sub = events
            .iter()
            .position(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("Sub(1) events present");
        assert!(start_idx < first_sub);
        match events
            .iter()
            .find(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present")
        {
            AgentEvent::SubAgentEnd { report, .. } => assert_eq!(report, "reply"),
            _ => unreachable!(),
        }
    }

    /// Legacy logs whose sub threads lead with the task user
    /// message still bracket sub runs; the synthesized
    /// SubAgentStart falls back to empty / "off" / "standard".
    #[test]
    fn replay_subagent_legacy_log_uses_fallback_settings() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.head().cloned().expect("head present")
        };
        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        match events
            .iter()
            .find(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present")
        {
            AgentEvent::SubAgentStart {
                task,
                background,
                settings,
                ..
            } => {
                assert_eq!(task, "subtask");
                assert!(!background, "a legacy log carries no mode, so foreground");
                assert_eq!(*settings, super::fallback_settings());
            }
            _ => unreachable!(),
        }
    }

    /// Build a log with a main thread and one foreground sub-agent run
    /// that produces assistant text, a tool call, a tool result, and a
    /// concluding report, then main resumes. The sub content is rich so
    /// the deferred/`project_thread` parity checks exercise
    /// `MessageStart`/`End`, `ToolExecution*`, and `UsageUpdate`.
    fn log_with_foreground_sub() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head")
        };

        let settings = AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-x".into(),
            thinking: "high".into(),
            thinking_display: String::new(),
            speed: "fast".into(),
            verbosity: "default".into(),
        };
        log.append_subagent_spawn(1, user_head, "subtask", false, &settings)
            .expect("spawn");
        {
            let sub_leaf = log
                .latest_leaf(ThreadFilter::subagent(1))
                .expect("sub leaf");
            let mut view = ConversationView::subagent(&mut log, sub_leaf, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::ToolCall(ToolCall {
                id: "sub-call-1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "/tmp/s"}),
            })]))
            .expect("a tool call");
            view.add_message(tool_result_msg("sub-call-1", "read_file", "sub body", None))
                .expect("tr");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "final sub report".into(),
                text_signature: None,
            })]))
            .expect("a report");
        }

        // Main resumes after the sub run.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "back on main".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        (dir, log)
    }

    /// Build a log whose sub-agent run interleaves with the parent's in
    /// append order: a background sub takes a turn, main takes a turn
    /// while it is still open, then the sub concludes and main
    /// concludes. In append order the sub's entries straddle a main
    /// entry, so full replay opens and closes the sub bracket twice.
    fn log_with_background_sub() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head")
        };

        let settings = super::fallback_settings();
        let mut sub_head = log
            .append_subagent_spawn(1, user_head.clone(), "bg subtask", true, &settings)
            .expect("spawn")
            .id;

        // First sub turn.
        sub_head = {
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(user_msg("bg subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub step one".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("sub head")
        };

        // Main takes a turn while the background sub is still open, so
        // the sub's remaining entry lands after this one in append order.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "main while bg runs".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        // Sub resumes and concludes.
        {
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "bg final report".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        // Main concludes.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "main done".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        (dir, log)
    }

    fn event_values<'a>(events: impl IntoIterator<Item = &'a AgentEvent>) -> Vec<Value> {
        events
            .into_iter()
            .map(|e| serde_json::to_value(e).expect("event serializes"))
            .collect()
    }

    fn bracket_events(events: &[AgentEvent]) -> Vec<Value> {
        event_values(events.iter().filter(|e| {
            matches!(
                e,
                AgentEvent::SubAgentStart { .. } | AgentEvent::SubAgentEnd { .. }
            )
        }))
    }

    /// Every event whose `agent_id()` is `Main`. Bracket events report
    /// the parent, so this subsequence includes them.
    fn main_subsequence(events: &[AgentEvent]) -> Vec<Value> {
        event_values(
            events
                .iter()
                .filter(|e| matches!(e.agent_id(), AgentId::Main)),
        )
    }

    fn sub_reports(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgentEnd { report, .. } => Some(report.clone()),
                _ => None,
            })
            .collect()
    }

    /// Count of sub-agent-tagged events. Bracket events report the
    /// parent, so every `Sub(_)`-tagged event is projected content.
    fn sub_content_count(events: &[AgentEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e.agent_id(), AgentId::Sub(_)))
            .count()
    }

    /// Deferred replay is the full state machine with sub-agent content
    /// gated off: identical bracket sequence, identical non-empty
    /// reports, identical main subsequence, and zero sub-agent content
    /// events (full replay emits some).
    fn assert_deferred_matches_full(log: &ConversationLog) {
        let full: Vec<AgentEvent> = replay(log).collect();
        let deferred: Vec<AgentEvent> = replay_deferring_subs(log).collect();

        assert_eq!(
            bracket_events(&full),
            bracket_events(&deferred),
            "bracket sequence must be identical"
        );

        let reports = sub_reports(&full);
        assert_eq!(reports, sub_reports(&deferred), "reports must be identical");
        assert!(!reports.is_empty(), "the log has at least one sub run");
        assert!(
            reports.iter().all(|r| !r.is_empty()),
            "reports must be non-empty, got {reports:?}"
        );

        assert_eq!(
            sub_content_count(&deferred),
            0,
            "deferred mode withholds all sub content, got {deferred:#?}"
        );
        assert!(
            sub_content_count(&full) > 0,
            "full replay projects sub content"
        );

        assert_eq!(
            main_subsequence(&full),
            main_subsequence(&deferred),
            "main-agent subsequence must be identical"
        );
    }

    /// `project_thread` for `Sub(n)` reproduces exactly the
    /// `Sub(n)`-tagged content events full replay emits for that thread
    /// (bracket events report the parent, so they're already excluded).
    fn assert_project_thread_matches_full(log: &ConversationLog, n: usize) {
        let full: Vec<AgentEvent> = replay(log).collect();
        let expected = event_values(
            full.iter()
                .filter(|e| matches!(e.agent_id(), AgentId::Sub(m) if m == n)),
        );
        assert!(
            !expected.is_empty(),
            "full replay produced Sub({n}) content"
        );

        let head = log
            .latest_leaf(ThreadFilter::subagent(n))
            .expect("sub leaf");
        let conv = log.linearize(&head, ThreadFilter::subagent(n));
        let projected = event_values(project_thread(&conv, AgentId::Sub(n)).iter());

        assert_eq!(projected, expected, "project_thread parity for Sub({n})");
    }

    /// Deferred replay of a clean foreground sub run matches full replay
    /// on brackets and reports and withholds all sub content. The
    /// concrete report value pins the report-capture refactor, whose
    /// naive form (skipping sub projection wholesale) yields an empty
    /// report here.
    #[test]
    fn replay_deferring_subs_matches_full_for_foreground_sub() {
        let (_dir, log) = log_with_foreground_sub();
        assert_deferred_matches_full(&log);
        let full: Vec<AgentEvent> = replay(&log).collect();
        assert_eq!(sub_reports(&full), vec!["final sub report".to_string()]);
    }

    /// Same parity contract for a background sub whose entries interleave
    /// with the parent's, so the bracket opens and closes twice.
    #[test]
    fn replay_deferring_subs_matches_full_for_background_sub() {
        let (_dir, log) = log_with_background_sub();
        assert_deferred_matches_full(&log);
        let full: Vec<AgentEvent> = replay(&log).collect();
        assert_eq!(
            sub_reports(&full),
            vec!["sub step one".to_string(), "bg final report".to_string()],
        );
    }

    #[test]
    fn project_thread_matches_full_replay_for_foreground_sub() {
        let (_dir, log) = log_with_foreground_sub();
        assert_project_thread_matches_full(&log, 1);
    }

    /// Parity relies on the sub's append order matching its `parent_id`
    /// chain, which interleaving would break if `linearize` and replay
    /// disagreed. This pins the interleaved case.
    #[test]
    fn project_thread_matches_full_replay_for_background_sub() {
        let (_dir, log) = log_with_background_sub();
        assert_project_thread_matches_full(&log, 1);
    }

    /// A legacy log with no `SubAgentSpawn` entry still yields a
    /// `SubAgentStart` under `replay_deferring_subs`, synthesized by the
    /// `bracket_subagent` fallback at the sub's first message. The resume
    /// drain seeds `deferred_subs` from that event, so without the
    /// synthesized start a legacy sub-agent would never be marked deferred
    /// and never materialize on observe.
    #[test]
    fn replay_deferring_subs_emits_start_for_legacy_log() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.head().cloned().expect("head present")
        };
        // No `append_subagent_spawn`: the sub thread leads straight with its
        // task message, the legacy shape the fallback exists for.
        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let deferred: Vec<AgentEvent> = replay_deferring_subs(&log).collect();
        match deferred
            .iter()
            .find(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("deferred replay synthesizes a SubAgentStart for the legacy sub")
        {
            AgentEvent::SubAgentStart { child, task, .. } => {
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "subtask");
            }
            _ => unreachable!(),
        }
        // Deferred mode still withholds the sub's content events.
        assert_eq!(
            sub_content_count(&deferred),
            0,
            "deferred mode withholds the legacy sub's content, got {deferred:#?}"
        );
    }

    /// The concatenated text of every `MessageEnd` for `agent` (user
    /// and assistant), in order.
    fn agent_texts(events: &[AgentEvent], agent: AgentId) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageEnd { agent_id, message } if *agent_id == agent => {
                    let text = match message.as_stored_wire()? {
                        Message::User(u) => u
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                UserContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<String>(),
                        Message::Assistant(a) => a
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                AssistantContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<String>(),
                        Message::ToolResult(_) => return None,
                    };
                    Some(text)
                }
                _ => None,
            })
            .collect()
    }

    /// The concatenated text of every user `MessageEnd`, in order.
    fn user_texts(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                    Some(Message::User(u)) => {
                        let text = u
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                UserContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<String>();
                        Some(text)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn path_aware_replay_excludes_sibling_branch() {
        // Two user-thread branches off a common parent. Replay must
        // project only the branch the head is on, not the sibling. This
        // pins the concurrent-writer interleaving fix.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let common = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common").id
        };

        // Branch A off the common parent.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("alpha")).expect("alpha");
        }
        let alpha = log.head().cloned().expect("alpha head");

        // Rewind and grow branch B off the same parent (a sibling on disk).
        log.set_head(common).expect("rewind to common");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("beta")).expect("beta");
        }
        let beta = log.head().cloned().expect("beta head");

        // Head on branch A: only A's user messages replay.
        log.set_head(alpha).expect("head to alpha");
        let events: Vec<AgentEvent> = replay(&log).collect();
        assert_eq!(
            user_texts(&events),
            vec!["common".to_string(), "alpha".to_string()]
        );

        // Head on branch B: only B's user messages replay.
        log.set_head(beta).expect("head to beta");
        let events: Vec<AgentEvent> = replay(&log).collect();
        assert_eq!(
            user_texts(&events),
            vec!["common".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn path_aware_replay_includes_sub_on_path_excludes_sub_on_abandoned_branch() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let common = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common").id
        };

        // Branch A: an assistant turn that spawns sub-agent 1.
        let a_a = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "branch A".into(),
                text_signature: None,
            })]))
            .expect("a")
            .id
        };
        let spawn_a = log
            .append_subagent_spawn(1, a_a.clone(), "sub A task", false, &fallback_settings())
            .expect("spawn 1")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_a, 1);
            view.add_message(user_msg("sub A prompt")).expect("sub a u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub A report".into(),
                text_signature: None,
            })]))
            .expect("sub a a");
        }

        // Branch B off the common parent: another assistant turn spawns sub 2.
        log.set_head(common).expect("rewind to common");
        let a_b = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "branch B".into(),
                text_signature: None,
            })]))
            .expect("b")
            .id
        };
        let spawn_b = log
            .append_subagent_spawn(2, a_b, "sub B task", false, &fallback_settings())
            .expect("spawn 2")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_b, 2);
            view.add_message(user_msg("sub B prompt")).expect("sub b u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub B report".into(),
                text_signature: None,
            })]))
            .expect("sub b a");
        }

        // Active path is branch A: sub 1 replays, sub 2 does not.
        log.set_head(a_a).expect("head to branch A");
        let events: Vec<AgentEvent> = replay(&log).collect();

        let started = |n: usize| {
            events.iter().any(|e| {
                matches!(
                    e,
                    AgentEvent::SubAgentStart { child, .. } if *child == AgentId::Sub(n)
                )
            })
        };
        assert!(started(1), "sub anchored on the active path is included");
        assert!(
            !started(2),
            "sub anchored on an abandoned branch is excluded"
        );
        assert!(
            !events.iter().any(|e| e.agent_id() == AgentId::Sub(2)),
            "no Sub(2) content events leak in"
        );
    }

    #[test]
    fn path_aware_replay_disambiguates_colliding_sub_agent_ids() {
        // Two concurrent writers that both resume before spawning can
        // mint the SAME `Sub(1)` id on different branches (the counter is
        // seeded from `max_agent_id` at resume). Inclusion must key on
        // the sub's parent chain, not its id: only the run anchored on
        // the active branch may replay, even though the abandoned run
        // carries the same id.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let common = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common").id
        };

        // Branch A: an assistant turn spawns sub-agent 1.
        let a_a = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "branch A".into(),
                text_signature: None,
            })]))
            .expect("a")
            .id
        };
        let spawn_a = log
            .append_subagent_spawn(1, a_a.clone(), "sub A task", false, &fallback_settings())
            .expect("spawn A")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_a, 1);
            view.add_message(user_msg("sub A prompt")).expect("sub a u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub A report".into(),
                text_signature: None,
            })]))
            .expect("sub a a");
        }

        // Branch B off the common parent: another assistant turn spawns a
        // sub-agent reusing the SAME id 1, the collision this test pins.
        log.set_head(common).expect("rewind to common");
        let a_b = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "branch B".into(),
                text_signature: None,
            })]))
            .expect("b")
            .id
        };
        let spawn_b = log
            .append_subagent_spawn(1, a_b, "sub B task", false, &fallback_settings())
            .expect("spawn B")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_b, 1);
            view.add_message(user_msg("sub B prompt")).expect("sub b u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub B report".into(),
                text_signature: None,
            })]))
            .expect("sub b a");
        }

        // Head on branch A: only branch A's sub run replays, even though
        // the abandoned branch B run shares the id `Sub(1)`.
        log.set_head(a_a).expect("head to branch A");
        let events: Vec<AgentEvent> = replay(&log).collect();

        // Exactly one SubAgentStart, carrying branch A's task.
        let starts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SubAgentStart { child, task, .. } if *child == AgentId::Sub(1) => {
                    Some(task.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec!["sub A task"], "only branch A's sub starts");

        // Every Sub(1) message comes from branch A, none from branch B.
        let sub_text = agent_texts(&events, AgentId::Sub(1));
        assert!(
            sub_text.iter().any(|t| t == "sub A prompt"),
            "branch A sub prompt replays: {sub_text:?}"
        );
        assert!(
            sub_text.iter().any(|t| t == "sub A report"),
            "branch A sub report replays: {sub_text:?}"
        );
        assert!(
            !sub_text.iter().any(|t| t.contains("sub B")),
            "colliding-id sub on the abandoned branch must not leak: {sub_text:?}"
        );
    }

    #[test]
    fn path_aware_replay_includes_legacy_sub_without_spawn_root() {
        // A legacy sub thread leads with the task user message (no
        // SubAgentSpawn root). Anchored on the active path, it must
        // still be included.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common");
        }
        let a1 = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a")
            .id
        };
        // Legacy sub thread: first entry is the task user message,
        // anchored at the active-path assistant message.
        {
            let mut view = ConversationView::subagent(&mut log, a1, 1);
            view.add_message(user_msg("legacy sub task"))
                .expect("sub u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "legacy sub report".into(),
                text_signature: None,
            })]))
            .expect("sub a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::SubAgentStart { child, .. } if *child == AgentId::Sub(1)
            )),
            "legacy sub anchored on the path is included"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::MessageEnd { agent_id, message }
                    if *agent_id == AgentId::Sub(1)
                        && matches!(message.as_stored_wire(), Some(Message::User(_)))
            )),
            "the legacy sub's task message replays"
        );
    }

    /// A log that exercises every projection shape the durable tagging
    /// rules care about, with a sub-agent run left open at end of log.
    ///
    /// Append positions, which the suffix tests use as cursors:
    /// 1 system prompt, 2 seed model change, 3 user, 4 assistant with a
    /// tool call, 5 tool result, 6 mid-session thinking change,
    /// 7 assistant, 8 compaction, 9 sub-agent spawn root, 10 sub user,
    /// 11 sub assistant.
    fn open_sub_log() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("system prompt");
        log.append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
            .expect("seed model change");
        let first_kept = {
            let mut view = ConversationView::user(&mut log);
            let user = view.add_message(user_msg("hi")).expect("user msg");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "/tmp/x"}),
                })],
                10,
                5,
                0,
                0,
            ))
            .expect("assistant with tool call");
            view.add_message(tool_result_msg("call-1", "read_file", "body", None))
                .expect("tool result");
            user.id
        };
        log.append_thinking_change(ThreadFilter::USER, "high")
            .expect("mid-session thinking change");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "done".into(),
                    text_signature: None,
                })],
                20,
                7,
                0,
                0,
            ))
            .expect("second assistant");
        }
        log.append_compaction(ThreadFilter::USER, "summary".into(), first_kept, 500, None)
            .expect("compaction");
        let parent_head = log.head().cloned().expect("head present");
        let spawn = log
            .append_subagent_spawn(1, parent_head, "do thing", true, &sub_settings())
            .expect("spawn root");
        {
            let mut view = ConversationView::subagent(&mut log, spawn.id, 1);
            view.add_message(user_msg("subtask")).expect("sub user");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "sub reply".into(),
                    text_signature: None,
                })],
                3,
                2,
                0,
                0,
            ))
            .expect("sub assistant");
        }
        (dir, log)
    }

    /// The settings snapshot the spawn root of `open_sub_log` carries.
    /// Deliberately distinct from `fallback_settings` so a
    /// re-synthesized start can be told apart from a synthesized one.
    fn sub_settings() -> AgentSettings {
        AgentSettings {
            provider: "anthropic".to_string(),
            model_id: "claude-sub".to_string(),
            thinking: "medium".to_string(),
            thinking_display: String::new(),
            speed: "fast".to_string(),
            verbosity: "low".to_string(),
        }
    }

    /// A second settings snapshot, so a log with two concurrent runs can
    /// tell each run's remembered start from the other's.
    fn other_sub_settings() -> AgentSettings {
        AgentSettings {
            provider: "openai".to_string(),
            model_id: "gpt-sub".to_string(),
            thinking: "high".to_string(),
            thinking_display: String::new(),
            speed: "standard".to_string(),
            verbosity: "high".to_string(),
        }
    }

    /// Two background sub-agents spawned in one parent turn, interleaving
    /// with each other and with the parent in append order.
    ///
    /// Append positions: 1 system prompt, 2 user, 3 parent assistant,
    /// 4 spawn root of sub 1, 5 spawn root of sub 2, 6 sub-1 user, 7
    /// sub-2 user, 8 sub-1 assistant, 9 parent assistant, 10 sub-2
    /// assistant, 11 sub-1 report, 12 sub-2 report.
    fn log_with_two_background_subs() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let parent_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating twice".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head")
        };

        let mut first = log
            .append_subagent_spawn(
                1,
                parent_head.clone(),
                "first bg task",
                true,
                &sub_settings(),
            )
            .expect("spawn 1")
            .id;
        let mut second = log
            .append_subagent_spawn(
                2,
                parent_head,
                "second bg task",
                true,
                &other_sub_settings(),
            )
            .expect("spawn 2")
            .id;

        first = {
            let mut view = ConversationView::subagent(&mut log, first, 1);
            view.add_message(user_msg("first bg task"))
                .expect("sub 1 u");
            view.head().cloned().expect("sub 1 head")
        };
        second = {
            let mut view = ConversationView::subagent(&mut log, second, 2);
            view.add_message(user_msg("second bg task"))
                .expect("sub 2 u");
            view.head().cloned().expect("sub 2 head")
        };
        first = {
            let mut view = ConversationView::subagent(&mut log, first, 1);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub one step".into(),
                text_signature: None,
            })]))
            .expect("sub 1 step");
            view.head().cloned().expect("sub 1 head")
        };

        // The parent takes a turn while both subs are open, so each sub's
        // remaining entries land after it in append order.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "main while both run".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        second = {
            let mut view = ConversationView::subagent(&mut log, second, 2);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub two step".into(),
                text_signature: None,
            })]))
            .expect("sub 2 step");
            view.head().cloned().expect("sub 2 head")
        };
        {
            let mut view = ConversationView::subagent(&mut log, first, 1);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub one report".into(),
                text_signature: None,
            })]))
            .expect("sub 1 report");
        }
        {
            let mut view = ConversationView::subagent(&mut log, second, 2);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub two report".into(),
                text_signature: None,
            })]))
            .expect("sub 2 report");
        }

        (dir, log)
    }

    /// A legacy sub-agent run: no `SubAgentSpawn` root, the sub thread
    /// leads with its task user message.
    ///
    /// Append positions: 1 system prompt, 2 user, 3 parent assistant,
    /// 4 sub user, 5 sub assistant.
    fn log_with_legacy_sub() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let parent_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head")
        };
        {
            let mut view = ConversationView::subagent(&mut log, parent_head, 1);
            view.add_message(user_msg("legacy subtask")).expect("sub u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "legacy report".into(),
                text_signature: None,
            })]))
            .expect("sub a");
        }

        (dir, log)
    }

    /// A user-thread branch point with an abandoned sibling, head left on
    /// the active branch.
    ///
    /// Append positions: 1 system prompt, 2 user "common", 3 active
    /// assistant, 4 abandoned user, 5 abandoned assistant, 6 active user,
    /// 7 active assistant. Entries 4 and 5 are off the head's path, so
    /// the projection skips them and its tagged positions have a gap in
    /// the middle rather than only at the front.
    fn log_with_abandoned_sibling_branch() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let common = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common").id
        };
        let active = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "active reply".into(),
                text_signature: None,
            })]))
            .expect("active reply")
            .id
        };

        log.set_head(common).expect("rewind to the branch point");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("abandoned"))
                .expect("abandoned u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "abandoned reply".into(),
                text_signature: None,
            })]))
            .expect("abandoned a");
        }

        log.set_head(active)
            .expect("head back on the active branch");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("more")).expect("more");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "final reply".into(),
                text_signature: None,
            })]))
            .expect("final reply");
        }

        (dir, log)
    }

    /// One assistant turn carrying three tool calls, followed by the
    /// three tool results.
    ///
    /// Append positions: 1 system prompt, 2 user, 3 assistant with the
    /// batch, 4/5/6 the tool results.
    fn tool_batch_log() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("read three files")).expect("u");
            view.add_message(assistant_msg(
                (1..=3)
                    .map(|n| {
                        AssistantContent::ToolCall(ToolCall {
                            id: format!("call-{n}"),
                            name: "read_file".into(),
                            arguments: json!({"path": format!("/tmp/{n}")}),
                        })
                    })
                    .collect(),
            ))
            .expect("a");
            for n in 1..=3 {
                view.add_message(tool_result_msg(
                    &format!("call-{n}"),
                    "read_file",
                    &format!("body {n}"),
                    None,
                ))
                .expect("tool result");
            }
        }
        (dir, log)
    }

    /// A settings change on a sub-agent's own thread, mid-run.
    ///
    /// Append positions: 1 system prompt, 2 user, 3 parent assistant,
    /// 4 spawn root, 5 sub user, 6 sub assistant, 7 sub thinking change,
    /// 8 sub assistant.
    fn log_with_sub_settings_change() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let parent_head = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head")
        };
        let sub_head = log
            .append_subagent_spawn(1, parent_head, "subtask", false, &sub_settings())
            .expect("spawn root")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, sub_head, 1);
            view.add_message(user_msg("subtask")).expect("sub u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "working".into(),
                text_signature: None,
            })]))
            .expect("sub a");
        }
        log.append_thinking_change(ThreadFilter::subagent(1), "high")
            .expect("sub thinking change");
        {
            let leaf = log
                .latest_leaf(ThreadFilter::subagent(1))
                .expect("sub leaf");
            let mut view = ConversationView::subagent(&mut log, leaf, 1);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "sub report".into(),
                text_signature: None,
            })]))
            .expect("sub report");
        }

        (dir, log)
    }

    fn wire(event: &AgentEvent) -> Value {
        serde_json::to_value(event).expect("events serialize")
    }

    /// The sub-agent ids a projection should treat as still running.
    fn live(ids: impl IntoIterator<Item = usize>) -> BTreeSet<usize> {
        ids.into_iter().collect()
    }

    /// The projected event kinds of one entry position, for readable
    /// assertions about what a suffix contains.
    fn kinds(backfill: &Backfill) -> Vec<(Option<u64>, String)> {
        backfill
            .events
            .iter()
            .map(|projected| {
                let kind = wire(&projected.event)["type"]
                    .as_str()
                    .expect("tagged event type")
                    .to_string();
                (projected.entry.as_ref().map(|e| e.seq), kind)
            })
            .collect()
    }

    /// The wire form of every event in `backfill`, for comparing a
    /// projection against a replay.
    fn projected_values(backfill: &Backfill) -> Vec<Value> {
        backfill
            .events
            .iter()
            .map(|projected| wire(&projected.event))
            .collect()
    }

    /// With no cursor and no live runs, the suffix projection *is*
    /// dead-log replay: same events in the same order, EOF closes
    /// included. Every deviation is driven by a cursor or by `live_subs`,
    /// which is what lets the two paths share one state machine.
    fn assert_full_suffix_matches_replay(log: &ConversationLog, label: &str) {
        let replayed: Vec<Value> = replay(log).map(|event| wire(&event)).collect();
        let backfill = project_suffix(&log.snapshot(), None, &BTreeSet::new());
        assert_eq!(
            projected_values(&backfill),
            replayed,
            "suffix and replay disagree for the {label} log"
        );
        assert!(
            backfill.open_subs.is_empty(),
            "with no live runs every bracket is closed ({label})"
        );
    }

    #[test]
    fn a_cursorless_suffix_with_no_live_subs_equals_replay() {
        let fixtures: Vec<(&str, (PathBuf, ConversationLog))> = vec![
            ("seeded main-thread", seeded_log()),
            ("foreground sub", log_with_foreground_sub()),
            ("background sub", log_with_background_sub()),
            ("two background subs", log_with_two_background_subs()),
            ("legacy sub", log_with_legacy_sub()),
            (
                "abandoned sibling branch",
                log_with_abandoned_sibling_branch(),
            ),
            ("sub-thread settings change", log_with_sub_settings_change()),
            ("tool batch", tool_batch_log()),
            ("open sub", open_sub_log()),
        ];
        for (label, (_dir, log)) in &fixtures {
            assert_full_suffix_matches_replay(log, label);
        }
    }

    #[test]
    fn full_suffix_leaves_a_live_runs_bracket_open() {
        let (_dir, log) = open_sub_log();
        let replayed: Vec<Value> = replay(&log).map(|event| wire(&event)).collect();
        let backfill = project_suffix(&log.snapshot(), None, &live([1]));

        // Dead-log replay force-closes the bracket at EOF. A live
        // backfill must not, because the real `SubAgentEnd` for a running
        // sub is still coming (spec 6.5).
        let (last, head) = replayed.split_last().expect("replay is not empty");
        assert_eq!(last["type"], "sub_agent_end");
        assert_eq!(
            projected_values(&backfill),
            head,
            "the suffix is replay minus the EOF close"
        );
        assert_eq!(backfill.open_subs, live([1]));
    }

    /// A background sub-agent's entries straddle its parent's, so closing
    /// every open run on an agent transition would fabricate a
    /// `SubAgentEnd` for a run that is still going and then re-open its
    /// bracket from the legacy fallback.
    #[test]
    fn a_live_background_run_keeps_its_bracket_open_across_parent_entries() {
        let (_dir, log) = log_with_background_sub();
        let backfill = project_suffix(&log.snapshot(), None, &live([1]));

        assert!(
            !backfill
                .events
                .iter()
                .any(|projected| matches!(projected.event, AgentEvent::SubAgentEnd { .. })),
            "a live run is never concluded by the projection: {:?}",
            kinds(&backfill)
        );
        let starts: Vec<&AgentEvent> = backfill
            .events
            .iter()
            .map(|projected| &projected.event)
            .filter(|event| matches!(event, AgentEvent::SubAgentStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "the run opens exactly once: {starts:#?}");
        // The parent's interleaved turn is projected between the sub's
        // own entries, with the bracket still open around them.
        let main_between = backfill
            .events
            .iter()
            .filter(|projected| matches!(projected.event.agent_id(), AgentId::Main))
            .count();
        assert!(main_between > 0, "the parent's entries project too");
        assert_eq!(backfill.open_subs, live([1]));
    }

    #[test]
    fn two_live_background_runs_both_stay_open_with_their_real_starts() {
        let (_dir, log) = log_with_two_background_subs();
        let backfill = project_suffix(&log.snapshot(), None, &live([1, 2]));

        assert!(
            !backfill
                .events
                .iter()
                .any(|projected| matches!(projected.event, AgentEvent::SubAgentEnd { .. })),
            "neither live run is concluded: {:?}",
            kinds(&backfill)
        );
        let starts: Vec<(usize, String, bool, AgentSettings)> = backfill
            .events
            .iter()
            .filter_map(|projected| match &projected.event {
                AgentEvent::SubAgentStart {
                    child: AgentId::Sub(n),
                    task,
                    background,
                    settings,
                    ..
                } => Some((*n, task.clone(), *background, settings.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (1, "first bg task".to_string(), true, sub_settings()),
                (2, "second bg task".to_string(), true, other_sub_settings()),
            ],
            "each run keeps its spawn root's task, mode and settings"
        );
        assert_eq!(backfill.open_subs, live([1, 2]));
    }

    /// A finished background run is closed at the parent's next entry and
    /// re-opens at its own next one. The re-opened bracket has to carry the
    /// spawn root's task, mode and settings: a client seeds the child's footer
    /// from them, so a default snapshot leaves it with no model line at all.
    #[test]
    fn a_finished_background_run_reopens_with_its_spawn_root_s_start() {
        let (_dir, log) = log_with_two_background_subs();
        let backfill = project_suffix(&log.snapshot(), None, &BTreeSet::new());

        let starts: Vec<(usize, String, bool, AgentSettings)> = backfill
            .events
            .iter()
            .filter_map(|projected| match &projected.event {
                AgentEvent::SubAgentStart {
                    child: AgentId::Sub(n),
                    task,
                    background,
                    settings,
                    ..
                } => Some((*n, task.clone(), *background, settings.clone())),
                _ => None,
            })
            .collect();
        assert!(
            starts.len() > 2,
            "the interleaving closes and re-opens both runs: {starts:#?}",
        );
        for (n, task, background, settings) in starts {
            let (expected_task, expected_settings) = match n {
                1 => ("first bg task", sub_settings()),
                2 => ("second bg task", other_sub_settings()),
                other => panic!("unexpected sub {other}"),
            };
            assert_eq!(task, expected_task, "sub {n} keeps its task");
            assert!(background, "sub {n} keeps its mode");
            assert_eq!(settings, expected_settings, "sub {n} keeps its settings");
        }
    }

    /// The transition close is the only way a finished run's box gets
    /// concluded from a log (a conclusion is never persisted), so it must
    /// still fire for every run the caller does not name as live.
    #[test]
    fn a_finished_run_is_still_concluded() {
        let (_dir, log) = log_with_foreground_sub();
        let backfill = project_suffix(&log.snapshot(), None, &BTreeSet::new());

        let ends: Vec<(AgentId, String, SubAgentConclusion)> = backfill
            .events
            .iter()
            .filter_map(|projected| match &projected.event {
                AgentEvent::SubAgentEnd {
                    child,
                    report,
                    conclusion,
                    ..
                } => Some((*child, report.clone(), *conclusion)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ends,
            vec![(
                AgentId::Sub(1),
                "final sub report".to_string(),
                SubAgentConclusion::Completed,
            )]
        );
        assert!(backfill.open_subs.is_empty());
    }

    /// A cursor inside a live run whose bracket has already survived an
    /// interleaved parent entry re-synthesizes the start from the spawn
    /// root the run remembers, not from the legacy fallback.
    #[test]
    fn a_cursor_inside_an_interleaved_live_run_resynthesizes_its_real_start() {
        let (_dir, log) = log_with_two_background_subs();
        // Position 9 is the parent's interleaved turn: both runs opened
        // below it and both continue above it.
        let backfill = project_suffix(&log.snapshot(), Some(9), &live([1, 2]));

        let starts: Vec<(usize, String, bool, AgentSettings, Option<u64>)> = backfill
            .events
            .iter()
            .filter_map(|projected| match &projected.event {
                AgentEvent::SubAgentStart {
                    child: AgentId::Sub(n),
                    task,
                    background,
                    settings,
                    ..
                } => Some((
                    *n,
                    task.clone(),
                    *background,
                    settings.clone(),
                    projected.entry.as_ref().map(|entry| entry.seq),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (
                    2,
                    "second bg task".to_string(),
                    true,
                    other_sub_settings(),
                    None,
                ),
                (1, "first bg task".to_string(), true, sub_settings(), None),
            ],
            "each run re-synthesizes its own start, untagged, when its \
             first suffix entry arrives"
        );
        assert_eq!(backfill.open_subs, live([1, 2]));
    }

    #[test]
    fn a_cursor_of_zero_projects_the_whole_log() {
        let (_dir, log) = open_sub_log();
        let snapshot = log.snapshot();
        assert_eq!(
            kinds(&project_suffix(&snapshot, Some(0), &live([1]))),
            kinds(&project_suffix(&snapshot, None, &live([1]))),
        );
    }

    #[test]
    fn a_cursor_at_the_last_position_projects_nothing() {
        let (_dir, log) = open_sub_log();
        let snapshot = log.snapshot();
        let backfill = project_suffix(&snapshot, Some(snapshot.last_seq()), &live([1]));
        assert!(
            backfill.events.is_empty(),
            "a caught-up client gets an empty suffix: {:?}",
            kinds(&backfill)
        );
        assert_eq!(
            backfill.open_subs,
            live([1]),
            "an empty suffix still reports the live run, so the caller can \
             conclude it"
        );
    }

    /// Spec 6.5: a cursor beyond `last_seq` cannot name a position in
    /// this materialization, so it reads as no cursor at all. Silently
    /// serving an empty suffix instead would lose the client's whole
    /// history.
    #[test]
    fn a_cursor_beyond_the_last_position_projects_the_whole_log() {
        let (_dir, log) = open_sub_log();
        let snapshot = log.snapshot();
        let full = kinds(&project_suffix(&snapshot, None, &live([1])));
        for cursor in [snapshot.last_seq() + 1, u64::MAX] {
            assert_eq!(
                kinds(&project_suffix(&snapshot, Some(cursor), &live([1]))),
                full,
                "cursor {cursor} must fall back to a full backfill"
            );
        }
    }

    #[test]
    fn an_empty_log_projects_an_empty_backfill() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let log = ConversationLog::create(&persistence).expect("create log");
        let snapshot = log.snapshot();
        for cursor in [None, Some(0), Some(1), Some(u64::MAX)] {
            let backfill = project_suffix(&snapshot, cursor, &BTreeSet::new());
            assert!(backfill.events.is_empty(), "cursor {cursor:?}");
            assert!(backfill.open_subs.is_empty(), "cursor {cursor:?}");
        }
    }

    #[test]
    fn suffix_after_cursor_keeps_projection_state_complete() {
        let (_dir, log) = open_sub_log();
        // Cursor at the assistant message that carries the tool call, so
        // its tool_call map entry and its usage are below the cursor.
        let backfill = project_suffix(&log.snapshot(), Some(4), &live([1]));

        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "tool_execution_start".to_string()),
                (None, "message_start".to_string()),
                (Some(5), "message_end".to_string()),
                (None, "tool_execution_end".to_string()),
                (Some(6), "notice".to_string()),
                (None, "message_start".to_string()),
                (Some(7), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (Some(8), "compaction_end".to_string()),
                (Some(9), "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(10), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(11), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ],
            "the suffix starts at the first entry above the cursor"
        );
        assert!(
            backfill
                .events
                .iter()
                .all(|projected| projected.entry.as_ref().is_none_or(|entry| entry.seq > 4)),
            "no event of an entry at or below the cursor is emitted: {:?}",
            kinds(&backfill)
        );
        // The tool result's entry is the first one above the cursor, and
        // it resolves against a tool call the walk projected but dropped.
        match &backfill.events.first().expect("suffix is not empty").event {
            AgentEvent::ToolExecutionStart { tool, args, .. } => {
                assert_eq!(tool, "read_file", "not the ('tool', {{}}) fallback");
                assert_eq!(args, &json!({"path": "/tmp/x"}));
            }
            other => panic!("expected the tool result's execution start, got {other:?}"),
        }
        // The usage accumulator carries the below-cursor turn's tokens.
        let usage = backfill
            .events
            .iter()
            .find_map(|projected| match &projected.event {
                AgentEvent::UsageUpdate {
                    agent_id: AgentId::Main,
                    usage,
                } => Some(usage.clone()),
                _ => None,
            })
            .expect("the above-cursor assistant turn projects a UsageUpdate");
        assert_eq!(usage.turn_input, 20);
        assert_eq!(
            usage.accumulated_input, 10,
            "accumulated from the below-cursor turn"
        );
        assert_eq!(usage.accumulated_output, 5);
    }

    #[test]
    fn suffix_inside_an_open_sub_resynthesizes_its_start() {
        let (_dir, log) = open_sub_log();
        // Cursor inside the sub's run: its spawn root (9) and first
        // message (10) are below the cursor, its assistant turn is not.
        let backfill = project_suffix(&log.snapshot(), Some(10), &live([1]));

        let first = backfill.events.first().expect("suffix is not empty");
        match &first.event {
            AgentEvent::SubAgentStart {
                parent,
                child,
                task,
                background,
                settings,
            } => {
                assert_eq!(*parent, AgentId::Main);
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "do thing", "the spawn root's real task");
                assert!(*background, "the spawn root's real run mode");
                assert_eq!(settings, &sub_settings(), "the spawn root's settings");
            }
            other => panic!("expected a re-synthesized SubAgentStart, got {other:?}"),
        }
        assert!(
            first.entry.is_none(),
            "a bracketing frame whose spawn root is at or below the cursor \
             must not be tagged durable"
        );
        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(11), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ]
        );
        assert_eq!(backfill.open_subs, live([1]));
    }

    #[test]
    fn suffix_resynthesizes_the_start_of_a_run_opened_at_the_cursor() {
        let (_dir, log) = open_sub_log();
        // The spawn root sits exactly at the cursor, so it is dropped and
        // the run is open at the boundary just the same.
        let backfill = project_suffix(&log.snapshot(), Some(9), &live([1]));
        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(10), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(11), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ]
        );
    }

    /// A legacy run has no spawn root to tag, so its re-synthesized start
    /// is untagged for the same reason a spawn-rooted one is: the client
    /// has already applied everything at or below its cursor.
    #[test]
    fn a_legacy_run_resynthesizes_an_untagged_start_inside_the_run() {
        let (_dir, log) = log_with_legacy_sub();
        // Position 4 is the run's first entry (its task message), which
        // is also where the legacy fallback opened the bracket.
        let backfill = project_suffix(&log.snapshot(), Some(4), &live([1]));

        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(5), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ]
        );
        match &backfill.events[0].event {
            AgentEvent::SubAgentStart {
                child,
                task,
                background,
                settings,
                ..
            } => {
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "legacy subtask", "taken from the task message");
                assert!(!background, "a legacy log carries no run mode");
                assert_eq!(*settings, fallback_settings());
            }
            other => panic!("expected a re-synthesized SubAgentStart, got {other:?}"),
        }
        assert_eq!(backfill.open_subs, live([1]));
    }

    /// The runs a backfill reports are the ones it walked, so a caller
    /// concluding what the projection left open never touches an abandoned
    /// branch's sub-agent.
    #[test]
    fn a_backfill_reports_only_the_runs_on_the_projected_path() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");
        let common = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("common")).expect("common").id
        };

        // The active branch delegates to sub 1.
        let active = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "active".into(),
                text_signature: None,
            })]))
            .expect("active")
            .id
        };
        let spawn_active = log
            .append_subagent_spawn(
                1,
                active.clone(),
                "active task",
                false,
                &fallback_settings(),
            )
            .expect("spawn 1")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_active, 1);
            view.add_message(user_msg("active prompt")).expect("sub u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "active report".into(),
                text_signature: None,
            })]))
            .expect("sub a");
        }

        // An abandoned sibling delegates to sub 2.
        log.set_head(common).expect("rewind to the branch point");
        let abandoned = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "abandoned".into(),
                text_signature: None,
            })]))
            .expect("abandoned")
            .id
        };
        let spawn_abandoned = log
            .append_subagent_spawn(2, abandoned, "abandoned task", false, &fallback_settings())
            .expect("spawn 2")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn_abandoned, 2);
            view.add_message(user_msg("abandoned prompt"))
                .expect("sub u");
        }
        log.set_head(active).expect("head back on the active path");

        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.sub_agent_ids(),
            live([1, 2]),
            "the log names both runs, on either branch",
        );
        let backfill = project_suffix(&snapshot, None, &BTreeSet::new());
        assert_eq!(
            backfill.subs,
            live([1]),
            "only the run anchored on the projected path is reported",
        );
        assert!(backfill.open_subs.is_empty(), "no run was said to be live");

        // A live run is reported too, and stays open.
        let backfill = project_suffix(&snapshot, None, &live([1]));
        assert_eq!(backfill.subs, live([1]));
        assert_eq!(backfill.open_subs, live([1]));
    }

    #[test]
    fn suffix_below_a_spawn_root_keeps_its_start_durable() {
        let (_dir, log) = open_sub_log();
        // The spawn root is above the cursor, so its `SubAgentStart` is the
        // entry's durable frame and must be emitted with its position: a
        // client whose cursor stops short of the spawn would otherwise
        // never learn the sub exists.
        let backfill = project_suffix(&log.snapshot(), Some(8), &live([1]));
        let first = backfill.events.first().expect("suffix is not empty");
        assert!(matches!(first.event, AgentEvent::SubAgentStart { .. }));
        assert_eq!(
            first.entry.as_ref().map(|entry| entry.seq),
            Some(9),
            "the spawn root's own position"
        );
    }

    #[test]
    fn a_sub_thread_settings_change_projects_a_tagged_notice_in_place() {
        let (_dir, log) = log_with_sub_settings_change();
        let backfill = project_suffix(&log.snapshot(), None, &live([1]));

        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "message_start".to_string()),
                (Some(2), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(3), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (Some(4), "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(5), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(6), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (Some(7), "notice".to_string()),
                (None, "message_start".to_string()),
                (Some(8), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ],
            "the notice sits between the sub's two turns"
        );
        let notice = backfill
            .events
            .iter()
            .find(|projected| matches!(projected.event, AgentEvent::Notice { .. }))
            .expect("the mid-run settings entry projects a notice");
        match &notice.event {
            AgentEvent::Notice { agent_id, text } => {
                assert_eq!(*agent_id, AgentId::Sub(1), "on the sub's own thread");
                assert_eq!(text, "Thinking effort set to high.");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn project_settings_entry_answers_what_the_projection_emits() {
        let (_dir, log) = open_sub_log();
        let snapshot = log.snapshot();
        let entry_at = |seq: u64| {
            snapshot
                .entry_in_append_order(usize::try_from(seq).expect("fits usize") - 1)
                .expect("position exists")
                .id
                .clone()
        };

        // Position 2 is the seed model change, ahead of any message on
        // its thread, so it projects nothing and the host must not
        // synthesize a notice for it.
        assert!(snapshot.project_settings_entry(&entry_at(2)).is_none());

        // Position 6 is the mid-session thinking change.
        let projected = snapshot
            .project_settings_entry(&entry_at(6))
            .expect("a mid-session settings entry projects a notice");
        let from_backfill = project_suffix(&snapshot, Some(5), &live([1]))
            .events
            .into_iter()
            .find(|tagged| tagged.entry.as_ref().is_some_and(|entry| entry.seq == 6))
            .expect("the backfill tags that entry")
            .event;
        assert_eq!(
            wire(&projected),
            wire(&from_backfill),
            "the answer must be exactly what a backfill regenerates"
        );

        // A message entry is not a settings entry.
        assert!(snapshot.project_settings_entry(&entry_at(3)).is_none());
    }

    #[test]
    fn sub_agent_ids_reports_every_sub_in_the_log() {
        let (_dir, log) = log_with_two_background_subs();
        assert_eq!(log.snapshot().sub_agent_ids(), live([1, 2]));

        // A finished run counts too: the host sweeps concluded boxes as
        // well as running ones.
        let (_dir, log) = log_with_foreground_sub();
        assert_eq!(log.snapshot().sub_agent_ids(), live([1]));

        let (_dir, log) = seeded_log();
        assert!(log.snapshot().sub_agent_ids().is_empty());
    }

    #[test]
    fn full_suffix_tags_exactly_the_durable_events() {
        let (_dir, log) = open_sub_log();
        let snapshot = log.snapshot();
        let backfill = project_suffix(&snapshot, None, &live([1]));

        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "message_start".to_string()),
                (Some(3), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(4), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (None, "tool_execution_start".to_string()),
                (None, "message_start".to_string()),
                (Some(5), "message_end".to_string()),
                (None, "tool_execution_end".to_string()),
                (Some(6), "notice".to_string()),
                (None, "message_start".to_string()),
                (Some(7), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (Some(8), "compaction_end".to_string()),
                (Some(9), "sub_agent_start".to_string()),
                (None, "message_start".to_string()),
                (Some(10), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(11), "message_end".to_string()),
                (None, "usage_update".to_string()),
            ]
        );

        assert_tags_name_their_own_entry(&snapshot, &backfill);
    }

    /// Every tag names its own entry, and no entry carries two: that is
    /// what makes a client's per-frame cursor advance well-defined.
    fn assert_tags_name_their_own_entry(snapshot: &LogSnapshot, backfill: &Backfill) {
        let mut seen: Vec<u64> = Vec::new();
        for projected in &backfill.events {
            let Some(entry) = &projected.entry else {
                continue;
            };
            let index = usize::try_from(entry.seq).expect("fits usize") - 1;
            let logged = snapshot
                .entry_in_append_order(index)
                .expect("tagged position exists in the log");
            assert_eq!(entry.id, logged.id, "tag names its own entry");
            assert!(
                !seen.contains(&entry.seq),
                "position {} is tagged twice",
                entry.seq
            );
            seen.push(entry.seq);
            if let AgentEvent::MessageEnd { message, .. } = &projected.event {
                assert_eq!(
                    entry.id,
                    message.id(),
                    "a MessageEnd's tag is its message's own id"
                );
            }
        }
    }

    /// A tool batch persists one entry per result, so the projection must
    /// tag each result's own `MessageEnd` and nothing else.
    #[test]
    fn a_tool_result_batch_tags_each_result_entry_once() {
        let (_dir, log) = tool_batch_log();
        let snapshot = log.snapshot();
        let backfill = project_suffix(&snapshot, None, &BTreeSet::new());

        assert_eq!(
            kinds(&backfill),
            vec![
                (None, "message_start".to_string()),
                (Some(2), "message_end".to_string()),
                (None, "message_start".to_string()),
                (Some(3), "message_end".to_string()),
                (None, "usage_update".to_string()),
                (None, "tool_execution_start".to_string()),
                (None, "message_start".to_string()),
                (Some(4), "message_end".to_string()),
                (None, "tool_execution_end".to_string()),
                (None, "tool_execution_start".to_string()),
                (None, "message_start".to_string()),
                (Some(5), "message_end".to_string()),
                (None, "tool_execution_end".to_string()),
                (None, "tool_execution_start".to_string()),
                (None, "message_start".to_string()),
                (Some(6), "message_end".to_string()),
                (None, "tool_execution_end".to_string()),
            ]
        );
        assert_tags_name_their_own_entry(&snapshot, &backfill);
    }

    #[test]
    fn seed_entries_leave_a_gap_before_the_first_tagged_position() {
        let (_dir, log) = open_sub_log();
        let backfill = project_suffix(&log.snapshot(), None, &live([1]));
        let first_tagged = backfill
            .events
            .iter()
            .find_map(|projected| projected.entry.as_ref())
            .expect("the suffix tags at least one event");
        // The system prompt and the seed model change project nothing, so
        // seqs start above 1 and are not contiguous. Clients must tolerate
        // that (spec 6.4).
        assert_eq!(first_tagged.seq, 3);
    }

    /// An abandoned branch's entries occupy interior positions that
    /// project nothing, so a client cannot do gap detection on seq at
    /// all, not even "the gaps are all at the front" (spec 6.4).
    #[test]
    fn an_abandoned_branch_leaves_an_interior_gap_in_the_tagged_positions() {
        let (_dir, log) = log_with_abandoned_sibling_branch();
        let snapshot = log.snapshot();
        let backfill = project_suffix(&snapshot, None, &BTreeSet::new());

        let tagged: Vec<u64> = backfill
            .events
            .iter()
            .filter_map(|projected| projected.entry.as_ref().map(|entry| entry.seq))
            .collect();
        assert_eq!(
            tagged,
            vec![2, 3, 6, 7],
            "positions 4 and 5 are the abandoned branch"
        );
        assert_tags_name_their_own_entry(&snapshot, &backfill);
        assert_eq!(
            user_texts(
                &backfill
                    .events
                    .iter()
                    .map(|p| p.event.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["common".to_string(), "more".to_string()],
            "the abandoned branch's messages are not projected"
        );
    }
}
