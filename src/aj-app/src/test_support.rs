//! TUI-agnostic test builders shared with downstream crates' tests.
//!
//! Gated behind the `test-support` feature (not `#[cfg(test)]`) so
//! consuming crates can build the same scripted-provider and run-config
//! fixtures in their own tests. A crate's `cfg(test)` items are not
//! visible across crate boundaries, which is why this is a feature.
//!
//! Frontend-bound helpers (a `Terminal` stub, the interactive
//! `SessionWorld` builder) stay in the consuming binary.

use std::collections::{BTreeMap, BTreeSet};
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
/// into it: the equality oracle for reducer-equivalence tests.
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

    /// Line-oriented rendering, so a mismatch reads as a diff instead of
    /// one very long line.
    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("canonical state serializes")
    }
}

/// Assert two canonical states are equal, reporting a mismatch as two
/// line-oriented JSON documents plus `context`.
#[track_caller]
pub fn assert_canonical_eq(left: &CanonicalState, right: &CanonicalState, context: &str) {
    if left != right {
        panic!(
            "canonical states differ ({context})\nleft:\n{}\nright:\n{}",
            left.to_pretty_json(),
            right.to_pretty_json(),
        );
    }
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
