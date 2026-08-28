//! TUI-agnostic test builders shared with downstream crates' tests.
//!
//! Gated behind the `test-support` feature (not `#[cfg(test)]`) so
//! consuming crates can build the same scripted-provider and run-config
//! fixtures in their own tests. A crate's `cfg(test)` items are not
//! visible across crate boundaries, which is why this is a feature.
//!
//! Frontend-bound helpers (a `Terminal` stub, the interactive
//! `SessionWorld` builder) stay in the consuming binary.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::Agent;
use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::{AgentId, AgentSettings, CompactionPhase};
use aj_agent::message::{TaskNotificationKind, TaskOutcome};
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use aj_conf::Config;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent,
};
use aj_session::{
    AppendHandoff, ConversationLog, ConversationPersistence, TaggedEvent, persisting_forwarder,
};
use aj_wire::QueueState;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::chat::{ChatState, Entry, EntryId, EntryKind, NoticeLevel, SubAgentStatus, ToolStatus};
use crate::client::SessionClient;
use crate::footer::ContextUsage;
use crate::session::{AgentLifecycle, SessionCore, SessionEntry, SessionSpec};
use crate::session_setup::RunConfigSnapshot;

/// [`ModelInfo`] consistent with the identity [`ScriptedProvider`]
/// stamps on every emitted partial, so the agent sees a coherent
/// provider identity in tests.
pub fn scripted_model_info() -> ModelInfo {
    ModelInfo {
        id: "scripted".to_string(),
        name: "scripted".to_string(),
        family: None,
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        input: vec![aj_models::registry::InputModality::Text],
        cost: aj_models::registry::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
    }
}

/// Finalized text-only assistant reply for scripting one-turn
/// conversations (no tool calls, `EndTurn`).
pub fn finalized_text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        model: "scripted".to_string(),
        account: None,
        response_id: Some("test-msg".to_string()),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        error: None,
        timestamp: 0,
    }
}

/// [`finalized_text_message`] carrying a non-zero input-token `usage`,
/// so occupancy-driven triggers see a real context size.
pub fn finalized_text_message_with_usage(text: &str, input_tokens: u64) -> AssistantMessage {
    let mut m = finalized_text_message(text);
    m.usage.input = input_tokens;
    m
}

/// Run-config snapshot over a [`ScriptedProvider`] replaying
/// `messages`. `ExhaustedBehavior::Panic` makes any unscripted
/// extra inference fail loudly.
pub fn scripted_run_config(messages: Vec<AssistantMessage>) -> Arc<StdMutex<RunConfigSnapshot>> {
    Arc::new(StdMutex::new(RunConfigSnapshot {
        provider: Arc::new(
            ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
        ),
        model_info: Arc::new(scripted_model_info()),
        stream_options: StreamOptions::default(),
        thinking: None,
        thinking_display: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }))
}

/// Like [`scripted_run_config`] but with a non-zero `context_window`,
/// for tests that exercise occupancy-driven triggers (threshold
/// compaction, silent overflow).
pub fn scripted_run_config_with_window(
    messages: Vec<AssistantMessage>,
    context_window: u64,
) -> Arc<StdMutex<RunConfigSnapshot>> {
    let mut model_info = scripted_model_info();
    model_info.context_window = context_window;
    Arc::new(StdMutex::new(RunConfigSnapshot {
        provider: Arc::new(
            ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
        ),
        model_info: Arc::new(model_info),
        stream_options: StreamOptions::default(),
        thinking: None,
        thinking_display: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }))
}

/// Build a headless agent over a fresh (`Create`) session by running
/// the frontend-agnostic session setup through [`SessionCore::build`],
/// then decomposing the core into the owned agent, its shared log, and
/// the persistence subscription. This keeps the seeding sequence in one
/// place: the same path a live interactive session takes.
///
/// The returned [`SubscriptionHandle`] must be kept alive for the life
/// of the agent: dropping it detaches the persistence listener, so a
/// caller that needs driven turns persisted (compaction reads the log)
/// binds it rather than discarding it.
pub fn build_test_agent(
    persistence: &ConversationPersistence,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
) -> (Agent, Arc<TokioMutex<ConversationLog>>, SubscriptionHandle) {
    let config = Config::default();
    let spec = SessionSpec::Create {
        entry: SessionEntry::Startup,
    };
    // The core owns its own run config (one per session), so the shared
    // fixture is cloned in rather than handed over. Tests that stage a
    // change through `run_config` after this therefore need
    // `core.run_config`, not the fixture.
    let snapshot = run_config
        .lock()
        .expect("run config mutex poisoned")
        .clone();
    let (core, _seed) = SessionCore::build(&config, snapshot, persistence, &spec, None)
        .expect("build session core");
    core.into_test_agent()
}

/// [`build_test_agent`] with the durable tagger in place of the plain
/// persistence listener: every event the agent emits arrives on the
/// returned receiver paired with the log entry it appended, which is the
/// shape a session host fans out to its clients (spec 6.4).
///
/// The forwarder persists as well, so the plain listener is dropped here.
/// Keeping both would append every message twice.
pub fn build_tagged_test_agent(
    persistence: &ConversationPersistence,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
) -> (
    Agent,
    Arc<TokioMutex<ConversationLog>>,
    SubscriptionHandle,
    UnboundedReceiver<TaggedEvent>,
) {
    let (agent, log, persistence_handle) = build_test_agent(persistence, run_config);
    drop(persistence_handle);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = agent.subscribe(persisting_forwarder(
        Arc::clone(&log),
        AppendHandoff::default(),
        tx,
    ));
    (agent, log, handle, rx)
}

/// Comparable projection of a [`ChatState`] and the client that folded
/// into it: the equality oracle for reducer-equivalence tests, and the
/// full form of the canonical form's two tiers (spec 11.2).
///
/// Two states that would render the same conversation project onto the
/// same value, so `assert_eq!` on this type answers "did these two folds
/// arrive at the same place". `ChatState` itself is not comparable (no
/// `PartialEq`, and the reducer stamps `Instant::now()` into entries), so
/// this projection is where equality becomes writable. It is also
/// `Serialize`, which is what [`CanonicalState::to_pretty_json`] uses to
/// turn a mismatch into a line-oriented artifact instead of one very long
/// `{:?}` line.
///
/// The full form is the tier for two clients that both saw every frame.
/// A client that was disconnected is held to [`ConvergentState`] instead,
/// which masks what a re-attach cannot recover.
///
/// Deliberately not covered:
///
/// - Wall-clock fields. An `Instant` differs between any two runs.
///   Where the distinction carries meaning it becomes a `finished` flag.
/// - The display flags and `active_view`. They are client-local view
///   state, set from config rather than from the event stream.
/// - Raw [`EntryId`](crate::chat::EntryId)s. Positional order carries the
///   same information without coupling the oracle to append counters,
///   which two folds of the same conversation legitimately differ on.
/// - A sub-agent box's `latest_activity`: transient detail a re-attach
///   quiesce drops, so two converged states may hold different values.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanonicalState {
    /// Every agent with a transcript or render bookkeeping, main first
    /// then subs in index order.
    pub agents: Vec<CanonicalAgent>,
    /// Where each sub-agent's box sits in its parent's transcript,
    /// keyed by sub index.
    pub sub_boxes: BTreeMap<usize, CanonicalLocation>,
    /// The background-task table in task-id order.
    pub tasks: Vec<CanonicalTask>,
    /// The pending messages the client is tracking, in canonical agent
    /// order.
    pub queue: Vec<CanonicalQueue>,
    /// Agents with an open `AgentStart`, in canonical agent order.
    pub running: Vec<AgentId>,
    /// Agents with an in-flight compaction, in canonical agent order.
    pub compacting: Vec<AgentId>,
}

/// One agent's transcript plus the accounting a footer renders for it and
/// the durable-identity bookkeeping the reducer keeps beside it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanonicalAgent {
    pub agent: AgentId,
    pub entries: Vec<CanonicalEntry>,
    /// The agent's settings snapshot, not a formatted model line: the
    /// line shows only model and thinking, which would make a `speed` or
    /// `verbosity` difference invisible to the oracle even though the
    /// `state` frame carries all four.
    pub settings: Option<AgentSettings>,
    pub context_usage: ContextUsage,
    pub compaction_phase: Option<CompactionPhase>,
    pub render: CanonicalRender,
}

/// One agent's durable-identity bookkeeping: what a re-applied event
/// would find. Covered as key sets plus a streaming flag, so the oracle
/// sees the state that decides "update in place or append" without
/// coupling to [`EntryId`](crate::chat::EntryId) counters.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CanonicalRender {
    /// The message a following `UsageUpdate` reports on.
    pub last_finalized_assistant: Option<String>,
    /// The `call_id`s that resolve to a cell.
    pub tool_calls: BTreeSet<String>,
    /// The message ids that resolve to a row.
    pub messages: BTreeSet<String>,
    /// Whether an assistant entry is open for streaming.
    pub streaming: bool,
}

/// Position of a transcript entry: whose transcript, and where in it.
///
/// `index` is `None` when the recorded entry no longer resolves, which
/// is a dangling-id bug the oracle surfaces rather than hides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CanonicalLocation {
    pub agent: AgentId,
    pub index: Option<usize>,
}

/// One tracked background task, without its timings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanonicalTask {
    pub id: TaskId,
    pub owner: AgentId,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
    /// The launching call, which is how the task's output finds its cell.
    pub call_id: String,
    /// Where that cell sits in the owner's transcript, so two states that
    /// differ in whether a `TaskOutput` will paint read as different.
    pub cell: Option<usize>,
}

/// One agent's pending messages, as the client is tracking them.
///
/// Only agents with something pending appear. An agent whose queue was
/// drained and an agent a client never heard about render the same empty
/// box, so keeping the emptied entry would report two converged clients as
/// different.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanonicalQueue {
    pub agent: AgentId,
    /// The queued messages themselves, through `serde_json` like the other
    /// payloads that carry no `PartialEq`. An `AgentMessage`'s id is
    /// `#[serde(skip)]`, so this compares the text a client would show and
    /// not the ids two folds legitimately mint differently.
    pub steering: Vec<Value>,
    pub follow_up: Vec<Value>,
}

/// One transcript entry, kind-tagged.
///
/// Payload types that carry no `PartialEq` (`AssistantMessage`,
/// `UserContent`, `ToolDetails`, `TokenUsage`) are projected through
/// `serde_json::to_value`, which every one of them supports, rather
/// than growing derives across the model crates.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum CanonicalEntry {
    User {
        message_id: Option<String>,
        content: Value,
    },
    Assistant {
        message_id: Option<String>,
        finalized: bool,
        message: Value,
    },
    Tool {
        call_id: String,
        tool: String,
        args: Value,
        status: ToolStatus,
        details: Option<Value>,
        content: Value,
        task: Option<TaskId>,
    },
    SubAgent {
        child: usize,
        task: String,
        status: SubAgentStatus,
        report: Option<String>,
        background: bool,
        /// Whether the box's runtime clock is frozen.
        finished: bool,
    },
    Compaction {
        tokens_before: u64,
        tokens_after: u64,
        summary: String,
        entry: Option<String>,
    },
    Notice {
        level: NoticeLevel,
        text: String,
        entry: Option<String>,
    },
    TurnUsage {
        agent: AgentId,
        after_message_id: Option<String>,
        usage: Value,
    },
    TaskNotification {
        message_id: Option<String>,
        label: String,
        kind: TaskNotificationKind,
        outcome: TaskOutcome,
        body: String,
    },
}

impl CanonicalEntry {
    /// Whether the row is a transient-only artifact: something the live
    /// stream carried once that no durable entry backs, so no backfill
    /// regenerates it.
    ///
    /// Two shapes qualify. A notice no log entry backs (every locally
    /// raised one) rides a reliable-transient frame, delivered exactly
    /// once (spec 6.4). An unfinalized assistant row is the in-flight
    /// streaming text, which the reducer's own quiesce drops on the way
    /// into a re-attach because nothing names it and the durable message
    /// replaces it.
    ///
    /// A notice that does carry an origin is not transient-only: the
    /// entry it derives from is on disk, and a backfill projects the
    /// notice again from it.
    pub fn is_transient_only(&self) -> bool {
        matches!(
            self,
            Self::Notice { entry: None, .. }
                | Self::Assistant {
                    finalized: false,
                    ..
                }
        )
    }
}

/// The convergent tier of the canonical form: a [`CanonicalState`] with
/// every transient-only artifact masked out (spec 11.2).
///
/// This is the tier a client that lost its connection can be held to. A
/// reliable-transient frame is delivered once and is never replayed
/// (spec 6.4), so a client disconnected across a transient's only
/// delivery window legitimately never has it and no re-attach can hand it
/// over later. Comparing the full form there would assert a promise the
/// protocol does not make. The no-fault comparisons keep the full form,
/// where both clients saw every frame and any difference is a real one.
///
/// The mask is narrow on purpose: it removes what
/// [`CanonicalEntry::is_transient_only`] names and nothing else. Notices
/// with a durable origin, every finalized row, the render indexes and all
/// the accounting stay under comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConvergentState(CanonicalState);

impl CanonicalState {
    /// Project the state `client` folded into `chat`.
    ///
    /// The client supplies lifecycle state. Queue and task snapshots live in
    /// `chat`, where a remote frontend can render them directly.
    pub fn of(chat: &ChatState, client: &SessionClient) -> Self {
        Self::of_reduced(chat, client.lifecycle())
    }

    /// Project a fold that went straight through [`reduce`](crate::chat::reduce)
    /// with no client around it.
    ///
    /// A direct reducer fold only changes the queue if its caller mirrors a
    /// `QueueUpdate` into `chat`, as [`SessionClient`] does.
    pub fn of_reduced(chat: &ChatState, lifecycle: &AgentLifecycle) -> Self {
        // The union of both maps: an agent can hold render bookkeeping
        // without a transcript, and a state the oracle skipped would be a
        // blind spot rather than a simplification.
        let mut ids: Vec<AgentId> = chat
            .transcripts
            .keys()
            .chain(chat.render.keys())
            .copied()
            .collect();
        ids.sort_by_key(|id| agent_order(*id));
        ids.dedup();
        let agents: Vec<CanonicalAgent> = ids
            .iter()
            .map(|&agent| CanonicalAgent {
                agent,
                entries: chat
                    .transcript(agent)
                    .map(|t| t.entries().iter().map(canonical_entry).collect())
                    .unwrap_or_default(),
                settings: chat.footers().settings(agent).cloned(),
                context_usage: chat.footers().context_usage(agent),
                compaction_phase: chat.compaction_phase(agent),
                render: chat
                    .render
                    .get(&agent)
                    .map(|render| CanonicalRender {
                        last_finalized_assistant: render.last_finalized_assistant.clone(),
                        tool_calls: render.tool_index.keys().cloned().collect(),
                        messages: render.message_index.keys().cloned().collect(),
                        streaming: render.current_assistant.is_some(),
                    })
                    .unwrap_or_default(),
            })
            .collect();

        let sub_boxes = chat
            .sub_boxes
            .iter()
            .map(|(&child, &(agent, entry))| {
                (
                    child,
                    CanonicalLocation {
                        agent,
                        index: position_of(chat, agent, entry),
                    },
                )
            })
            .collect();

        let tasks = chat
            .tasks()
            .iter()
            .map(|(&id, info)| CanonicalTask {
                id,
                owner: info.owner,
                kind: info.kind.clone(),
                label: info.label.clone(),
                status: info.status,
                call_id: info.call_id.clone(),
                cell: chat
                    .task_cell(id)
                    .and_then(|(owner, entry)| position_of(chat, owner, entry)),
            })
            .collect();

        let mut running = lifecycle.running_agents();
        running.sort_by_key(|id| agent_order(*id));
        let mut compacting = lifecycle.compacting_agents();
        compacting.sort_by_key(|id| agent_order(*id));

        Self {
            agents,
            sub_boxes,
            tasks,
            queue: canonical_queue(chat.queue()),
            running,
            compacting,
        }
    }

    /// The projection of one agent, for tests that assert on a single
    /// transcript.
    pub fn agent(&self, id: AgentId) -> Option<&CanonicalAgent> {
        self.agents.iter().find(|a| a.agent == id)
    }

    /// This state's [`ConvergentState`]: the same projection with every
    /// transient-only row taken out and the streaming flag that names one
    /// cleared.
    ///
    /// Dropping rows renumbers the transcript, so the recorded locations
    /// (a sub-agent's box, a task's launch cell) move with it. Without
    /// that, two clients that agree on everything but a masked notice
    /// would still read as different at the first location behind it.
    pub fn convergent(&self) -> ConvergentState {
        let mut state = self.clone();
        // The positions the mask took out, per agent, which is what the
        // locations below are renumbered against.
        let mut masked: HashMap<AgentId, Vec<usize>> = HashMap::new();
        for agent in &mut state.agents {
            let mut dropped = Vec::new();
            let mut kept = Vec::with_capacity(agent.entries.len());
            for (index, entry) in std::mem::take(&mut agent.entries).into_iter().enumerate() {
                if entry.is_transient_only() {
                    dropped.push(index);
                } else {
                    kept.push(entry);
                }
            }
            agent.entries = kept;
            // The flag names the streaming row the loop just dropped, so
            // it goes with it or the tier contradicts itself.
            agent.render.streaming = false;
            masked.insert(agent.agent, dropped);
        }
        for location in state.sub_boxes.values_mut() {
            location.index = renumber(masked.get(&location.agent), location.index);
        }
        for task in &mut state.tasks {
            task.cell = renumber(masked.get(&task.owner), task.cell);
        }
        ConvergentState(state)
    }

    /// Line-oriented rendering, so a mismatch reads as a diff instead of
    /// one very long line.
    pub fn to_pretty_json(&self) -> String {
        pretty(self)
    }
}

/// Where `index` lands once `masked`'s positions are taken out of the
/// transcript, `None` when `index` is one of them.
///
/// A masked row is no longer nameable, and the oracle reports that the
/// same way it reports a dangling id. No location names one today (a box
/// and a launch cell are both durable rows), so that arm is a definition
/// rather than a case with a caller.
fn renumber(masked: Option<&Vec<usize>>, index: Option<usize>) -> Option<usize> {
    let index = index?;
    let masked = masked?;
    if masked.contains(&index) {
        return None;
    }
    Some(index - masked.iter().filter(|&&position| position < index).count())
}

/// Assert two canonical states are equal, reporting a mismatch as two
/// line-oriented JSON documents plus `context`.
///
/// The full form: for two folds that both saw every frame.
#[track_caller]
pub fn assert_canonical_eq(left: &CanonicalState, right: &CanonicalState, context: &str) {
    assert_tier_eq(left, right, "canonical states", context);
}

/// Assert two convergent tiers are equal, reporting a mismatch the same
/// way [`assert_canonical_eq`] does.
///
/// For a fold that was disconnected: the transient-only artifacts it
/// could not have are masked out of both sides.
#[track_caller]
pub fn assert_convergent_eq(left: &ConvergentState, right: &ConvergentState, context: &str) {
    assert_tier_eq(left, right, "convergent tiers", context);
}

#[track_caller]
fn assert_tier_eq<T: PartialEq + Serialize>(left: &T, right: &T, tier: &str, context: &str) {
    if left != right {
        panic!(
            "{tier} differ ({context})\nleft:\n{}\nright:\n{}",
            pretty(left),
            pretty(right),
        );
    }
}

fn pretty<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("the canonical form serializes")
}

/// Every recorded [`EntryId`](crate::chat::EntryId) that no longer names
/// the entry it was recorded for, one description per problem.
///
/// A dangling id renders as a missing sub-agent box, an unroutable task,
/// or a duplicate cell, so the equivalence harness and the reducer's own
/// tests assert on this rather than eyeballing it. Empty for a consistent
/// state.
pub fn dangling_entry_ids(chat: &ChatState) -> Vec<String> {
    let mut problems = Vec::new();
    for (&n, &(parent, entry)) in &chat.sub_boxes {
        match chat.transcript(parent).and_then(|t| t.get(entry)) {
            None => problems.push(format!("sub_boxes[{n}] dangles")),
            Some(found) if !matches!(found.kind, EntryKind::SubAgent(_)) => {
                problems.push(format!("sub_boxes[{n}] does not name a box"));
            }
            Some(_) => {}
        }
    }
    for (&agent, render) in &chat.render {
        let resolves = |entry| chat.transcript(agent).and_then(|t| t.get(entry)).is_some();
        if let Some(entry) = render.current_assistant
            && !resolves(entry)
        {
            problems.push(format!("current_assistant for {agent:?} dangles"));
        }
        for (call_id, &entry) in &render.tool_index {
            if !resolves(entry) {
                problems.push(format!("tool_index[{call_id}] for {agent:?} dangles"));
            }
        }
        for (message_id, &entry) in &render.message_index {
            if !resolves(entry) {
                problems.push(format!("message_index[{message_id}] for {agent:?} dangles"));
            }
        }
    }
    // The task table's launch cells resolve through `tool_index`, which
    // the loop above already covers, so it needs no check of its own.
    problems
}

/// Panic unless every recorded entry id still resolves.
#[track_caller]
pub fn assert_no_dangling(chat: &ChatState) {
    let problems = dangling_entry_ids(chat);
    assert!(problems.is_empty(), "dangling entry ids: {problems:?}");
}

/// Total order over agents: main first, then subs by index.
fn agent_order(id: AgentId) -> (u8, usize) {
    match id {
        AgentId::Main => (0, 0),
        AgentId::Sub(n) => (1, n),
    }
}

/// Project a client's queue snapshot: the agents with something pending,
/// in canonical agent order.
///
/// Sorted rather than taken as given, because the two sources of a client's
/// queue disagree on order: `QueueUpdate` frames arrive in mutation order
/// and the queue read answers main first.
fn canonical_queue(queue: &QueueState) -> Vec<CanonicalQueue> {
    let mut queues: Vec<CanonicalQueue> = queue
        .queues
        .iter()
        .filter(|agent| !agent.steering.is_empty() || !agent.follow_up.is_empty())
        .map(|agent| CanonicalQueue {
            agent: agent.agent_id,
            steering: agent.steering.iter().map(json).collect(),
            follow_up: agent.follow_up.iter().map(json).collect(),
        })
        .collect();
    queues.sort_by_key(|queue| agent_order(queue.agent));
    queues
}

/// Where `entry` sits in `agent`'s transcript, `None` when it no longer
/// resolves.
fn position_of(chat: &ChatState, agent: AgentId, entry: EntryId) -> Option<usize> {
    chat.transcript(agent)?
        .entries()
        .iter()
        .position(|e| e.id == entry)
}

/// Project a payload that carries no `PartialEq` onto a comparable
/// value. Every one of them is `Serialize`, so this needs no new
/// derives in the model crates.
fn json<T: serde::Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).expect("transcript payloads serialize")
}

/// Project one transcript entry.
fn canonical_entry(entry: &Entry) -> CanonicalEntry {
    match &entry.kind {
        EntryKind::User(u) => CanonicalEntry::User {
            message_id: u.message_id.clone(),
            content: json(&u.content),
        },
        EntryKind::Assistant(a) => CanonicalEntry::Assistant {
            message_id: a.message_id.clone(),
            finalized: a.finalized,
            message: json(&a.message),
        },
        EntryKind::Tool(t) => CanonicalEntry::Tool {
            call_id: t.call_id.clone(),
            tool: t.tool.clone(),
            args: t.args.clone(),
            status: t.status,
            details: t.details.as_ref().map(json),
            content: json(&*t.content),
            task: t.task,
        },
        EntryKind::SubAgent(s) => CanonicalEntry::SubAgent {
            child: s.child,
            task: s.task.clone(),
            status: s.status,
            report: s.report.clone(),
            background: s.background,
            finished: s.finished_at.is_some(),
        },
        EntryKind::Compaction(c) => CanonicalEntry::Compaction {
            tokens_before: c.tokens_before,
            tokens_after: c.tokens_after,
            summary: c.summary.clone(),
            entry: c.entry.clone(),
        },
        EntryKind::Notice(n) => CanonicalEntry::Notice {
            level: n.level,
            text: n.text.clone(),
            entry: n.entry.clone(),
        },
        EntryKind::TurnUsage(u) => CanonicalEntry::TurnUsage {
            agent: u.agent_id,
            after_message_id: u.after_message_id.clone(),
            usage: json(&u.usage),
        },
        EntryKind::TaskNotification(n) => CanonicalEntry::TaskNotification {
            message_id: n.message_id.clone(),
            label: n.label.clone(),
            kind: n.kind,
            outcome: n.outcome,
            body: n.body.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aj_agent::events::AgentEvent;
    use aj_agent::tool::TaskKind;
    use aj_models::streaming::AssistantMessageEvent;

    use crate::chat::reduce;

    /// The notice a compact that found nothing to compact raises: the
    /// canonical reliable-transient, appending no entry and carrying no
    /// durable identity.
    const TRANSIENT: &str = "Nothing to compact.";

    fn agent_settings() -> AgentSettings {
        AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            thinking_display: "default".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    /// A fold of one run, optionally with a locally raised `notice` at the
    /// head of it.
    ///
    /// The rows behind the notice are the point: a sub-agent box and a
    /// task's launch cell both record where they sit, so a mask that drops
    /// a row without renumbering leaves them naming the wrong one.
    fn folded(notice: Option<&str>) -> CanonicalState {
        let mut chat = ChatState::new(agent_settings(), 200_000, Arc::new(Vec::new()));
        let mut lifecycle = AgentLifecycle::default();
        let mut apply = |chat: &mut ChatState, event| {
            let _ = reduce(chat, &mut lifecycle, event, None);
        };
        if let Some(text) = notice {
            apply(
                &mut chat,
                AgentEvent::Notice {
                    agent_id: AgentId::Main,
                    text: text.to_string(),
                },
            );
        }
        apply(
            &mut chat,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "look into it".to_string(),
                background: false,
                settings: agent_settings(),
            },
        );
        apply(
            &mut chat,
            AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
                args: serde_json::json!({"command": "sleep 30"}),
            },
        );
        apply(
            &mut chat,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "call-1".to_string(),
                kind: TaskKind::Bash {
                    command: "sleep 30".to_string(),
                },
                label: "sleep 30".to_string(),
            },
        );
        CanonicalState::of_reduced(&chat, &lifecycle)
    }

    /// The property the fault-injection sweep rests on: a client that was
    /// disconnected across a transient notice's only delivery window lands
    /// where a client that got it lands, once both are masked.
    #[test]
    fn the_convergent_tier_masks_a_notice_no_entry_backs() {
        let with = folded(Some(TRANSIENT));
        let without = folded(None);

        // Name the harm the renumbering answers before the whole-state
        // comparison runs into it: every row behind the notice sits one
        // position further down.
        let box_at = |state: &CanonicalState| state.sub_boxes[&1].index;
        assert_eq!(
            box_at(&with),
            box_at(&without).map(|index| index + 1),
            "the notice pushed the rows behind it down",
        );
        assert_convergent_eq(
            &with.convergent(),
            &without.convergent(),
            "a fold that missed a transient notice",
        );
    }

    /// The other half of the tier distinction: the full form is where a
    /// transient notice is still compared, so two folds that both saw
    /// every frame are held to it.
    #[test]
    fn the_full_form_keeps_the_notice_the_convergent_tier_masks() {
        let with = folded(Some(TRANSIENT));
        let main = with.agent(AgentId::Main).expect("a main transcript");
        assert!(
            main.entries.iter().any(|entry| matches!(
                entry,
                CanonicalEntry::Notice { text, entry: None, .. } if text == TRANSIENT
            )),
            "the full form holds the transient notice: {:?}",
            main.entries,
        );
        assert_ne!(
            with,
            folded(None),
            "so two folds that differ only in it read as different",
        );
    }

    /// The mask is narrow: a notice a log entry backs is regenerated by a
    /// backfill, so a re-attached client has it and both tiers compare it.
    #[test]
    fn the_convergent_tier_keeps_a_notice_a_log_entry_backs() {
        let mut chat = ChatState::new(agent_settings(), 200_000, Arc::new(Vec::new()));
        let mut lifecycle = AgentLifecycle::default();
        let _ = reduce(
            &mut chat,
            &mut lifecycle,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "Thinking level set to high.".to_string(),
            },
            Some(&"entry-7".to_string()),
        );
        let projected = CanonicalState::of_reduced(&chat, &lifecycle);
        let empty = CanonicalState::of_reduced(
            &ChatState::new(agent_settings(), 200_000, Arc::new(Vec::new())),
            &AgentLifecycle::default(),
        );

        assert_ne!(
            projected.convergent(),
            empty.convergent(),
            "a projected state notice survives the mask",
        );
    }

    /// The other transient-only artifact: the in-flight streaming row and
    /// the flag that names it.
    #[test]
    fn the_convergent_tier_masks_the_in_flight_streaming_row() {
        let mut chat = ChatState::new(agent_settings(), 200_000, Arc::new(Vec::new()));
        let mut lifecycle = AgentLifecycle::default();
        let mut partial = finalized_text_message("half a th");
        partial.response_id = None;
        let _ = reduce(
            &mut chat,
            &mut lifecycle,
            AgentEvent::MessageUpdate {
                agent_id: AgentId::Main,
                message: aj_agent::message::AgentMessage::wire(
                    aj_models::types::Message::Assistant(partial.clone()),
                ),
                event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "half a th".to_string(),
                    partial,
                },
            },
            None,
        );
        let streaming = CanonicalState::of_reduced(&chat, &lifecycle);
        let quiet = CanonicalState::of_reduced(
            &ChatState::new(agent_settings(), 200_000, Arc::new(Vec::new())),
            &AgentLifecycle::default(),
        );

        assert!(
            streaming
                .agent(AgentId::Main)
                .expect("a main transcript")
                .render
                .streaming,
            "the fold is mid-message",
        );
        assert_ne!(streaming, quiet, "the full form keeps the open row");
        assert_convergent_eq(
            &streaming.convergent(),
            &quiet.convergent(),
            "a fold whose in-flight text a re-attach would drop",
        );
    }
}
