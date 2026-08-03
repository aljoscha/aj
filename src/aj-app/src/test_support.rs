//! TUI-agnostic test builders shared with downstream crates' tests.
//!
//! Gated behind the `test-support` feature (not `#[cfg(test)]`) so
//! consuming crates can build the same scripted-provider and run-config
//! fixtures in their own tests. A crate's `cfg(test)` items are not
//! visible across crate boundaries, which is why this is a feature.
//!
//! Frontend-bound helpers (a `Terminal` stub, the interactive
//! `SessionWorld` builder) stay in the consuming binary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::Agent;
use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::{AgentId, CompactionPhase};
use aj_agent::message::{TaskNotificationKind, TaskOutcome};
use aj_agent::tool::{TaskId, TaskKind, TaskStatus};
use aj_conf::Config;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent,
};
use aj_session::{ConversationLog, ConversationPersistence};
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;

use crate::chat::{ChatState, Entry, EntryKind, NoticeLevel, SubAgentStatus, ToolStatus};
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
    let (core, _seed) = SessionCore::build(&config, run_config, persistence, &spec, None)
        .expect("build session core");
    core.into_test_agent()
}

/// Comparable projection of a [`ChatState`] and its [`AgentLifecycle`]:
/// the equality oracle for reducer-equivalence tests.
///
/// Two states that would render the same conversation project onto the
/// same value, so `assert_eq!` on this type answers "did these two folds
/// arrive at the same place". `ChatState` itself is not comparable (no
/// `PartialEq`, and the reducer stamps `Instant::now()` into entries), so
/// this projection is where equality becomes writable.
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
/// - A sub-agent box's `latest_activity` and the `pending_task_cells`
///   residue: transient detail a re-attach quiesce drops, so two
///   converged states may hold different values.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalState {
    /// Every agent with a transcript, main first then subs in index
    /// order.
    pub agents: Vec<CanonicalAgent>,
    /// Where each sub-agent's box sits in its parent's transcript,
    /// keyed by sub index.
    pub sub_boxes: BTreeMap<usize, CanonicalLocation>,
    /// The background-task table in task-id order.
    pub tasks: Vec<CanonicalTask>,
    /// Agents with an open `AgentStart`, in canonical agent order.
    pub running: Vec<AgentId>,
    /// Agents with an in-flight compaction, in canonical agent order.
    pub compacting: Vec<AgentId>,
}

/// One agent's transcript plus the accounting a footer renders for it.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalAgent {
    pub agent: AgentId,
    pub entries: Vec<CanonicalEntry>,
    pub model_line: Option<String>,
    pub context_usage: ContextUsage,
    pub compaction_phase: Option<CompactionPhase>,
}

/// Position of a transcript entry: whose transcript, and where in it.
///
/// `index` is `None` when the recorded entry no longer resolves, which
/// is a dangling-id bug the oracle surfaces rather than hides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalLocation {
    pub agent: AgentId,
    pub index: Option<usize>,
}

/// One tracked background task, without its timings.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalTask {
    pub id: TaskId,
    pub owner: AgentId,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
}

/// One transcript entry, kind-tagged.
///
/// Payload types that carry no `PartialEq` (`AssistantMessage`,
/// `UserContent`, `ToolDetails`, `TokenUsage`) are projected through
/// `serde_json::to_value`, which every one of them supports, rather
/// than growing derives across the model crates.
#[derive(Clone, Debug, PartialEq)]
pub enum CanonicalEntry {
    User {
        message_id: String,
        content: Value,
    },
    Assistant {
        message_id: String,
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
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    TurnUsage {
        agent: AgentId,
        after_message_id: Option<String>,
        usage: Value,
    },
    TaskNotification {
        message_id: String,
        label: String,
        kind: TaskNotificationKind,
        outcome: TaskOutcome,
        body: String,
    },
}

impl CanonicalState {
    /// Project `chat` and `lifecycle` onto their comparable form.
    pub fn of(chat: &ChatState, lifecycle: &AgentLifecycle) -> Self {
        let mut ids: Vec<AgentId> = chat.transcripts.keys().copied().collect();
        ids.sort_by_key(|id| agent_order(*id));
        let agents: Vec<CanonicalAgent> = ids
            .iter()
            .map(|&agent| CanonicalAgent {
                agent,
                entries: chat
                    .transcript(agent)
                    .map(|t| t.entries().iter().map(canonical_entry).collect())
                    .unwrap_or_default(),
                model_line: chat.footers().model_line(agent),
                context_usage: chat.footers().context_usage(agent),
                compaction_phase: chat.compaction_phase(agent),
            })
            .collect();

        let sub_boxes = chat
            .sub_boxes
            .iter()
            .map(|(&child, &(agent, entry))| {
                let index = chat
                    .transcript(agent)
                    .and_then(|t| t.entries().iter().position(|e| e.id == entry));
                (child, CanonicalLocation { agent, index })
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
            running,
            compacting,
        }
    }

    /// The projection of one agent, for tests that assert on a single
    /// transcript.
    pub fn agent(&self, id: AgentId) -> Option<&CanonicalAgent> {
        self.agents.iter().find(|a| a.agent == id)
    }
}

/// Total order over agents: main first, then subs by index.
fn agent_order(id: AgentId) -> (u8, usize) {
    match id {
        AgentId::Main => (0, 0),
        AgentId::Sub(n) => (1, n),
    }
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
        },
        EntryKind::Notice(n) => CanonicalEntry::Notice {
            level: n.level,
            text: n.text.clone(),
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
