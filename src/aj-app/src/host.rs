//! The session host: N live sessions behind one attachment and command
//! interface (spec section 5).
//!
//! The local frontend and a network server are peers here. Both attach
//! through [`SessionHost::attach`] and mutate through
//! [`SessionHost::command`], so "there is no owner of a session beyond the
//! host process" holds by construction rather than by discipline. The layer
//! depends on no transport and no rendering backend: it produces
//! [`aj_wire::Frame`] values, which a server serializes unchanged and the
//! client fold ([`crate::client::SessionClient`]) applies unchanged.
//!
//! Three concerns, three modules: [`fanout`] owns the subscriber registry
//! and the delivery rules, [`live`] one live session's state, [`driver`]
//! the task that drives it. This module is the surface and the lifecycle.
//!
//! **Concurrency.** Per session there is exactly one writer (its driver
//! task) and one publisher (the same task), which is what makes "live
//! durable frames reach a stream in strictly increasing seq order" a
//! property of the code. The fan-out subscribes to the bus through a
//! channel rather than an inline listener, because an inline listener's
//! stall or error becomes a fatal turn error (spec 6.9): network activity
//! must never be able to fail a turn.
//!
//! **Locks.** The order is log, then session status, then the subscriber
//! registry. The log's is an async mutex and is the only one a caller may
//! hold across an await. The other two are std mutexes, so holding one
//! across an await would make the driver's future non-`Send` and fail to
//! compile inside its spawned task, which is what keeps that rule mechanical
//! rather than a discipline. Both are only ever held to copy a few fields or
//! to push onto unbounded queues, so no await ever needs to happen under
//! them. The cold-directory cache in [`store`] holds one more, outside this
//! order: it is a strict leaf, taken only to read or replace a cache entry,
//! and nothing is ever acquired while it is held.

pub(crate) mod driver;
mod fanout;
mod live;
mod store;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use aj_agent::events::AgentId;
use aj_agent::queue::MessageQueues;
use aj_agent::tool::TaskId;
use aj_agent::types::UsageSummary;
use aj_agent::{BoxError, SubAgentRegistry, TaskRegistry};
use aj_conf::{AgentEnv, Config, ConfigThinkingDisplay};
use aj_models::ThinkingConfig;
use aj_models::auth::AuthStorage;
use aj_models::registry::{ModelInfo, default_thinking_level, validate_thinking_level};
use aj_models::types::{Speed, UserContent};
use aj_models::{speed_from_name, thinking_config_from_name, verbosity_from_name};
use aj_session::{
    AppendHandoff, ConversationLog, ConversationPersistence, EntryId, LockHolder, SessionLock,
    normalize_tag, project_suffix,
};
use aj_wire::{
    AgentQueue, Cursor, DurableEvent, Frame, Hello, ModelSelection, PROTOCOL_VERSION, QueueCounts,
    QueueState, SessionList, SessionSettings, SessionSummary, SessionTree, TaskDetails,
    TaskSummary, TaskTable, TreeSegment,
};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::{Sender, channel, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::host::driver::Driver;
use crate::host::live::{LiveSession, ReleaseOutcome, Request, SessionStatus, settings_of};
use crate::host::store::ColdSessions;
use crate::session::{SessionCore, SessionEntry, SessionSpec, SubAgentOverrides};
use crate::session_setup::{
    RestoreContext, RunConfigSnapshot, thinking_display_from_name, thinking_level_for,
};
use crate::settings::{ConfigLayers, PersistAction};

pub use fanout::Attachment;
use fanout::Fanout;

/// How long the list publisher coalesces directory changes before emitting
/// a frame.
///
/// `last_seq` churns on every durable event of a busy turn, and one `list`
/// frame per event would swamp every client's queue for data that is
/// cumulative anyway (spec 6.8).
const LIST_COALESCE: Duration = Duration::from_millis(200);

/// How long a session stays live with nothing running and nobody attached
/// before the host releases it (spec section 5).
///
/// The tradeoff is resume cost against lock hold time. Shorter, and switching
/// away from a session and back re-resumes its whole log for nothing. Longer,
/// and another process in the same directory waits that much longer for a
/// session this one is done with, which is the failure this exists to fix.
pub const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(30);

/// File in the session store holding this store's stable host id.
///
/// It names the store, not the process: session ids are unique within a
/// store, which is what makes `<host_id>:<session_id>` globally unique
/// (spec section 4). It therefore lives next to the logs it identifies,
/// never in user-global state or the working directory.
const HOST_ID_FILE: &str = "host-id";

/// Why a host request could not be served.
///
/// Typed because callers branch on it: a network server maps the variants
/// onto the status vocabulary of spec 6.1. 400 for [`Self::Invalid`], 404
/// for the unknown cases, 409 for [`Self::Conflict`], [`Self::Locked`] and
/// [`Self::Unsupported`], 500 for [`Self::Internal`]. (503 is the gateway's
/// alone: it means an upstream host is unreachable, which a host cannot say
/// about itself.)
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("unknown session {0}")]
    UnknownSession(String),
    #[error("unknown background task {0}")]
    UnknownTask(TaskId),
    #[error("unknown log entry {0}")]
    UnknownEntry(String),
    /// The request is well formed but conflicts with the session's current
    /// state (a turn is running, background work is live).
    #[error("{reason}")]
    Conflict { reason: String },
    /// Another writer holds the session's advisory lock, so this host
    /// refuses to materialize it rather than grow a sibling branch in a
    /// shared log.
    ///
    /// The message names the holder when its lock file recorded one, because a
    /// user told "session in use" needs to know which process to go quit or
    /// detach (spec section 5). `None` when the record is missing or illegible,
    /// which an older build's lock file is.
    #[error("session {session} is held by {}", holder_name(holder))]
    Locked {
        session: String,
        holder: Option<LockHolder>,
    },
    /// The request is well formed and conflicts with nothing, but this host
    /// cannot serve it: a model it has no credentials for, a settings
    /// change for an agent that is not live. Deliberately not
    /// [`Self::Invalid`], because nothing about the request is malformed and
    /// a client retrying it verbatim against another host may well succeed.
    #[error("{0}")]
    Unsupported(String),
    /// The request is malformed: an empty prompt, a session named twice in
    /// one attach, an entry id whose role cannot be a head.
    #[error("{0}")]
    Invalid(String),
    #[error("internal host error: {0}")]
    Internal(#[source] BoxError),
}

impl HostError {
    /// The protocol code this failure travels as (spec 6.1): in an error body,
    /// and in the `error` frame that refuses one session's attach (spec 6.3).
    ///
    /// The vocabulary lives here, beside the failures it names, rather than in
    /// the HTTP layer, which maps the same variants onto statuses. A frame
    /// carries no status, so the two would otherwise have to agree by
    /// discipline.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownSession(_) => "unknown_session",
            Self::UnknownTask(_) => "unknown_task",
            Self::UnknownEntry(_) => "unknown_entry",
            Self::Conflict { .. } => "conflict",
            Self::Locked { .. } => "locked",
            Self::Unsupported(_) => "unsupported",
            Self::Invalid(_) => "invalid_request",
            Self::Internal(_) => "internal",
        }
    }
}

/// Why a create did not deliver everything that was asked of it.
///
/// Creating the session is the operation that either happens or does not.
/// Applying a label and submitting a first prompt happen *after* the session
/// exists, so their failure cannot un-create it.
///
/// Typed because callers branch on the distinction, and on nothing finer: a
/// [`Self::Refused`] create did not happen, a [`Self::Incomplete`] one did.
/// What went wrong underneath is opaque, because no caller does anything
/// with it but show it.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// Nothing was minted. The request was refused before a log existed, so
    /// it left no discoverable session behind.
    #[error(transparent)]
    Refused(#[from] HostError),
    /// The session was minted. See [`PartialCreate`].
    #[error(transparent)]
    Incomplete(#[from] PartialCreate),
}

/// A create that minted its session and could not apply everything asked of
/// it afterwards.
///
/// The session named exists, is live and is in the host's directory. The id
/// travels as a field rather than only in the message, because that is what
/// a caller acts on: it attaches the session and retries the step that did
/// not land, rather than creating a second session.
#[derive(Debug, thiserror::Error)]
#[error("session {session} created, {step} not applied: {source}")]
pub struct PartialCreate {
    /// The session the create minted, which the caller acts on.
    pub session: String,
    /// What did not land, worded for the message ("tag", "first prompt").
    step: &'static str,
    #[source]
    source: BoxError,
}

impl PartialCreate {
    /// The label did not land, so the store still says what it said before.
    fn tag(session: String, source: HostError) -> Self {
        Self {
            session,
            step: "tag",
            source: Box::new(source),
        }
    }

    /// The first prompt was not taken, so the session is idle rather than
    /// working.
    fn prompt(session: String, source: HostError) -> Self {
        Self {
            session,
            step: "first prompt",
            source: Box::new(source),
        }
    }
}

/// Who a [`HostError::Locked`] message names as the holder: what the lock
/// file recorded, or a generic writer where it recorded nothing.
///
/// The specific name replaces the generic one rather than qualifying it, so
/// the refusal names its cause once. A user told a session is in use is being
/// pointed at a process to go quit, and "another writer (pid 5 of host h)"
/// spends two clauses getting there.
fn holder_name(holder: &Option<LockHolder>) -> String {
    match holder {
        Some(holder) => holder.to_string(),
        None => "another writer".to_string(),
    }
}

/// What a host is built from: the process-wide handles a frontend already
/// assembles at startup.
pub struct HostSetup {
    pub config: Arc<StdMutex<Config>>,
    pub layers: Arc<StdMutex<ConfigLayers>>,
    pub catalog: Arc<Vec<ModelInfo>>,
    /// The process default every session's own run config is cloned from.
    pub run_config: RunConfigSnapshot,
    /// Resume-time settings restoration, `None` on the scripted path.
    pub restore: Option<RestoreContext>,
    pub persistence: ConversationPersistence,
    pub auth: AuthStorage,
    /// The one working directory this host serves (spec section 4).
    pub working_directory: PathBuf,
    /// How long an idle, unattached session is held before it is released,
    /// `None` for [`DEFAULT_IDLE_GRACE`].
    pub idle_grace: Option<Duration>,
    /// How many frames one client's live queue holds before an undroppable
    /// frame evicts it (spec 6.9), `None` for the fan-out's own default.
    ///
    /// Tuning, not policy: the bound governs live fan-out only, and eviction
    /// and its recovery behave the same at any value.
    pub live_capacity: Option<NonZeroUsize>,
}

/// A mutation of one session.
///
/// Commands that act on "the viewed agent" locally carry the target
/// explicitly, so a client resolves its own view to a parameter (spec 6.6).
pub enum Command {
    Prompt {
        agent: AgentId,
        content: Vec<UserContent>,
    },
    /// Queue steering, or promote the pending follow-up when `text` is
    /// empty.
    Steer {
        agent: AgentId,
        text: String,
    },
    Cancel {
        agent: AgentId,
    },
    Queue(QueueOp),
    Compact {
        instructions: Option<String>,
    },
    Settings(SettingsChange),
    /// Set the session's label, `None` clears it (spec 6.6).
    ///
    /// The value is expected to have been through
    /// [`aj_session::normalize_tag`] already: a trimmed single line, or `None`
    /// for anything that clears. Validating at the edge is what keeps a
    /// refused label from costing a materialization.
    Tag {
        tag: Option<String>,
    },
    /// Switch the session's head. Refused while work is live.
    Head {
        target: HeadTarget,
    },
    KillTask {
        task: TaskId,
    },
}

/// Which entry a head switch moves to.
///
/// [`Self::Before`] exists because branching from a transcript message must
/// replace that message rather than continue after it, so the head goes to
/// its parent. The host resolves the parent, which keeps the gesture one
/// command and keeps every client from repeating the same walk (spec 6.6).
pub enum HeadTarget {
    Entry(EntryId),
    Before(EntryId),
}

impl HeadTarget {
    /// The entry the request named, which is what a refusal quotes. A
    /// [`Self::Before`] target moves the head somewhere else, so reporting
    /// the resolved entry would name an id the client never sent.
    pub fn named(&self) -> &str {
        match self {
            Self::Entry(entry) | Self::Before(entry) => entry,
        }
    }
}

/// A withdrawal of one agent's pending message, or a clear of the whole
/// session's queues.
///
/// The queues hold at most one message per agent (the "one message, one
/// kind" invariant), so a withdrawal names the agent rather than a slot.
/// A clear takes no agent: spec 6.6 makes it session-wide, and only
/// `remove` targets an agent.
pub enum QueueOp {
    Remove { agent: AgentId },
    Clear,
}

/// Which settings axis a change moves, and to what.
pub enum SettingsAxis {
    Model(ModelInfo),
    Thinking(Option<ThinkingConfig>),
    ThinkingDisplay(Option<ConfigThinkingDisplay>),
    Speed(Option<Speed>),
    Verbosity(Option<aj_conf::ConfigVerbosity>),
}

/// A settings change: the agent it targets, whether it outlives the
/// session, and the axis it moves.
pub struct SettingsChange {
    pub agent: AgentId,
    pub persist: PersistAction,
    pub axis: SettingsAxis,
}

/// What accepting a command handed back.
#[derive(Debug)]
pub enum CommandOutcome {
    /// Accepted, with nothing to return.
    Accepted,
    /// The withdrawn queued message, so a client can restore it to its
    /// editor the way the local dequeue gesture does. `None` when nothing
    /// was pending.
    Withdrawn(Option<String>),
}

/// One session to attach, with the cursor the client offers for it.
#[derive(Clone)]
pub struct AttachRequest {
    pub session: String,
    /// The last durable position the client committed. A cursor from
    /// another epoch, or beyond the session's high-water mark, means a full
    /// backfill (spec 6.5).
    pub cursor: Option<Cursor>,
}

/// What one attached stream owes one of the sessions it named: the block it
/// asked for, or the refusal it gets instead (spec 6.5).
///
/// Resolved before the stream is handed back and written in the order the
/// sessions were named, so a client reads one answer per session and can tell
/// which of them it is waiting for.
enum Serving {
    Block(AttachRequest, Arc<LiveSession>),
    Refusal(Frame),
}

/// Direct handles into one live session, for a client attached in process.
///
/// Spec section 5 sanctions this: the local frontend attaches "through direct
/// handles and channels, not through HTTP". It is a **read** surface: the
/// footer reads the run config, a task-output overlay the task registry, and
/// none of that goes through a command. The pending-message box does not
/// appear here, it renders the queue snapshot the fold keeps in
/// [`ChatState::queue`](crate::chat::ChatState::queue), which is one path for
/// a local and a remote frontend alike.
///
/// Mutating through these handles is a convention this type cannot enforce,
/// because the handles it hands out (the queues, the log, the run config) are
/// the real ones and carry their own mutators. Breaking the convention is
/// invisible rather than loud: an enqueue that bypasses
/// [`SessionHost::command`] publishes no `QueueUpdate`, so every other
/// client's queue view silently goes stale, and the local and the remote path
/// stop being one path.
///
/// The tests do use them to stage state the command surface cannot reach: an
/// idle session's queue, which no command can fill because queueing only
/// happens while an agent is busy.
///
/// Nothing outside this process can have them, so no protocol rule may come
/// to depend on them.
pub struct LocalHandles {
    pub session_id: String,
    pub queues: MessageQueues,
    pub task_registry: TaskRegistry,
    pub registry: SubAgentRegistry,
    pub log: Arc<TokioMutex<ConversationLog>>,
    pub run_config: Arc<StdMutex<RunConfigSnapshot>>,
    pub sub_overrides: Arc<StdMutex<HashMap<usize, SubAgentOverrides>>>,
    pub env: AgentEnv,
    /// Legacy in-process restoration diagnostics.
    ///
    /// New frontends render one local summary from
    /// [`crate::client::SessionClient::take_first_attach_settings`]. The host
    /// never publishes these lines, which prevents reconnect duplication.
    pub restore_notices: Vec<String>,
}

/// Everything a session's driver needs that is not the session itself.
///
/// Deliberately free of a back-reference to the session map: a driver task
/// holding one would keep the host alive after every handle to it is gone.
pub(crate) struct HostShared {
    pub(crate) config: Arc<StdMutex<Config>>,
    pub(crate) layers: Arc<StdMutex<ConfigLayers>>,
    pub(crate) catalog: Arc<Vec<ModelInfo>>,
    pub(crate) auth: AuthStorage,
    pub(crate) restore: Option<RestoreContext>,
    pub(crate) fanout: Arc<Fanout>,
    /// The store the session's own files live in. A driver reaches it for the
    /// tag sidecar, which is written under the session's advisory lock and so
    /// belongs to the task that holds it.
    pub(crate) persistence: ConversationPersistence,
}

/// The session map, held.
type SessionMap<'a> = tokio::sync::MutexGuard<'a, HashMap<String, LiveEntry>>;

/// One live session plus the task driving it.
struct LiveEntry {
    session: Arc<LiveSession>,
    /// Awaited whenever the session is torn down, at shutdown or on release.
    /// The driver holds the session's advisory lock, so joining the task is
    /// what releases it.
    driver: JoinHandle<()>,
}

struct HostInner {
    shared: Arc<HostShared>,
    persistence: ConversationPersistence,
    base_run_config: RunConfigSnapshot,
    host_id: String,
    working_directory: PathBuf,
    sessions: TokioMutex<HashMap<String, LiveEntry>>,
    /// Wall clock and monotonic clock read at the same moment, for
    /// projecting a task's `Instant` onto wall time (see [`wall_clock`]).
    /// Read once per host rather than per call, so the same task's start
    /// time does not move between two reads of the table.
    clock_anchor: (DateTime<Utc>, Instant),
    /// The store's own sessions, with the per-file facts a directory entry
    /// needs cached against the files they came from. A `list` refresh runs on
    /// a coalescing tick that session events drive, so it must not rescan the
    /// store's contents (spec 6.8).
    cold: ColdSessions<ConversationPersistence>,
    idle_grace: Duration,
    /// Set by [`SessionHost::shutdown`], and never cleared: a host is torn
    /// down once. Every operation refuses afterwards (see
    /// [`SessionHost::alive`]).
    shut_down: AtomicBool,
}

impl Drop for HostInner {
    fn drop(&mut self) {
        // A host dropped without `shutdown` would otherwise leave its
        // drivers running (and its session locks held) for the life of the
        // process. Aborting is not the graceful path (no turn cancel, no
        // task quiescing, no log flush), so `shutdown` stays the
        // documented teardown, and this only bounds the damage, loudly.
        let abandoned: Vec<&String> = self.sessions.get_mut().keys().collect();
        if !abandoned.is_empty() {
            tracing::warn!(
                "host dropped without shutdown: aborting the drivers of {abandoned:?} \
                 without cancelling turns or flushing their logs"
            );
        }
        for entry in self.sessions.get_mut().values() {
            entry.driver.abort();
        }
    }
}

/// Owns every live session in this process and serves attachments and
/// commands against them.
#[derive(Clone)]
pub struct SessionHost {
    inner: Arc<HostInner>,
}

impl SessionHost {
    /// Build a host over `setup`'s session store, minting or reading back
    /// the store's stable host id.
    pub fn new(setup: HostSetup) -> Result<Self, HostError> {
        let HostSetup {
            config,
            layers,
            catalog,
            run_config,
            restore,
            persistence,
            auth,
            working_directory,
            idle_grace,
            live_capacity,
        } = setup;
        let host_id = resolve_host_id(persistence.sessions_dir())?;
        let inner = Arc::new(HostInner {
            shared: Arc::new(HostShared {
                config,
                layers,
                catalog,
                auth,
                restore,
                fanout: Arc::new(Fanout::new(live_capacity)),
                persistence: persistence.clone(),
            }),
            cold: ColdSessions::new(persistence.clone()),
            persistence,
            base_run_config: run_config,
            host_id,
            working_directory,
            sessions: TokioMutex::new(HashMap::new()),
            clock_anchor: (Utc::now(), Instant::now()),
            idle_grace: idle_grace.unwrap_or(DEFAULT_IDLE_GRACE),
            shut_down: AtomicBool::new(false),
        });
        // Host startup is an enumeration point (spec 6.8). Nothing is live
        // yet, and a store that cannot be read is not fatal: the next
        // enumeration point tries again.
        //
        // Synchronous, which it can afford to be because a row costs a
        // `readdir` entry, a `stat` and one first-line sniff. Anything that
        // read a log to build a row would put the whole store's bytes in front
        // of the first frame, which on a real store is gigabytes.
        if let Err(err) = inner.cold.enumerate(|_| false) {
            tracing::warn!("could not read the session store at startup: {err}");
        }
        spawn_list_publisher(&inner);
        spawn_idle_sweeper(&inner);
        Ok(Self { inner })
    }

    /// Protocol identity and capabilities (spec 6.1). The capability list is
    /// empty: everything the protocol carries today is in its base version.
    pub fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: Vec::new(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            host_id: self.inner.host_id.clone(),
            working_directory: Some(self.inner.working_directory.clone()),
        }
    }

    /// Create a session in the host's working directory and hold it live.
    pub async fn create(&self) -> Result<String, HostError> {
        self.alive()?;
        self.mint(None).await
    }

    /// Creates a session with creator-selected settings, a first prompt and a
    /// tag.
    ///
    /// Creation is the operation that either happens or does not. Every
    /// setting, the prompt and the tag are validated before a log is created,
    /// so a request this host refuses ([`CreateError::Refused`]) leaves no
    /// discoverable empty session behind.
    ///
    /// The tag and the prompt are applied once the session is live, through
    /// the same commands a later relabelling or prompt takes, so they land
    /// under the session's own lock. They are best-effort: a step that fails
    /// on its own terms, a store that will not take the sidecar write, say,
    /// is not a failed create. The answer is a [`PartialCreate`] naming the
    /// session, which is live and in the directory, and what did not land
    /// ("session <id> created, tag not applied: <reason>"), so the caller
    /// retags rather than creating a second session. A minted session is
    /// durable user state and is never deleted to make an error tidier.
    pub async fn create_with(
        &self,
        settings: Option<SessionSettings>,
        prompt: Option<Vec<UserContent>>,
        tag: Option<String>,
    ) -> Result<String, CreateError> {
        self.alive()?;
        if let Some(content) = prompt.as_deref() {
            validate_prompt(content)?;
        }
        let tag = normalize_tag(tag.as_deref().unwrap_or_default())
            .map_err(|err| HostError::Invalid(err.to_string()))?;
        let session = self.mint(settings.as_ref()).await?;
        if tag.is_some() {
            if let Err(err) = self.command(&session, Command::Tag { tag }).await {
                return Err(PartialCreate::tag(session, err).into());
            }
        }
        if let Some(content) = prompt {
            let prompt = Command::Prompt {
                agent: AgentId::Main,
                content,
            };
            if let Err(err) = self.command(&session, prompt).await {
                return Err(PartialCreate::prompt(session, err).into());
            }
        }
        Ok(session)
    }

    /// Mint a session with `settings` resolved against this host's catalog
    /// and hold it live, answering its id.
    ///
    /// The half of a create that is all-or-nothing. Callers gate on
    /// [`Self::alive`] first, so that a shut-down host refuses before it
    /// validates anything.
    async fn mint(&self, settings: Option<&SessionSettings>) -> Result<String, HostError> {
        let run_config = self.resolve_creator_settings(settings)?;
        let mut sessions = self.inner.sessions.lock().await;
        let live = self
            .materialize(&mut sessions, None, Some(run_config))
            .await?;
        Ok(live.id().to_string())
    }

    /// Resolves a wire model-selection triple against this host's catalog.
    ///
    /// The returned row is host-owned data with an optional client URL
    /// override. Callers pass it to [`SettingsAxis::Model`] rather than
    /// accepting a catalog object from the wire.
    pub fn resolve_model_selection(
        &self,
        selection: &ModelSelection,
    ) -> Result<ModelInfo, HostError> {
        validate_model_selection(selection)?;
        let mut info = self
            .inner
            .shared
            .catalog
            .iter()
            .find(|info| info.provider == selection.api && info.id == selection.name)
            .cloned()
            .or_else(|| {
                (self.inner.base_run_config.model_key
                    == (selection.api.clone(), selection.name.clone()))
                    .then(|| (*self.inner.base_run_config.model_info).clone())
            })
            .ok_or_else(|| {
                HostError::Unsupported(format!(
                    "model {}/{} is not in the host catalog",
                    selection.api, selection.name
                ))
            })?;
        if let Some(url) = &selection.url {
            info.base_url.clone_from(url);
        }
        Ok(info)
    }

    /// Open a stream and serve an attach block for every session it can
    /// resolve, and a session-scoped refusal for every session it cannot.
    ///
    /// Registering the subscriber and projecting a session's suffix are
    /// atomic with respect to that session's event flow: a durable frame
    /// published in between is either already in the backfill (and filtered
    /// against its boundary) or above it (and delivered), so a client can
    /// neither miss one nor be served one twice. That is what makes attaching
    /// a single round trip with no client-side buffer-and-reconcile dance
    /// (spec 6.5).
    ///
    /// A stream never fails wholesale over one bad session (spec 6.5). Each
    /// named session gets one of two answers, in the order it was named: its
    /// attach block, or an `error` frame carrying why this host could not
    /// serve it (an id its store could never hold, one it does not have, one
    /// another writer holds). Refusing the request instead would cost the
    /// client every healthy session it named. What does fail the request is
    /// what is wrong with the request rather than with a session: a session
    /// named twice, and a host that is shut down.
    ///
    /// Returning successfully means every session [`Attachment::attached`]
    /// names will have its block written, which is what a client arms its
    /// fold from.
    pub async fn attach(&self, requests: &[AttachRequest]) -> Result<Attachment, HostError> {
        self.alive()?;
        // One block per named session is the client contract (spec 6.5), and
        // a duplicate would be served two: the second would open a block
        // the client is not expecting and quiesce state it just applied. It
        // is the request that is malformed, not a session, so it is refused
        // as one.
        let mut names: Vec<String> = Vec::with_capacity(requests.len());
        for request in requests {
            if names.contains(&request.session) {
                return Err(HostError::Invalid(format!(
                    "session {} is named twice in one attach",
                    request.session
                )));
            }
            names.push(request.session.clone());
        }
        // Registered before anything is materialized, so the release path sees
        // an attach in flight as use from the first instant (spec section 5).
        // Materializing first would leave a window where an idle session could
        // be released out from under a block about to be served from it.
        let (id, live_frames, cancelled) = self.inner.shared.fanout.register(&names);
        // Resolved up front, so that returning means every block this stream
        // owes can be written.
        let mut serving = Vec::with_capacity(requests.len());
        let mut attached = Vec::with_capacity(requests.len());
        for request in requests {
            match self.live(&request.session).await {
                Ok(session) => {
                    attached.push(request.session.clone());
                    serving.push(Serving::Block(request.clone(), session));
                }
                Err(err) => {
                    // Off the attach set again: this host may hold the session
                    // later, for somebody else, and its frames must not reach a
                    // stream that was never served its block.
                    self.inner.shared.fanout.detach(id, &request.session);
                    serving.push(Serving::Refusal(Frame::Error {
                        session: request.session.clone(),
                        // The session was never resolved, so there is no epoch
                        // this could be about.
                        epoch: None,
                        code: err.code().to_string(),
                        message: err.to_string(),
                    }));
                }
            }
        }
        // A stream attach is an enumeration point (spec 6.8), placed after the
        // materializations so their sessions are already out of the per-file
        // work. A store that cannot be read does not fail an attach whose
        // sessions all resolved.
        if let Err(err) = self.enumerate().await {
            tracing::warn!("could not re-read the session store for an attach: {err}");
        }
        let (block_tx, block_rx) = channel(1);
        let attachment = Attachment::new(
            id,
            block_rx,
            live_frames,
            cancelled.clone(),
            attached,
            Arc::clone(&self.inner.shared.fanout),
        );
        // Registered above before any block is projected: from here on every
        // frame this host publishes is either queued behind the block or filtered
        // against its boundary, which is the atomicity the doc promises.
        let host = self.clone();
        tokio::spawn(async move {
            for item in serving {
                let served = match item {
                    Serving::Block(request, session) => {
                        host.serve_block(id, &request, &session, &block_tx, &cancelled)
                            .await
                    }
                    Serving::Refusal(frame) => send_block_frame(&block_tx, &cancelled, frame).await,
                };
                if !served {
                    break;
                }
            }
        });
        Ok(attachment)
    }

    /// Apply a command to `session`, materializing it if it is only on disk.
    ///
    /// The session map is held from the materialization through the send, which
    /// is what keeps a release from landing in between (spec section 5): a
    /// command either reaches the driver ahead of the release request, and the
    /// driver then declines to go, or it waits out the whole teardown and
    /// re-materializes. The reply is awaited with the map released, since
    /// requests are answered in arrival order and this one is already queued.
    pub async fn command(
        &self,
        session: &str,
        command: Command,
    ) -> Result<CommandOutcome, HostError> {
        let (reply, outcome) = oneshot::channel();
        {
            let (_sessions, live) = self.live_locked(session).await?;
            if !live.send(Request::Command { command, reply }) {
                return Err(HostError::Internal("session driver is gone".into()));
            }
        }
        outcome
            .await
            .map_err(|_| HostError::Internal("session driver dropped the request".into()))?
    }

    /// Every session of the host's working directory, on-disk ones as well
    /// as live ones. The discovery surface (spec 6.7): there is no separate
    /// on-disk listing.
    ///
    /// An enumeration point (spec 6.8), so an explicit listing shows a session
    /// a sibling process left in the directory even though no refresh would
    /// have gone looking for it.
    pub async fn sessions(&self) -> Result<SessionList, HostError> {
        self.alive()?;
        self.enumerate().await?;
        Ok(self.directory().await)
    }

    /// The directory as the host holds it, live sessions' own state merged with
    /// the cold rows as they stand.
    ///
    /// Touches no filesystem: this is what a refresh serves (spec 6.8), and it
    /// runs on every published frame.
    async fn directory(&self) -> SessionList {
        // Both halves are read under the session map, which is what makes them
        // one observation. A release records its cold row and drops the session
        // from the map under that same lock, so a session read here is either
        // live or has a row, never neither. Taking the rows outside the hold
        // would let a release land in between and drop the session out of the
        // directory for a frame, which is not something a release may do (spec
        // section 5: a client sees the liveness flag flip and nothing else). The
        // cold cache is a leaf, so nesting its lock under the map cannot invert
        // an order.
        let sessions = self.inner.sessions.lock().await;
        let live: Vec<Arc<LiveSession>> = sessions
            .values()
            .map(|entry| Arc::clone(&entry.session))
            .collect();
        let cold = self.inner.cold.rows();
        drop(sessions);
        let mut summaries: BTreeMap<String, SessionSummary> = cold
            .into_iter()
            .map(|session| {
                (
                    session.id.clone(),
                    SessionSummary {
                        id: session.id,
                        live: false,
                        working: false,
                        queued: QueueCounts::default(),
                        tasks: 0,
                        // A cold row carries no durable position (spec 6.8):
                        // the count is not recorded anywhere, so producing one
                        // would mean reading the log.
                        last_seq: None,
                        last_activity: session.last_activity,
                        tag: session.tag,
                        // Only a gateway names the host a row belongs to
                        // (spec 6.8). Every row here is this host's own.
                        host: None,
                        unreachable: false,
                    },
                )
            })
            .collect();
        // A live session always wins the id: the cold cache can still hold the
        // row a session had before it was materialized, and of those two
        // answers only the host's own is current.
        for session in &live {
            summaries.insert(session.id().to_string(), summarize(session));
        }
        // Latest first: session ids are minted as timestamps, so their
        // descending order is chronological.
        let mut sessions: Vec<SessionSummary> = summaries.into_values().collect();
        sessions.sort_by(|left, right| right.id.cmp(&left.id));
        // No hosts: a plain host serves one working directory, and the field
        // is a gateway's (spec 7.1).
        SessionList {
            sessions,
            hosts: Vec::new(),
        }
    }

    /// How many times the host has read its session store's directory.
    ///
    /// The refresh contract (spec 6.8) is about the filesystem work a refresh
    /// does *not* do, which the frames it produces cannot show, so this is the
    /// seam the tests assert on.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_directory_reads(&self) -> u64 {
        self.inner.cold.directory_reads()
    }

    /// How many times the host has read its session store's `meta/` directory.
    ///
    /// The other half of the enumeration's directory cost (spec 6.8). A
    /// sidecar listing transfers no bytes and reads no sidecar, so neither a
    /// byte budget nor [`Self::store_tag_reads`] can see it: this is the only
    /// seam that catches a refresh that went looking for labels.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_sidecar_directory_reads(&self) -> u64 {
        self.inner.cold.sidecar_directory_reads()
    }

    /// How many membership questions the host has put to its session store.
    ///
    /// The seam for spec 6.2's "before it reaches ... any store lookup": an id
    /// the grammar turns away and one the store does not hold answer the same
    /// 404, so only this tells them apart.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_membership_lookups(&self) -> u64 {
        self.inner.cold.membership_lookups()
    }

    /// How many tag sidecars the host has read to refresh its directory.
    ///
    /// The per-file half of the refresh contract's budget (spec 6.8): a row
    /// carries its label whether it was cached or freshly read, so this is the
    /// only way to tell an untagged store costing nothing from one paying a
    /// read per row. A materialization's own read of the session it opens goes
    /// straight to the store and is not counted here.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_tag_reads(&self) -> u64 {
        self.inner.cold.tag_reads()
    }

    /// The activity stamp a session starts its materialization from.
    ///
    /// Materializing is not activity. A session resumed from a log written
    /// last week keeps reporting last week, or every session the user merely
    /// opens would claim it just did something and the unseen-output glyph
    /// would read off it (spec 6.8).
    ///
    /// A row this host already holds wins outright, because it is an answer
    /// about the session and the file's modification time is an answer about
    /// its bytes (see `ReleasedRow`). Taking the later of the two instead
    /// would let a release's own teardown flush, which moves the mtime long
    /// after the work it wrote, come back as activity. The file stands in when
    /// the host has no row, which is every session on a host that just
    /// started. A created session has neither, and being created is the
    /// activity.
    fn opening_stamp(&self, log: &ConversationLog, session_id: &str) -> DateTime<Utc> {
        self.inner
            .cold
            .stamp(session_id)
            .or_else(|| log_modified_at(log))
            .unwrap_or_else(Utc::now)
    }

    /// Re-read the store into the cold cache. The enumeration point (spec 6.8).
    async fn enumerate(&self) -> Result<(), HostError> {
        // The live set keeps a live session's log out of the per-file work.
        // The host holds its status, which is both cheaper and more current
        // than the file, and a session mid-append is the last thing worth
        // sniffing (spec 6.8).
        let live: HashSet<String> = self.inner.sessions.lock().await.keys().cloned().collect();
        // The scan is blocking IO, so it runs with the session map's lock
        // already released.
        let scanned = self.inner.cold.enumerate(|id| live.contains(id));
        // Marked whatever the scan found, and whether or not it succeeded. What
        // it discovered has to reach the next published frame, and an
        // enumeration point is also where a subscriber that has seen no
        // directory yet gets one, which suppression leaves to the mark.
        self.inner.shared.fanout.mark_list_dirty();
        scanned.map_err(|err| HostError::Internal(Box::new(err)))
    }

    /// The session's background-task table, with wall-clock timestamps: the
    /// in-memory registry keeps `Instant`s, which mean nothing off-process.
    ///
    /// A session that is not live answers empty rather than being
    /// materialized for the read (spec 6.7): a cold session has no tasks by
    /// definition, and paying a resume, an agent rebuild and the advisory
    /// lock to learn that would be perverse.
    pub async fn tasks(&self, session: &str) -> Result<TaskTable, HostError> {
        let Some(live) = self.live_or_cold(session).await? else {
            return Ok(TaskTable::default());
        };
        let tasks = live
            .core
            .task_registry
            .snapshot()
            .into_iter()
            .map(|task| TaskSummary {
                id: task.id,
                owner: task.owner,
                call_id: task.call_id,
                kind: task.kind,
                label: task.label,
                status: task.status,
                started_at: wall_clock(self.inner.clock_anchor, task.started_at),
            })
            .collect();
        Ok(TaskTable { tasks })
    }

    /// Detailed, remotely reachable output for one background task.
    ///
    /// Cold sessions have no task registry, so every id is unknown. The host's
    /// spill path is intentionally omitted from the returned wire model.
    pub async fn task(&self, session: &str, task: TaskId) -> Result<TaskDetails, HostError> {
        let Some(live) = self.live_or_cold(session).await? else {
            return Err(HostError::UnknownTask(task));
        };
        let (status, read) = live
            .core
            .task_registry
            .read(task)
            .ok_or(HostError::UnknownTask(task))?;
        Ok(TaskDetails {
            id: task,
            status,
            stdout_tail: read.stdout_tail,
            stderr_tail: read.stderr_tail,
            stdout_total_bytes: read.stdout_total_bytes,
            stderr_total_bytes: read.stderr_total_bytes,
            report: read.report,
        })
    }

    /// The session's pending steering and follow-up messages. Empty, and no
    /// materialization, for a session that is not live (see [`Self::tasks`]).
    pub async fn queue(&self, session: &str) -> Result<QueueState, HostError> {
        let Some(live) = self.live_or_cold(session).await? else {
            return Ok(QueueState::default());
        };
        let mut agents = live.core.message_queues.queued_agents();
        agents.sort_by_key(|agent| match agent {
            AgentId::Main => (0, 0),
            AgentId::Sub(n) => (1, *n),
        });
        let queues = agents
            .into_iter()
            .map(|agent| {
                let (steering, follow_up) = live.core.message_queues.event_messages(agent);
                AgentQueue {
                    agent_id: agent,
                    steering,
                    follow_up,
                }
            })
            .collect();
        Ok(QueueState { queues })
    }

    /// The session's branch tree, for a tree view and head switching.
    ///
    /// The one read that materializes (spec 6.7): the tree is derived from
    /// the log's parent chains, so answering it means parsing the log, which
    /// is what a materialization does anyway.
    pub async fn tree(&self, session: &str) -> Result<SessionTree, HostError> {
        let live = self.live(session).await?;
        // Cheap and in-memory, but it still walks the log, so snapshot
        // under the lock and build outside it.
        let snapshot = live.core.log.lock().await.snapshot();
        let head = snapshot.head().cloned();
        let tree = snapshot.session_tree();
        Ok(SessionTree {
            segments: tree
                .segments
                .into_iter()
                .map(|segment| TreeSegment {
                    head: segment.head,
                    label: segment.label,
                    message_count: segment.message_count,
                    last_timestamp: segment.last_timestamp,
                    parent: segment.parent,
                    children: segment.children,
                    on_active_path: segment.on_active_path,
                    is_leaf: segment.is_leaf,
                })
                .collect(),
            head,
        })
    }

    /// The session's accumulated token usage, for an end-of-run report.
    ///
    /// `None` for a session that is not live: usage is per materialization, so a
    /// session this host is not holding spent nothing that this host can still
    /// account for. Locks the agent, so a turn in flight holds this up for the
    /// length of that turn.
    pub async fn usage(&self, session: &str) -> Result<Option<UsageSummary>, HostError> {
        let Some(live) = self.live_or_cold(session).await? else {
            return Ok(None);
        };
        Ok(Some(live.core.usage_summary().await))
    }

    /// Direct handles into a live session, for an in-process client. See
    /// [`LocalHandles`].
    pub async fn local_handles(&self, session: &str) -> Result<LocalHandles, HostError> {
        let live = self.live(session).await?;
        Ok(LocalHandles {
            session_id: live.id().to_string(),
            queues: live.core.message_queues.clone(),
            task_registry: live.core.task_registry.clone(),
            registry: live.core.registry.clone(),
            log: Arc::clone(&live.core.log),
            run_config: Arc::clone(&live.core.run_config),
            sub_overrides: Arc::clone(&live.core.sub_overrides),
            env: live.core.env.clone(),
            restore_notices: live.core.restore_notices.clone(),
        })
    }

    /// Tear every live session down and close every client stream.
    ///
    /// Each session's turns are cancelled through the graceful path (so
    /// transcripts stay consistent), its background tasks quiesced and its
    /// log flushed, and only then is its advisory lock released, which
    /// happens when its driver task ends.
    ///
    /// Terminal: every later request fails rather than rebuilding a session
    /// behind a driver nobody will ever tell to stop.
    pub async fn shutdown(&self) {
        // Set before the map is drained, so a request that raced this
        // cannot materialize a session between the drain and the flag.
        self.inner.shut_down.store(true, Ordering::Release);
        let entries: Vec<LiveEntry> = {
            let mut sessions = self.inner.sessions.lock().await;
            sessions.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            let LiveEntry { session, driver } = entry;
            session.send(Request::Shutdown);
            // The driver holds the session lock, so joining it is what
            // releases it. Dropping our own handle first keeps the session
            // from outliving the task.
            drop(session);
            let _ = driver.await;
        }
        self.inner.shared.fanout.close();
    }

    /// Release `session` if its driver reports it idle, answering whether it
    /// went (spec section 5).
    ///
    /// The session map is held for the whole teardown, which is what makes
    /// release serialize with materialization: a command or attach that arrives
    /// meanwhile waits here and then materializes afresh, rather than building
    /// a second core for a session whose lock the outgoing driver still holds.
    /// The driver is joined under the same hold, because joining it is what
    /// releases the lock. Awaiting the driver under the map cannot deadlock: a
    /// driver never reaches back for the map (see [`HostShared`]).
    ///
    /// The cost is that every other session's materialization waits out this
    /// one's teardown. Short in practice: the driver answers this at its own
    /// queue position, so at worst we wait out one command it had already
    /// started, and a session that is releasable has no turn to drain and no
    /// task to quiesce, which leaves a log flush.
    ///
    /// The driver has the last word on releasability (see
    /// [`Request::Release`]), so a caller may ask about a session that has
    /// since become busy.
    async fn release_if_idle(&self, session: &str) -> bool {
        let mut sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get(session) else {
            return false;
        };
        let (reply, outcome) = oneshot::channel();
        let answer = if entry.session.send(Request::Release { reply }) {
            outcome.await.ok()
        } else {
            None
        };
        if matches!(answer, Some(ReleaseOutcome::Declined)) {
            return false;
        }
        let reaped = answer.is_none();
        if reaped {
            // No answer means the driver is gone without the host asking, which
            // only a panicked task leaves behind. Reaping it is the recovery: a
            // session nothing drives can serve nothing, and leaving it in the
            // map makes every later command fail on a dead channel.
            tracing::warn!(session, "reaping a session whose driver is gone");
        }
        // Recorded under the same map hold that drops the session, so no
        // directory read can observe the session as neither live nor rowed.
        if let Some(ReleaseOutcome::Released { row }) = &answer {
            self.inner.cold.note_released(row);
        }
        let entry = sessions
            .remove(session)
            .expect("the entry is ours: the map has been held throughout");
        // The driver returns right after answering, and its return is what
        // drops the session's lock.
        let _ = entry.driver.await;
        drop(sessions);
        if reaped {
            // A reaped session left no row, so what the host knows about its
            // log is whatever it knew before the session was materialized, which
            // can be nothing at all. Going back to the store is the only way to
            // give it a row, and the alternative is a session that is on disk
            // and in no directory. The scan marks the directory dirty itself,
            // and only once it has a row to publish.
            if let Err(err) = self.enumerate().await {
                tracing::warn!(session, "could not re-read the store after a reap: {err}");
            }
        } else {
            // The session's liveness flag is the only trace a release leaves on
            // the wire (spec section 5), so a client watching the directory has
            // to be told.
            self.inner.shared.fanout.mark_list_dirty();
        }
        true
    }

    /// Refuse a request that arrived after [`Self::shutdown`].
    ///
    /// Without this every path through [`Self::live`] would happily rebuild
    /// a session the host just tore down: it would re-take the session's
    /// advisory lock and sit behind a driver nobody will ever send
    /// `Shutdown` to. Reachable from a request in flight when SIGTERM
    /// lands.
    fn alive(&self) -> Result<(), HostError> {
        if self.inner.shut_down.load(Ordering::Acquire) {
            return Err(HostError::Conflict {
                reason: "the host is shut down".to_string(),
            });
        }
        Ok(())
    }

    /// The live session for `session`, materializing it when it is only on
    /// disk.
    async fn live(&self, session: &str) -> Result<Arc<LiveSession>, HostError> {
        let (sessions, live) = self.live_locked(session).await?;
        drop(sessions);
        Ok(live)
    }

    /// The live session for `session`, with the session map still held.
    ///
    /// A caller that has to queue something on the session's driver keeps the
    /// map until it has (see [`Self::command`]): letting go first would let a
    /// release land in between and take the driver with it.
    async fn live_locked(
        &self,
        session: &str,
    ) -> Result<(SessionMap<'_>, Arc<LiveSession>), HostError> {
        self.alive()?;
        validate_session_id(session)?;
        let mut sessions = self.inner.sessions.lock().await;
        let live = self.materialize(&mut sessions, Some(session), None).await?;
        Ok((sessions, live))
    }

    /// The live session for `session`, or `None` when the store knows it but
    /// this host has not materialized it.
    ///
    /// The read path. A read must not materialize (spec 6.7), because doing
    /// so resumes the log, rebuilds the agent environment and takes the
    /// session's advisory lock, all to answer a question whose answer for a
    /// cold session is "nothing".
    async fn live_or_cold(&self, session: &str) -> Result<Option<Arc<LiveSession>>, HostError> {
        self.alive()?;
        validate_session_id(session)?;
        if let Some(entry) = self.inner.sessions.lock().await.get(session) {
            return Ok(Some(Arc::clone(&entry.session)));
        }
        if !self.on_disk(session)? {
            return Err(HostError::UnknownSession(session.to_string()));
        }
        Ok(None)
    }

    /// Take a session's advisory lock, refusing when another writer holds
    /// it.
    fn acquire(&self, id: &str) -> Result<SessionLock, HostError> {
        SessionLock::try_acquire(&self.inner.persistence, id, &self.inner.host_id)
            .map_err(|err| HostError::Internal(Box::new(err)))?
            .ok_or_else(|| HostError::Locked {
                session: id.to_string(),
                // Read only on the refusal path, and the record is cleared on
                // release, so what it names is a holder that has the lock now.
                holder: SessionLock::holder(&self.inner.persistence, id),
            })
    }

    /// Resolves creator overrides into a complete per-session run config.
    fn resolve_creator_settings(
        &self,
        settings: Option<&SessionSettings>,
    ) -> Result<RunConfigSnapshot, HostError> {
        let mut run = self.inner.base_run_config.clone();
        let Some(settings) = settings else {
            return Ok(run);
        };

        let speed = match settings.speed.as_deref() {
            Some(name) => speed_from_name(name).ok_or_else(|| {
                HostError::Invalid(format!("unknown speed {name:?}. Expected standard or fast"))
            })?,
            None => run.speed,
        };

        if let Some(selection) = &settings.model {
            let info = self.resolve_model_selection(selection)?;
            let base_bundle = self.inner.base_run_config.model_key
                == (selection.api.clone(), selection.name.clone())
                && self.inner.shared.catalog.iter().all(|catalog| {
                    catalog.provider != selection.api || catalog.id != selection.name
                });
            if base_bundle {
                if selection
                    .url
                    .as_deref()
                    .is_some_and(|url| url != run.model_info.base_url)
                {
                    return Err(HostError::Unsupported(format!(
                        "the host's injected model {}/{} cannot change its URL",
                        selection.api, selection.name
                    )));
                }
            } else {
                let resolved = crate::model::from_model_info(&self.inner.shared.auth, info, speed)
                    .map_err(|err| HostError::Unsupported(err.to_string()))?;
                run.provider = resolved.provider;
                run.model_info = resolved.model_info;
                run.stream_options = resolved.stream_options;
            }
            run.model_key = (selection.api.clone(), selection.name.clone());
        }

        run.speed = speed;
        run.stream_options.speed = speed;

        if let Some(name) = settings.thinking_display.as_deref() {
            run.thinking_display = thinking_display_from_name(name).ok_or_else(|| {
                HostError::Invalid(format!(
                    "unknown thinking display {name:?}. Expected default, summarized, detailed, or omitted"
                ))
            })?;
        }
        crate::model::apply_thinking_display(&mut run.stream_options, run.thinking_display);

        if let Some(name) = settings.verbosity.as_deref() {
            run.stream_options.verbosity = verbosity_from_name(name).ok_or_else(|| {
                HostError::Invalid(format!(
                    "unknown verbosity {name:?}. Expected default, low, medium, or high"
                ))
            })?;
        }

        if let Some(name) = settings.thinking.as_deref() {
            run.thinking = thinking_config_from_name(name).ok_or_else(|| {
                HostError::Invalid(format!(
                    "unknown thinking level {name:?}. Expected off, minimal, low, medium, high, xhigh, or max"
                ))
            })?;
            let level = run
                .thinking
                .as_ref()
                .map(thinking_level_for)
                .unwrap_or(aj_models::types::ThinkingLevel::Off);
            validate_thinking_level(&run.model_info, &level).map_err(HostError::Unsupported)?;
        } else {
            // Unstated, so this axis is ours to default and we default it
            // against the model actually chosen (spec section 8). Our own
            // configured level was resolved for our own default model, and a
            // creator who names a model without naming a level would otherwise
            // inherit a level that model may have no word for.
            let configured = run
                .thinking
                .as_ref()
                .map(thinking_level_for)
                .unwrap_or(aj_models::types::ThinkingLevel::Off);
            let level = default_thinking_level(&run.model_info, &configured);
            if level != configured {
                run.thinking = thinking_config_from_name(level.as_str())
                    .expect("a canonical level name parses");
            }
        }

        Ok(run)
    }

    /// Return the live session for `id`, creating it (when `id` is `None`)
    /// or resuming it from disk.
    ///
    /// `id` must already have passed [`validate_session_id`]: this is where a
    /// session id becomes a store path and an advisory lock.
    ///
    /// Runs under the session-map lock, which serializes materialization
    /// against every other one: two attaches of the same on-disk session
    /// must not both build a core and fight over its lock.
    async fn materialize(
        &self,
        sessions: &mut HashMap<String, LiveEntry>,
        id: Option<&str>,
        create_run_config: Option<RunConfigSnapshot>,
    ) -> Result<Arc<LiveSession>, HostError> {
        if let Some(id) = id {
            if let Some(entry) = sessions.get(id) {
                return Ok(Arc::clone(&entry.session));
            }
            if !self.on_disk(id)? {
                return Err(HostError::UnknownSession(id.to_string()));
            }
        }
        // A resume's lock is taken before the build, because the build is
        // not read-only: `ConversationLog::resume` truncates a torn tail
        // and the repair walk appends synthesized tool results. A
        // materialization this host refuses must have done neither (spec
        // section 5).
        //
        // A create has nothing on disk to read or repair, and it mints its
        // id by an atomic `create_new` claim on that id's lock-file path,
        // so no other writer can be holding the lock it takes below.
        let claimed = match id {
            Some(id) => Some(self.acquire(id)?),
            None => None,
        };
        let spec = match id {
            Some(id) => SessionSpec::Resume {
                session_id: id.to_string(),
                entry: SessionEntry::Switch,
            },
            None => SessionSpec::Create {
                entry: SessionEntry::Startup,
            },
        };
        // NOTE: the build does blocking IO (a resume reads the whole log,
        // and the agent environment re-reads the context files and skills)
        // while this task holds the session map. Every other
        // materialization waits behind it. Acceptable while a host holds a
        // handful of sessions. If it starts to hurt, the build moves to the
        // blocking pool.
        let config = self
            .inner
            .shared
            .config
            .lock()
            .expect("config mutex poisoned")
            .clone();
        let (mut core, _seed) = SessionCore::build(
            &config,
            create_run_config.unwrap_or_else(|| self.inner.base_run_config.clone()),
            &self.inner.persistence,
            &spec,
            self.inner.shared.restore.as_ref(),
        )
        .map_err(|err| HostError::Internal(err.into()))?;
        let session_id = core.session_id.clone();
        let lock = match claimed {
            Some(lock) => lock,
            None => self.acquire(&session_id)?,
        };

        let handoff = AppendHandoff::default();
        let events = core.install_persisting_forwarder(&handoff);
        // One small read, on a path that has just read the whole log. From
        // here the session answers its own label out of memory, so no
        // directory refresh ever reaches the sidecar for it (spec 6.8). A
        // label we cannot read is not worth failing a materialization over.
        let tag = match self.inner.persistence.read_tag(&session_id) {
            Ok(tag) => tag,
            Err(err) => {
                tracing::warn!(
                    session = session_id,
                    "could not read the session's tag: {err}"
                );
                // A read that failed says nothing about the label, so the
                // session goes live with what the host last knew rather than
                // with "untagged". Recording the failure as untagged would
                // outlive the blip: the row answers from memory for the whole
                // live period, and the release then writes that answer back
                // over the cached label.
                self.inner.cold.label(&session_id)
            }
        };
        let log = core.log.lock().await;
        let status = SessionStatus {
            epoch: mint_epoch(),
            last_seq: log.last_seq(),
            working: false,
            settings: settings_of(&core.run_config),
            // Nothing runs at materialization, so every sub-agent the log
            // names has finished. Seeding them is what keeps a backfill
            // concluding a resumed session's boxes while leaving the
            // brackets of runs this host starts open (see
            // `SessionStatus::finished_subs`).
            finished_subs: log.sub_agent_ids(),
            driven_subs: std::collections::BTreeSet::new(),
            last_activity: self.opening_stamp(&log, &session_id),
            tag,
            last_work: Instant::now(),
        };
        drop(log);
        let (requests_tx, requests) = unbounded_channel();
        let session = Arc::new(LiveSession::new(core, handoff, status, requests_tx));
        let driver = Driver::new(
            Arc::clone(&session),
            Arc::clone(&self.inner.shared),
            events,
            requests,
        );
        // The lock rides into the driver task: it is released when the task
        // ends, which is what `shutdown` awaits.
        let handle = tokio::spawn(async move {
            let lock = lock;
            driver.run().await;
            drop(lock);
        });
        sessions.insert(
            session_id,
            LiveEntry {
                session: Arc::clone(&session),
                driver: handle,
            },
        );
        // Here rather than at the callers: a session goes live through
        // create, attach and command alike, and until a `list` frame says
        // so every other client's directory reports it as on-disk only. An
        // attached-but-idle session emits no events of its own, so nothing
        // else would correct that.
        self.inner.shared.fanout.mark_list_dirty();
        Ok(session)
    }

    /// Whether the store holds a session `id` this host could materialize.
    ///
    /// Costs one `stat`: an id that is not one this store could ever hold has
    /// already been refused by [`validate_session_id`], so nothing here builds
    /// a path out of an unchecked string (spec 6.2).
    fn on_disk(&self, id: &str) -> Result<bool, HostError> {
        self.inner
            .cold
            .contains(id)
            .map_err(|err| HostError::Internal(Box::new(err)))
    }

    /// Serve one session's attach block on `id`'s stream.
    async fn serve_block(
        &self,
        id: fanout::SubscriberId,
        request: &AttachRequest,
        session: &Arc<LiveSession>,
        block: &Sender<Frame>,
        cancelled: &CancellationToken,
    ) -> bool {
        // The snapshot and the epoch are read under the log lock, because a
        // head switch moves both under it: reading them separately could
        // pair the old projection with the new epoch.
        let (snapshot, epoch, working_seen, settings_seen, finished_subs, driven_subs) = {
            let log = session.core.log.lock().await;
            let status = session.status();
            (
                log.snapshot(),
                status.epoch.clone(),
                status.working,
                status.settings.clone(),
                status.finished_subs.clone(),
                status.driven_subs.clone(),
            )
        };
        let boundary = snapshot.last_seq();
        // Only the epoch is checked here. A cursor past the boundary is treated
        // as a mismatch too (spec 6.5), which `project_suffix` does: it owns
        // the clamp because it is the layer that knows the log's own mark.
        let cursor = request
            .cursor
            .as_ref()
            .filter(|cursor| cursor.epoch == epoch)
            .map(|cursor| cursor.seq);
        // A run is live if the log names it and the host has not seen it
        // finish, or if the host is driving a turn for it (a continuation of
        // a run that did finish). Deriving it this way rather than tracking
        // the live set keeps the one unavoidable lag (a spawn root reaches
        // disk before the host consumes the run's `AgentStart`) on the safe
        // side: the worst case is a bracket left open a moment too long,
        // which the live `SubAgentEnd` closes, instead of a fabricated
        // conclusion for a running sub-agent.
        let live_subs: std::collections::BTreeSet<usize> = snapshot
            .sub_agent_ids()
            .difference(&finished_subs)
            .copied()
            .chain(driven_subs)
            .collect();
        // Projected outside the log lock: a full backfill walks the whole
        // log, and holding the lock would stall the session's next append
        // for the length of it.
        let backfill = project_suffix(&snapshot, cursor, &live_subs);

        if !send_block_frame(
            block,
            cancelled,
            Frame::State {
                session: session.id().to_string(),
                epoch: epoch.clone(),
                working: working_seen,
                settings: settings_seen.clone(),
                last_seq: boundary,
            },
        )
        .await
        {
            return false;
        }
        for tagged in backfill.events {
            if !send_block_frame(
                block,
                cancelled,
                Frame::Event {
                    session: session.id().to_string(),
                    epoch: epoch.clone(),
                    durability: tagged.entry.map(|entry| DurableEvent {
                        seq: entry.seq,
                        entry_id: entry.id,
                    }),
                    event: tagged.event.into(),
                },
            )
            .await
            {
                return false;
            }
        }
        if !send_block_frame(
            block,
            cancelled,
            Frame::CaughtUp {
                session: session.id().to_string(),
                epoch: epoch.clone(),
                last_seq: boundary,
            },
        )
        .await
        {
            return false;
        }
        // Every sub-agent the host knows to be idle is concluded after
        // `caught_up`. A sub whose `SubAgentEnd` fell into this client's
        // disconnected window would otherwise spin forever, and the
        // backfill cannot carry the conclusion itself when no durable entry
        // follows the cursor. The reducer's `AgentEnd` arm leaves a
        // concluded box alone, so the sweep is idempotent.
        //
        // Scoped to the runs the projection walked, and to the ones it did
        // not leave open. Both come out of the same walk as `live_subs`
        // above, so the sweep cannot contradict the brackets, and an
        // abandoned branch's runs (which the log names but the projection
        // never mentions) are left alone.
        for child in backfill.subs.difference(&backfill.open_subs) {
            if !send_block_frame(
                block,
                cancelled,
                Frame::Event {
                    session: session.id().to_string(),
                    epoch: epoch.clone(),
                    // Synthesized bracketing, so untagged: its spawn root sits
                    // at or below the cursor and tagging it durable would make
                    // the client's cursor invariant drop it.
                    durability: None,
                    event: aj_agent::events::AgentEvent::AgentEnd {
                        agent_id: AgentId::Sub(*child),
                        messages: Vec::new(),
                    }
                    .into(),
                },
            )
            .await
            {
                return false;
            }
        }
        self.inner
            .shared
            .fanout
            .finish_block(id, session.id(), boundary);

        // `working` and `settings` were read before the projection, and a
        // change during it was held and dropped as lossy. One more `state`
        // frame is what self-heals that. Only when something actually
        // moved: an unconditional re-emission would make every attach look
        // like a state change to every other client on the host.
        session.publish_state(&self.inner.shared.fanout, |status| {
            status.working != working_seen || status.settings != settings_seen
        });
        true
    }
}

/// Sends one attach-block frame under receiver backpressure.
async fn send_block_frame(
    sender: &Sender<Frame>,
    cancelled: &CancellationToken,
    frame: Frame,
) -> bool {
    tokio::select! {
        biased;
        _ = cancelled.cancelled() => false,
        result = sender.send(frame) => result.is_ok(),
    }
}

/// Refuse a session id that could never name a log in this store.
///
/// The wire treats session ids as opaque strings, so one arriving from a peer
/// is checked against the store's grammar before it reaches a path or a
/// lookup (spec 6.2). Membership in an enumeration is not a substitute: it
/// happens to be safe, but it makes path safety depend on how a lookup is
/// implemented, and it costs a directory read per question.
///
/// 404, because spec 6.2 says so. The store refuses the same ids at its own
/// door, which is what makes the safety hold whatever route reaches it, so
/// this gate is about *where* the refusal happens rather than whether it
/// does.
pub(crate) fn validate_session_id(session: &str) -> Result<(), HostError> {
    if aj_session::is_valid_session_id(session) {
        return Ok(());
    }
    Err(HostError::UnknownSession(session.to_string()))
}

/// Applies the same empty-prompt rule as the session driver before creation.
fn validate_prompt(content: &[UserContent]) -> Result<(), HostError> {
    if content.is_empty() {
        return Err(HostError::Invalid("the prompt is empty".to_string()));
    }
    let mut text = String::new();
    for block in content {
        let UserContent::Text(block) = block else {
            return Ok(());
        };
        text.push_str(&block.text);
    }
    if text.is_empty() {
        return Err(HostError::Invalid("the prompt is empty".to_string()));
    }
    Ok(())
}

fn validate_model_selection(selection: &ModelSelection) -> Result<(), HostError> {
    if selection.api.is_empty() || selection.name.is_empty() {
        return Err(HostError::Invalid(
            "model api and name must not be empty".to_string(),
        ));
    }
    if let Some(url) = selection.url.as_deref() {
        let parsed = url::Url::parse(url)
            .map_err(|err| HostError::Invalid(format!("invalid model url {url:?}: {err}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
            return Err(HostError::Invalid(format!(
                "model url {url:?} must be an absolute http or https URL"
            )));
        }
    }
    Ok(())
}

/// The modification time of `log`'s file, or `None` for a log that has none
/// yet (a created session defers its file to the first punctuating append).
fn log_modified_at(log: &ConversationLog) -> Option<DateTime<Utc>> {
    if !log.is_durable() {
        return None;
    }
    Some(std::fs::metadata(log.path()).ok()?.modified().ok()?.into())
}

/// Project one live session onto its directory entry.
fn summarize(session: &Arc<LiveSession>) -> SessionSummary {
    let (steering, follow_up) = session.core.message_queues.pending_counts();
    let tasks = session
        .core
        .task_registry
        .snapshot()
        .into_iter()
        .filter(|task| task.status == aj_agent::tool::TaskStatus::Running)
        .count();
    let status = session.status();
    SessionSummary {
        id: session.id().to_string(),
        live: true,
        working: status.working,
        queued: QueueCounts {
            steering,
            follow_up,
        },
        tasks,
        last_seq: Some(status.last_seq),
        last_activity: status.last_activity,
        tag: status.tag.clone(),
        host: None,
        unreachable: false,
    }
}

/// Project a monotonic `Instant` onto wall clock through `anchor`, a pair of
/// clocks read at the same moment.
///
/// The anchor is read once per host rather than per call, so the same task's
/// reported start time does not move between two reads of the table. Exact
/// enough for a task table either way: the two clocks drift by microseconds
/// over a host's lifetime.
fn wall_clock(anchor: (DateTime<Utc>, Instant), at: Instant) -> DateTime<Utc> {
    let (wall, instant) = anchor;
    // Signed: a task started after the anchor is ahead of it, and
    // `saturating_duration_since` would report it as the anchor itself.
    if at >= instant {
        wall + chrono::Duration::from_std(at.duration_since(instant)).unwrap_or_default()
    } else {
        wall - chrono::Duration::from_std(instant.duration_since(at)).unwrap_or_default()
    }
}

/// Mint a fresh epoch token.
///
/// Opaque and never persisted (spec 6.5): a host restart must invalidate
/// every cursor, because the log tail is not crash-stable and a
/// post-restart position may not mean what it meant before.
pub(crate) fn mint_epoch() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Read the store's host id, minting and writing one when it has none.
fn resolve_host_id(sessions_dir: &Path) -> Result<String, HostError> {
    let path = sessions_dir.join(HOST_ID_FILE);
    if let Some(id) = read_host_id(&path)? {
        return Ok(id);
    }
    std::fs::create_dir_all(sessions_dir).map_err(|err| HostError::Internal(Box::new(err)))?;
    let minted = format!("{:032x}", rand::random::<u128>());
    // `create_new`, because two hosts starting in one store must not both
    // mint: the store would be advertised under two ids and a gateway would
    // see one working directory as two hosts. The loser of the race reads
    // the winner's id back.
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, format!("{minted}\n").as_bytes())
                .map_err(|err| HostError::Internal(Box::new(err)))?;
            Ok(minted)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            read_host_id(&path)?.ok_or_else(|| {
                // The file exists and is blank, which only a crash between
                // the create and the write leaves behind. Overwriting it
                // would reopen the race this claim exists to close, so this
                // is the operator's call.
                HostError::Internal(
                    format!("{} is empty: remove it to mint a fresh id", path.display()).into(),
                )
            })
        }
        Err(err) => Err(HostError::Internal(Box::new(err))),
    }
}

/// The store's recorded host id, `None` when the file is absent or blank.
fn read_host_id(path: &Path) -> Result<Option<String>, HostError> {
    match std::fs::read_to_string(path) {
        Ok(id) if !id.trim().is_empty() => Ok(Some(id.trim().to_string())),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(HostError::Internal(Box::new(err))),
    }
}

/// Release sessions that have been idle and unattached for the host's grace
/// period (spec section 5).
///
/// Two clocks have to agree before a session goes, because neither sees the
/// whole picture. This task's own observation covers what the session cannot
/// report, its last client detaching, but it only samples on a tick, so work
/// that starts and ends between two ticks is invisible to it. The session's own
/// `last_work` covers exactly that, but it does not move when a client comes or
/// goes. A session is due when both say a full grace has passed.
///
/// Holds a weak reference, so a host whose last handle is gone lets this task
/// exit rather than keeping its sessions alive.
fn spawn_idle_sweeper(inner: &Arc<HostInner>) {
    let weak = Arc::downgrade(inner);
    let grace = inner.idle_grace;
    // Half the grace, so a session goes at most one tick later than its grace
    // is up. Floored because a test's grace can be tiny.
    let tick = (grace / 2).max(Duration::from_millis(1));
    tokio::spawn(async move {
        let mut idle_since: HashMap<String, Instant> = HashMap::new();
        loop {
            tokio::time::sleep(tick).await;
            let Some(inner) = weak.upgrade() else { return };
            let host = SessionHost { inner };
            if host.alive().is_err() {
                return;
            }
            let held: Vec<Arc<LiveSession>> = host
                .inner
                .sessions
                .lock()
                .await
                .values()
                .map(|entry| Arc::clone(&entry.session))
                .collect();
            let fanout = &host.inner.shared.fanout;
            let now = Instant::now();
            let mut due = Vec::new();
            for session in &held {
                if !live::releasable(session, fanout) {
                    idle_since.remove(session.id());
                    continue;
                }
                let observed = *idle_since.entry(session.id().to_string()).or_insert(now);
                let worked = session.status().last_work;
                if now.duration_since(observed) >= grace && now.duration_since(worked) >= grace {
                    due.push(session.id().to_string());
                }
            }
            // Drop what we remember about sessions this host no longer holds,
            // so the map tracks the live set rather than its history.
            idle_since.retain(|id, _| held.iter().any(|session| session.id() == id));
            for session in due {
                // A decline means the driver saw work this task did not, so the
                // observation starts over rather than retrying every tick.
                if !host.release_if_idle(&session).await {
                    idle_since.remove(&session);
                }
            }
        }
    });
}

/// Publish `list` frames on a coalescing tick.
///
/// Holds a weak reference so a host whose last handle is gone lets this
/// task exit rather than keeping the session map alive.
fn spawn_list_publisher(inner: &Arc<HostInner>) {
    let weak = Arc::downgrade(inner);
    let fanout = Arc::clone(&inner.shared.fanout);
    tokio::spawn(async move {
        loop {
            fanout.list_dirty().notified().await;
            tokio::time::sleep(LIST_COALESCE).await;
            let Some(inner) = weak.upgrade() else { return };
            let host = SessionHost { inner };
            // A shut-down host has drained its session map, so a directory
            // composed from it would report every torn-down session from its
            // cold row, dropping its position and walking its stamp backwards
            // on the last frame a client ever sees. Nothing will mark the
            // directory dirty again either.
            if host.alive().is_err() {
                return;
            }
            fanout.publish_list(host.directory().await.sessions);
        }
    });
}
