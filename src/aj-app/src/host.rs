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
    AppendHandoff, ConversationLog, ConversationPersistence, EntryId, LockHolder,
    PersistenceFailure, SessionLock, normalize_tag, project_suffix, validate_session_env,
};
use aj_wire::{
    ARCHIVE_CAPABILITY, AgentQueue, COMPACTION_USAGE_CAPABILITY, Cursor, DurableEvent, Frame,
    Hello, MAX_HOST_NAME_BYTES, ModelSelection, PROTOCOL_VERSION, QueueCounts, QueueState,
    SessionList, SessionSettings, SessionSummary, SessionTree, TaskDetails, TaskSummary, TaskTable,
    TreeSegment, normalize_host_name,
};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::{Sender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
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

/// Elapsed shutdown time at which unfinished ownership is reported. Currently
/// 30 seconds. This boundary does not release or transfer that ownership.
const HOST_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Forced-cleanup reserve between inner-driver abortion and the reporting
/// boundary. Currently 10 seconds, so inner drivers are aborted at 20 seconds
/// (`HOST_SHUTDOWN_GRACE - HOST_ABORT_GRACE`) and their outer owners get this
/// interval before the host reports that it is continuing to wait.
const HOST_ABORT_GRACE: Duration = Duration::from_secs(10);

/// How long a session stays live with nothing running and nobody attached
/// before the host releases it (spec section 5).
///
/// The tradeoff is resume cost against lock hold time. Shorter, and switching
/// away from a session and back re-resumes its whole log for nothing. Longer,
/// and another process in the same directory waits that much longer for a
/// session this one is done with, which is the failure this exists to fix.
pub const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(30);

/// The `error` frame code a session's stream carries when its conversation
/// log refused a write and the materialization ended over it (spec 6.5). A
/// client re-asks for the session at once: the host rebuilds it from disk.
pub const PERSISTENCE_FAILED_CODE: &str = "persistence_failed";

/// The one sentence a client sees for a fused log. A session with a canonical
/// log on disk reopens; one whose first publication failed has nothing to
/// reopen, so the user is sent to a new session instead.
pub(crate) fn persistence_failure_message(failure: &PersistenceFailure) -> String {
    if failure.can_reopen() {
        format!(
            "Saving this session failed: {failure}. To protect its history, AJ stopped the \
             session and will not save more work to it. Free disk space, then reopen the \
             session. The interrupted action may need to be retried."
        )
    } else {
        format!(
            "Saving this session failed: {failure}. The message you just sent was not \
             recorded. Fix the storage problem, then start a new session and resend it."
        )
    }
}

/// How often the host re-probes the sessions it publishes as locked.
///
/// The falling edge of the `locked` bit (spec 6.8), and the only recurring
/// question the host owes: a rival letting go is invisible otherwise, cleanly
/// or by crashing, and the client's half of the contract forbids it from asking
/// on a schedule (spec 6.5). Rising edges are events the host already has, its
/// own refusal and the enumeration sweep, so this tick only ever clears.
///
/// Deliberately its own constant rather than a share of the idle grace: the two
/// pace unrelated things, and tuning one must not silently move the other. The
/// scale is a rejoin the user is waiting through, and a tick over an empty set
/// is one set check, so seconds is what this costs nothing to make.
pub const LOCK_PROBE_TICK: Duration = Duration::from_secs(2);

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
    /// state: a turn is running, background work is live, or a lifecycle mark
    /// names work the host has no mechanism to mutate.
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
        /// The generation of this refused acquire, in the vocabulary of the
        /// row's `lock_generation` (spec 6.8).
        ///
        /// Captured by the same cache update that advances the row, so later
        /// acquires cannot change what this refusal carries.
        generation: Option<u64>,
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

    /// The acquire generation a `locked` refusal names, `None` from every other
    /// failure (spec 6.5, 6.8).
    ///
    /// Beside [`Self::code`] for the same reason that one is here: the frame
    /// that refuses one session's attach is assembled from this error, and the
    /// generation is part of what the refusal says rather than something the
    /// assembling code could look up. Asking the directory for it there would
    /// read whatever the bit had moved to since.
    fn lock_generation(&self) -> Option<u64> {
        match self {
            Self::Locked { generation, .. } => *generation,
            _ => None,
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
    /// What this host calls itself for a reader, `None` to derive a name
    /// from the working directory.
    ///
    /// A value that is not a legal name ([`aj_wire::normalize_host_name`]) is
    /// dropped in favour of the derivation, so nothing a caller states can put
    /// one on the wire. The `--name` flag refuses first, with a message, which
    /// is what keeps a name a host would not state from costing a startup.
    pub name: Option<String>,
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

/// The name a host reports when nothing named it: its working directory,
/// abbreviated to `~` under `home`.
///
/// The whole path and not its last segments, because which segments tell two
/// clones apart is not something a host can know about its neighbours.
/// `None` when the path makes no legal name, which leaves the host labelled
/// by its id as an older host's clients already have it.
fn derive_host_name(working_directory: &Path, home: Option<&Path>) -> Option<String> {
    let displayed = aj_conf::display_path_with_home(working_directory, home);
    normalize_host_name(keep_tail(&displayed)).ok().flatten()
}

/// `path` cut to [`MAX_HOST_NAME_BYTES`] from the end.
///
/// A path is told from another by its tail, so an over-long one loses its
/// head. Whole segments where one fits, so what is left reads as a path
/// rather than as a severed first segment, and the severed segment is dropped
/// only when dropping it leaves something to name.
fn keep_tail(path: &str) -> &str {
    if path.len() <= MAX_HOST_NAME_BYTES {
        return path;
    }
    let mut start = path.len() - MAX_HOST_NAME_BYTES;
    // Terminates: the length itself is a boundary.
    while !path.is_char_boundary(start) {
        start += 1;
    }
    if path.as_bytes()[start - 1] == b'/' {
        // The window already begins a segment, so there is nothing severed to
        // drop. `start` is at least one, because the cap is not zero.
        return &path[start..];
    }
    match path[start..].find('/') {
        Some(offset) if start + offset + 1 < path.len() => &path[start + offset + 1..],
        _ => &path[start..],
    }
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
    /// Set or clear the session's archived bit.
    ///
    /// Display metadata and nothing else: it touches nothing about the
    /// session's life, so a session working through a turn takes it without
    /// interruption. Explicit in both directions, `false` unarchives, and
    /// nothing else ever clears it.
    Archive {
        archived: bool,
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
    /// Persistence-writer lifetime state for composed lifecycle tests.
    #[cfg(any(test, feature = "test-support"))]
    pub persistence_fence: aj_session::PersistenceFence,
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
    /// This outer owner retains the session's advisory lock through detached
    /// task reaping and persistence fencing, so joining it is what releases the
    /// complete writer lifetime.
    driver: JoinHandle<()>,
    /// Map-independent stop controls for the complete session scope.
    stop: SessionStop,
}

/// Stop controls that remain reachable even while another task holds the async
/// session map. Task cancellation starts at host shutdown. Driver abortion is
/// the later cutoff that makes the outer owner begin forced cleanup.
#[derive(Clone)]
struct SessionStop {
    driver: AbortHandle,
    tasks: TaskRegistry,
}

/// Complete session stop handles retained while the host-owned teardown task
/// owns the outer session joins.
///
/// If runtime teardown drops the host task, every inner driver is still
/// cancelled. Dropping the outer join handles detaches the session owner tasks,
/// which retain their advisory locks until task reaping and persistence fencing
/// complete.
struct ShutdownStops(Vec<(String, SessionStop)>);

impl Drop for ShutdownStops {
    fn drop(&mut self) {
        for (_, stop) in &self.0 {
            stop.tasks.shutdown();
            stop.tasks.abort_drivers();
            if !stop.driver.is_finished() {
                stop.driver.abort();
            }
        }
    }
}

/// Close every attachment whenever the host-owned teardown task ends.
///
/// On the normal path this runs after session-owner joins. If runtime teardown
/// drops the task, [`ShutdownStops`] runs first and this still leaves no parked
/// stream.
struct ShutdownFinish<'a>(&'a HostInner);

impl Drop for ShutdownFinish<'_> {
    fn drop(&mut self) {
        self.0.shared.fanout.close();
    }
}

#[derive(Default)]
struct ShutdownState {
    started: bool,
    complete: bool,
}

struct HostInner {
    shared: Arc<HostShared>,
    persistence: ConversationPersistence,
    base_run_config: RunConfigSnapshot,
    host_id: String,
    working_directory: PathBuf,
    /// What this host calls itself on the wire: `--name`, else the working
    /// directory's abbreviation, else nothing.
    name: Option<String>,
    sessions: TokioMutex<HashMap<String, LiveEntry>>,
    /// Complete stop controls for every session, independently of the async
    /// session-map lock. Shutdown can cancel detached work and abort drivers
    /// even when a release is holding that map.
    session_stops: StdMutex<HashMap<String, SessionStop>>,
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
    /// One owned teardown task serves every shutdown caller. Cancelling a
    /// caller cannot transfer join ownership or make a later caller report
    /// completion early.
    shutdown: StdMutex<ShutdownState>,
    shutdown_changed: tokio::sync::Notify,
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
            // Leave the outer owner task alive. It observes this inner abort,
            // reaps detached tasks, fences persistence, and only then drops
            // the advisory lock. Dropping its JoinHandle merely detaches it.
            entry.stop.tasks.shutdown();
            entry.stop.tasks.abort_drivers();
            entry.stop.driver.abort();
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
            name,
            idle_grace,
            live_capacity,
        } = setup;
        let host_id = resolve_host_id(persistence.sessions_dir())?;
        // A startup fact like the id. Derived per hello instead, it could
        // answer two clients differently when the environment moves under a
        // long-lived process. A stated name is re-checked here because this
        // field is public and what it holds ends up painted on a peer.
        let name = name
            .and_then(|name| normalize_host_name(&name).ok().flatten())
            .or_else(|| derive_host_name(&working_directory, aj_conf::home_dir().as_deref()));
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
            name,
            sessions: TokioMutex::new(HashMap::new()),
            session_stops: StdMutex::new(HashMap::new()),
            clock_anchor: (Utc::now(), Instant::now()),
            idle_grace: idle_grace.unwrap_or(DEFAULT_IDLE_GRACE),
            shut_down: AtomicBool::new(false),
            shutdown: StdMutex::new(ShutdownState::default()),
            shutdown_changed: tokio::sync::Notify::new(),
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
        spawn_lock_probe(&inner);
        spawn_idle_sweeper(&inner);
        Ok(Self { inner })
    }

    /// Protocol identity and capabilities (spec 6.1).
    ///
    /// The list names the routes this host serves past the protocol-1
    /// baseline, which spec 6.10 asks a new endpoint to arrive with. It is
    /// self-description and not a gate: what a peer does with it is the peer's
    /// business, and a client that simply attempts a route and reads the
    /// refusal is following the same section.
    pub fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: vec![
                ARCHIVE_CAPABILITY.to_string(),
                COMPACTION_USAGE_CAPABILITY.to_string(),
            ],
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            host_id: self.inner.host_id.clone(),
            working_directory: Some(self.inner.working_directory.clone()),
            name: self.inner.name.clone(),
        }
    }

    /// Create a session in the host's working directory and hold it live.
    pub async fn create(&self) -> Result<String, HostError> {
        self.alive()?;
        self.mint(None, None).await
    }

    /// Whether a create naming `host` is this host's to serve (spec 6.6).
    ///
    /// A host serves exactly one working directory, so the only session it can
    /// create is its own: an absent name and this host's own id are the same
    /// request, and any other id is a create for a store this process does not
    /// hold. [`HostError::Unsupported`], so a 409 rather than a 400: nothing
    /// about the request is malformed, and the very same one against the host
    /// it names may well succeed.
    pub fn creates_here(&self, host: Option<&str>) -> Result<(), HostError> {
        let own = &self.inner.host_id;
        match host {
            None => Ok(()),
            Some(named) if named == own => Ok(()),
            Some(named) => Err(HostError::Unsupported(format!(
                "this host is {own}, not {named:?}: a host creates sessions only in its own \
                 working directory"
            ))),
        }
    }

    /// Creates a session with creator-selected settings, a first prompt, a tag,
    /// and an immutable environment map.
    ///
    /// Creation is the operation that either happens or does not. Every
    /// setting, the prompt, tag, and environment are validated before a
    /// log is created, so a request this host refuses
    /// ([`CreateError::Refused`]) leaves no discoverable empty session behind.
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
        session_env: Option<BTreeMap<String, String>>,
    ) -> Result<String, CreateError> {
        self.alive()?;
        if let Some(content) = prompt.as_deref() {
            validate_prompt(content)?;
        }
        let tag = normalize_tag(tag.as_deref().unwrap_or_default())
            .map_err(|err| HostError::Invalid(format!("tag: {err}")))?;
        if let Some(env) = session_env.as_ref() {
            validate_session_env(env)
                .map_err(|err| HostError::Invalid(format!("session env: {err}")))?;
        }
        let session = self.mint(settings.as_ref(), session_env).await?;
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
    async fn mint(
        &self,
        settings: Option<&SessionSettings>,
        session_env: Option<BTreeMap<String, String>>,
    ) -> Result<String, HostError> {
        let run_config = self.resolve_creator_settings(settings)?;
        let mut sessions = self.inner.sessions.lock().await;
        let live = self
            .materialize(&mut sessions, None, Some(run_config), session_env)
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
        let stopped = live_frames.block_stop_token();
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
                        // A locked refusal names its acquire generation, and
                        // every other code carries none (spec 6.5).
                        lock_generation: err.lock_generation(),
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
        let (attachment, block_tx, block_complete) = Attachment::new(
            id,
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
            let mut completed = true;
            for item in serving {
                let served = match item {
                    Serving::Block(request, session) => {
                        host.serve_block(id, &request, &session, &block_tx, &stopped)
                            .await
                    }
                    Serving::Refusal(frame) => send_block_frame(&block_tx, &stopped, frame).await,
                };
                if !served {
                    completed = false;
                    break;
                }
            }
            if completed {
                block_complete.finish();
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
                        archived: session.archived,
                        locked: session.locked,
                        lock_generation: session.lock_generation,
                    },
                )
            })
            .collect();
        // A live session always wins the id: the cold cache can still hold the
        // row a session had before it was materialized, and of those two
        // answers only the host's own is current.
        for session in &live {
            let id = session.id();
            // The one field a live row still takes from the cold cache. The
            // generation describes the session's lock history, not who holds it
            // now, so it survives this host taking the session: a client refused
            // over the hold that ended is owed the same evidence either way
            // (spec 6.8).
            let generation = self.inner.cold.lock_generation(id);
            summaries.insert(id.to_string(), summarize(session, generation));
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

    /// The directory as the host publishes it, without enumerating.
    ///
    /// [`Self::sessions`] is an enumeration point, so a test that wants to know
    /// what the host is publishing right now cannot ask through it: the ask
    /// would itself refresh what it is asking about. This is the same snapshot
    /// a `list` frame carries.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn published_directory(&self) -> SessionList {
        self.directory().await
    }

    /// Hold the live-session map until `release`, exposing entry through
    /// `entered` once the hold is established.
    ///
    /// Test support for the shutdown branch an idle release exercises while it
    /// joins a session owner. Production callers never coordinate on the map
    /// directly.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn hold_session_map_for_test(
        &self,
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    ) {
        let _sessions = self.inner.sessions.lock().await;
        let _ = entered.send(());
        let _ = release.await;
    }

    /// Offer one frame through the production live fan-out.
    ///
    /// Composed attach tests use this seam to place a delayed forwarder
    /// delivery after subscriber registration while the same durable event is
    /// already present in the attach snapshot. The frame still crosses the
    /// real attaching-state drop rule, live queue, boundary filter, and
    /// post-`CaughtUp` release.
    #[cfg(any(test, feature = "test-support"))]
    pub fn publish_live_frame_for_test(&self, frame: Frame) {
        self.inner.shared.fanout.publish(frame);
    }

    /// How many times the host has read its session store's `locks/`
    /// directory.
    ///
    /// The third of the enumeration's directory reads (spec 6.8), for the axis
    /// whose fact belongs to another writer. Like the sidecar listings it
    /// transfers no bytes, so no byte budget can see it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_lock_directory_reads(&self) -> u64 {
        self.inner.cold.lock_directory_reads()
    }

    /// How many session locks the host has probed.
    ///
    /// The per-file half of the lock axis's budget: the holder record filters
    /// which locks are worth asking about, so this is the only way to tell a
    /// settled store probing nothing from one asking per lock file it ever
    /// minted. A probe transfers no bytes either.
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_lock_probes(&self) -> u64 {
        self.inner.cold.lock_probes()
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
            #[cfg(any(test, feature = "test-support"))]
            persistence_fence: live.core.persistence_fence.clone(),
            restore_notices: live.core.restore_notices.clone(),
        })
    }

    /// Tear every live session down and close every client stream.
    ///
    /// Each session's turns are cancelled through the graceful path (so
    /// transcripts stay consistent), its background tasks quiesced, and its
    /// log given a bounded final flush. Its advisory lock is released only after
    /// the session owner has also reaped detached tasks and fenced old
    /// persistence listeners. Drivers wind down concurrently under one
    /// host-wide escalation point. A driver still running at the cutoff is
    /// named and aborted. The call continues awaiting each outer owner after
    /// that point because returning while its detached process driver or
    /// advisory lock remains live would report a shutdown that has not happened.
    ///
    /// Terminal: every later request fails rather than rebuilding a session
    /// behind a driver nobody will ever tell to stop.
    pub async fn shutdown(&self) {
        let start = {
            let mut state = self
                .inner
                .shutdown
                .lock()
                .expect("host shutdown mutex poisoned");
            if state.started {
                false
            } else {
                // Set before the owner can await anything, so a request cannot
                // materialize a session behind teardown.
                state.started = true;
                self.inner.shut_down.store(true, Ordering::Release);
                true
            }
        };
        if start {
            // Teardown belongs to the host, not to whichever frontend first
            // awaited it. Every caller waits on the same completion fact, and
            // cancelling one caller leaves all joins with this owned task.
            let host = self.clone();
            tokio::spawn(async move {
                host.shutdown_owned().await;
                host.inner
                    .shutdown
                    .lock()
                    .expect("host shutdown mutex poisoned")
                    .complete = true;
                host.inner.shutdown_changed.notify_waiters();
            });
        }

        loop {
            let changed = self.inner.shutdown_changed.notified();
            if self
                .inner
                .shutdown
                .lock()
                .expect("host shutdown mutex poisoned")
                .complete
            {
                return;
            }
            changed.await;
        }
    }

    async fn shutdown_owned(&self) {
        let _finish = ShutdownFinish(&self.inner);
        // Cancel before a driver's advisory lock can be released. The producer
        // may finish computation over a snapshot it already owns, but every
        // later log acquisition and send prefers cancellation, so it cannot
        // read from or emit through a rival writer.
        self.inner.shared.fanout.stop_blocks();
        let deadline = tokio::time::Instant::now() + HOST_SHUTDOWN_GRACE;
        let graceful_deadline = deadline - HOST_ABORT_GRACE;
        let mut stops = ShutdownStops(
            self.inner
                .session_stops
                .lock()
                .expect("session stops mutex poisoned")
                .iter()
                .map(|(session, stop)| (session.clone(), stop.clone()))
                .collect(),
        );
        // Detached work can observe its session root without waiting for a
        // command ahead of Request::Shutdown. This also covers the map-held
        // path below, where LiveEntry itself is not reachable until cutoff.
        for (_, stop) in &stops.0 {
            stop.tasks.shutdown();
        }
        let mut map_aborted = HashSet::new();
        let entries: Vec<LiveEntry> = match tokio::time::timeout_at(
            graceful_deadline,
            self.inner.sessions.lock(),
        )
        .await
        {
            Ok(mut sessions) => sessions.drain().map(|(_, entry)| entry).collect(),
            Err(_) => {
                // An idle release can hold the map while its request is queued
                // behind a stuck command. The independent handles are what let
                // the host end those drivers without first acquiring the map.
                for (session, stop) in &stops.0 {
                    if !stop.driver.is_finished() {
                        tracing::warn!(
                            session = session.as_str(),
                            phase = "session map drain",
                            "the live session map remained locked through the graceful cutoff; aborting its driver before the host deadline"
                        );
                        stop.driver.abort();
                        map_aborted.insert(session.clone());
                    }
                }
                match tokio::time::timeout_at(deadline, self.inner.sessions.lock()).await {
                    Ok(mut sessions) => sessions.drain().map(|(_, entry)| entry).collect(),
                    Err(_) => {
                        tracing::warn!(
                            phase = "session map drain",
                            "the live session map remained locked through the host deadline; continuing to await complete session ownership"
                        );
                        let mut sessions = self.inner.sessions.lock().await;
                        sessions.drain().map(|(_, entry)| entry).collect()
                    }
                }
            }
        };

        let mut drivers = JoinSet::new();
        let mut pending_reapers = HashSet::new();
        for entry in entries {
            let LiveEntry {
                session,
                driver,
                stop,
            } = entry;
            let id = session.id().to_string();
            pending_reapers.insert(id.clone());
            session.request_shutdown();
            // The outer owner holds the session lock through task reaping and
            // persistence fencing, so joining it is what releases the complete
            // writer lifetime. Dropping our own handle first keeps the session
            // from outliving that owner.
            drop(session);
            if !stops.0.iter().any(|(session, _)| session == &id) {
                stop.tasks.shutdown();
                stops.0.push((id.clone(), stop));
            }
            drivers.spawn(async move { (id, driver.await) });
        }

        while !drivers.is_empty() {
            match tokio::time::timeout_at(graceful_deadline, drivers.join_next()).await {
                Ok(Some(joined)) => {
                    remove_joined_session(&mut pending_reapers, &joined);
                    warn_driver_join(joined);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        for (session, stop) in &stops.0 {
            if !stop.driver.is_finished() {
                if !map_aborted.contains(session) {
                    tracing::warn!(
                        session = session.as_str(),
                        phase = "session driver join",
                        "session driver did not finish before the abort cutoff; aborting it before the host deadline"
                    );
                }
                stop.driver.abort();
            }
        }
        while !drivers.is_empty() {
            match tokio::time::timeout_at(deadline, drivers.join_next()).await {
                Ok(Some(joined)) => {
                    remove_joined_session(&mut pending_reapers, &joined);
                    warn_driver_join(joined);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        if !pending_reapers.is_empty() {
            tracing::warn!(
                sessions = ?pending_reapers,
                phase = "session owner reap",
                "session cleanup remained live at the host deadline; continuing to await detached drivers and advisory-lock release"
            );
        }
        // The deadline is an escalation and reporting boundary, not an
        // ownership boundary. Each outer owner holds the advisory lock and the
        // registry's process-reap fence, so dropping these joins would let
        // shutdown return while work still reports Running and a rival writer
        // still cannot acquire the session.
        while let Some(joined) = drivers.join_next().await {
            remove_joined_session(&mut pending_reapers, &joined);
            warn_driver_join(joined);
        }
        debug_assert!(pending_reapers.is_empty());
        self.inner
            .session_stops
            .lock()
            .expect("session stops mutex poisoned")
            .clear();
    }

    /// Release `session` if its driver reports it idle, answering whether it
    /// went (spec section 5).
    ///
    /// The session map is held for the whole teardown, which is what makes
    /// release serialize with materialization: a command or attach that arrives
    /// meanwhile waits here and then materializes afresh, rather than building
    /// a second core for a session whose outgoing owner still holds the lock.
    /// The owner is joined under the same hold because joining it is what
    /// releases the lock. Awaiting it under the map cannot deadlock: neither
    /// the driver nor its owner reaches back for the map (see [`HostShared`]).
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
        // The driver returns right after answering. The outer owner then
        // completes task and persistence cleanup before dropping the lock.
        let _ = entry.driver.await;
        self.inner
            .session_stops
            .lock()
            .expect("session stops mutex poisoned")
            .remove(session);
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
        let live = self
            .materialize(&mut sessions, Some(session), None, None)
            .await?;
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
    ///
    /// Either answer records the session's `locked` bit (spec 6.8): a refusal
    /// says a rival holds it, and a won lock clears any stale rival bit. Every
    /// answer advances the session's generation before its row is published.
    fn acquire(&self, id: &str) -> Result<SessionLock, HostError> {
        let taken = SessionLock::try_acquire(&self.inner.persistence, id, &self.inner.host_id)
            .map_err(|err| HostError::Internal(Box::new(err)))?;
        // Keep the post-increment value from the same cache guard that writes
        // the row. Re-reading later could stamp this refusal with another
        // acquire's generation.
        let generation = self.inner.cold.note_acquire(id, taken.is_none());
        self.inner.shared.fanout.mark_list_dirty();
        taken.ok_or_else(|| HostError::Locked {
            session: id.to_string(),
            // Read only on the refusal path, and the record is cleared on
            // release, so what it names is a holder that has the lock now.
            holder: SessionLock::holder(&self.inner.persistence, id),
            generation: Some(generation),
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
        create_session_env: Option<BTreeMap<String, String>>,
    ) -> Result<Arc<LiveSession>, HostError> {
        if let Some(id) = id {
            if let Some(entry) = sessions.get(id) {
                if !entry.session.is_draining() {
                    return Ok(Arc::clone(&entry.session));
                }
                // A draining entry under the map is a materialization ending
                // over a fused log (a release or shutdown holds the map for
                // its whole teardown, so neither is observable here). It is
                // as good as absent: join its owner, which is what releases
                // the session lock, and rebuild from disk below.
                let entry = sessions
                    .remove(id)
                    .expect("the entry is ours: the map has been held throughout");
                let _ = entry.driver.await;
                self.inner
                    .session_stops
                    .lock()
                    .expect("session stops mutex poisoned")
                    .remove(id);
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
                session_env: create_session_env,
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
        let (events, persistence_failure) = core.install_persisting_forwarder(&handoff).await;
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
        // One `stat` beside it, and the same reasoning about a read that
        // failed: the session goes live with the bit the host last knew.
        let archived = match self.inner.persistence.read_archived(&session_id) {
            Ok(archived) => archived,
            Err(err) => {
                tracing::warn!(
                    session = session_id,
                    "could not read whether the session is archived: {err}"
                );
                self.inner.cold.archived(&session_id)
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
            archived,
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
            persistence_failure,
        );
        // Synchronize the terminal check with shutdown's abort snapshot. If
        // shutdown wins, this materialization drops its lock and core without
        // starting a driver behind a host that has already torn down.
        let mut session_stops = self
            .inner
            .session_stops
            .lock()
            .expect("session stops mutex poisoned");
        self.alive()?;
        // The advisory lock belongs to an outer owner task, not to the driver
        // future that host cutoff may have to abort. Whatever way the driver
        // ends, the owner cancels and reaps detached tasks and drains every
        // persistence-listener invocation admitted before the fence closes.
        // Only then can a rival writer acquire the session.
        let task_registry = session.core.task_registry.clone();
        let persistence_fence = session.core.persistence_fence.clone();
        let inner = tokio::spawn(driver.run());
        let driver_abort = inner.abort_handle();
        let stop = SessionStop {
            driver: driver_abort,
            tasks: task_registry.clone(),
        };
        let owner_session = session_id.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = inner.await
                && !err.is_cancelled()
            {
                tracing::warn!(
                    session = owner_session,
                    phase = "session driver join",
                    "session driver ended abnormally: {err}"
                );
            }
            if !crate::shutdown_background_tasks_owned(&task_registry).await {
                tracing::warn!(
                    session = owner_session,
                    phase = "background task quiesce",
                    "forced background-task teardown completed after the shutdown grace"
                );
            }
            persistence_fence.close().await;
            drop(lock);
        });
        session_stops.insert(session_id.clone(), stop.clone());
        drop(session_stops);
        sessions.insert(
            session_id,
            LiveEntry {
                session: Arc::clone(&session),
                driver: handle,
                stop,
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
        stopped: &CancellationToken,
    ) -> bool {
        // The snapshot and the epoch are read under the log lock, because a
        // head switch moves both under it: reading them separately could
        // pair the old projection with the new epoch.
        let (snapshot, epoch, working_seen, settings_seen, finished_subs, driven_subs) = {
            let log = tokio::select! {
                biased;
                _ = stopped.cancelled() => return false,
                log = session.core.log.lock() => log,
            };
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
            stopped,
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
                stopped,
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
        // The opening half of the lifecycle repair: a sub-agent still
        // running when this client attached announced itself only through a
        // live `AgentStart` this client never saw, so the block synthesizes
        // it. After the backfill events so the box the reducer's
        // `AgentStart` arm reopens exists, and before `caught_up` so the
        // lifecycle set is complete the moment the attach flips live, with
        // no window in which the footer, the picker, or a busy-gated
        // gesture reads the sub as idle. `open_subs` is already
        // liveness-scoped (`close_finished_runs` force-closes non-live
        // runs), so no second filter here. Idempotent on re-attach:
        // `mark_running` is a set insert and `reopen_sub_box` leaves a
        // running box alone.
        for child in &backfill.open_subs {
            if !send_block_frame(
                block,
                stopped,
                Frame::Event {
                    session: session.id().to_string(),
                    epoch: epoch.clone(),
                    // Synthesized bracketing, so untagged: like the closing
                    // sweep below, tagging it durable would make the
                    // client's cursor invariant drop it.
                    durability: None,
                    event: aj_agent::events::AgentEvent::AgentStart {
                        agent_id: AgentId::Sub(*child),
                    }
                    .into(),
                },
            )
            .await
            {
                return false;
            }
        }
        if !send_block_frame(
            block,
            stopped,
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
                stopped,
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

/// Report an abnormal session-driver join. Cancellation is the expected
/// outcome after the host's abort cutoff.
fn remove_joined_session(
    pending: &mut HashSet<String>,
    joined: &Result<(String, Result<(), tokio::task::JoinError>), tokio::task::JoinError>,
) {
    if let Ok((session, _)) = joined {
        pending.remove(session);
    }
}

fn warn_driver_join(
    joined: Result<(String, Result<(), tokio::task::JoinError>), tokio::task::JoinError>,
) {
    match joined {
        Ok((_, Ok(()))) => {}
        Ok((_, Err(err))) if err.is_cancelled() => {}
        Ok((session, Err(err))) => {
            tracing::warn!(
                session,
                phase = "session driver join",
                "session driver ended abnormally during shutdown: {err}"
            );
        }
        Err(err) => {
            tracing::warn!(
                phase = "session driver join",
                "session driver join waiter ended abnormally during shutdown: {err}"
            );
        }
    }
}

/// Sends one attach-block frame under receiver backpressure.
async fn send_block_frame(
    sender: &Sender<Frame>,
    stopped: &CancellationToken,
    frame: Frame,
) -> bool {
    tokio::select! {
        biased;
        _ = stopped.cancelled() => false,
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
///
/// `lock_generation` comes from the host's lock bookkeeping rather than from the
/// session, which knows nothing about the holds that preceded it.
fn summarize(session: &Arc<LiveSession>, lock_generation: Option<u64>) -> SessionSummary {
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
        archived: status.archived,
        // Never locked: the bit names a rival, and this host holds this
        // session's lock for as long as it is live (spec 6.8).
        locked: false,
        lock_generation,
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
                // A session whose driver ended on its own (a fused log) has
                // nothing to wait a grace for: `release_if_idle` reaps it.
                if session.driver_gone() {
                    due.push(session.id().to_string());
                    continue;
                }
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

/// Clear the `locked` bit of any session whose rival has let go.
///
/// The host's half of spec 6.5's rejoin contract. A refused client is forbidden
/// to ask on a schedule, which buys it the host's diligence instead, and this is
/// where that debt is paid: the rising edges are events the host already has,
/// its own refusal and the enumeration sweep, and the falling edge has none at
/// all. A clean release truncates the holder record and a crash releases by
/// closing a descriptor, and neither reaches this process. Asking the flock is
/// the only read the fact supports, so this paces that read rather than standing
/// in for a signal.
///
/// Both release paths are bounded by the same constant, because a probe asks
/// whether the lock is held rather than waiting to be told that it was dropped.
///
/// Costs nothing on a settled host: the set of published locks is read first and
/// almost always empty, so a tick is a set check, and the session map is not
/// even locked. A non-empty set costs one probe per member, which is a few
/// microseconds each.
///
/// Holds a weak reference, so a host whose last handle is gone lets this task
/// exit rather than probing for one nobody holds.
fn spawn_lock_probe(inner: &Arc<HostInner>) {
    let weak = Arc::downgrade(inner);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LOCK_PROBE_TICK).await;
            let Some(inner) = weak.upgrade() else { return };
            let host = SessionHost { inner };
            if host.alive().is_err() {
                return;
            }
            let published = host.inner.cold.locked();
            if published.is_empty() {
                continue;
            }
            // A session this host holds is never published as locked, so this
            // filters nothing in the normal case. It is here because the one
            // way the set could hold a live session is a bug elsewhere, and the
            // probe would then read this host's own flock as a rival's and pin
            // the bit true for as long as the session lived.
            let live: HashSet<String> = host.inner.sessions.lock().await.keys().cloned().collect();
            let mut freed = false;
            for session in published {
                if live.contains(&session) {
                    continue;
                }
                match host.inner.cold.probe_lock(&session) {
                    Ok(true) => {}
                    Ok(false) => freed |= host.inner.cold.note_unlocked(&session),
                    // The lock is unreadable, which says nothing about who
                    // holds it. The bit stands and the next tick asks again.
                    Err(err) => {
                        tracing::warn!("could not probe the lock of {session}: {err}")
                    }
                }
            }
            if freed {
                host.inner.shared.fanout.mark_list_dirty();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The name a host falls back to is its whole working directory, written
    /// the way a person writes it. Not its last segments: how deep two clones
    /// differ is not something a host can know.
    #[test]
    fn a_derived_name_is_the_working_directory_under_a_tilde() {
        let home = Path::new("/home/umber");
        assert_eq!(
            derive_host_name(Path::new("/home/umber/work/umber/aj"), Some(home)).as_deref(),
            Some("~/work/umber/aj"),
        );
        assert_eq!(
            derive_host_name(home, Some(home)).as_deref(),
            Some("~"),
            "a host serving home itself is named for it, with nothing trailing",
        );
    }

    /// Only a directory under home is abbreviated, and the abbreviation needs
    /// a home to measure against.
    #[test]
    fn a_directory_outside_home_keeps_its_absolute_path() {
        let outside = Path::new("/srv/build/aj");
        assert_eq!(
            derive_host_name(outside, Some(Path::new("/home/umber"))).as_deref(),
            Some("/srv/build/aj"),
        );
        assert_eq!(
            derive_host_name(outside, None).as_deref(),
            Some("/srv/build/aj"),
            "with no home there is nothing to abbreviate against",
        );
    }

    /// Over the cap the head goes: a path is told from another by its tail.
    /// Whole segments where one fits, so what is left still reads as a path.
    #[test]
    fn an_overlong_path_keeps_its_tail() {
        let home = Path::new("/home/u");
        let long_segment = "a".repeat(76);
        let deep = home.join(&long_segment).join("aj");
        assert_eq!(
            derive_host_name(&deep, Some(home)).as_deref(),
            Some(format!("{long_segment}/aj").as_str()),
            "the `~/` head goes and the segment under it survives whole",
        );

        let one_segment = format!("/{}", "b".repeat(100));
        assert_eq!(
            derive_host_name(Path::new(&one_segment), None).as_deref(),
            Some("b".repeat(MAX_HOST_NAME_BYTES).as_str()),
            "a segment that does not fit is cut, because there is nothing else to keep",
        );
    }

    /// A segment is dropped because it was severed, not because it is at the
    /// front. When the cut lands exactly on a separator the whole window is
    /// already whole segments, and dropping the first would throw away the
    /// part that tells this host from its neighbour.
    #[test]
    fn a_tail_that_starts_on_a_separator_keeps_every_segment_it_has() {
        let head = "a".repeat(10);
        let tail = "b".repeat(69);
        let path = format!("/{head}/{tail}");
        assert_eq!(path.len(), MAX_HOST_NAME_BYTES + 1, "one byte over the cap");

        assert_eq!(
            derive_host_name(Path::new(&path), None).as_deref(),
            Some(format!("{head}/{tail}").as_str()),
            "only the leading separator went",
        );
    }

    /// Dropping the severed segment must leave a name behind. A path whose
    /// only separator inside the cap is its last byte has nothing after it.
    #[test]
    fn a_trailing_separator_does_not_swallow_the_whole_name() {
        let path = format!("/{}/", "a".repeat(90));
        let name = derive_host_name(Path::new(&path), None).expect("a legal name");
        assert!(name.len() <= MAX_HOST_NAME_BYTES, "{name:?} fits the cap");
        assert!(path.ends_with(&name), "{name:?} is the tail of {path:?}");
    }

    /// The cut lands on a character boundary rather than inside a character,
    /// so a path of multi-byte segments yields a name and not a panic.
    #[test]
    fn an_overlong_path_of_wide_characters_is_cut_between_characters() {
        let wide = format!("/{}", "€".repeat(40));
        assert!(
            !wide.is_char_boundary(wide.len() - MAX_HOST_NAME_BYTES),
            "the naive cut would split a character"
        );

        let name = derive_host_name(Path::new(&wide), None).expect("a legal name");
        assert!(name.len() <= MAX_HOST_NAME_BYTES, "{name:?} fits the cap");
        assert!(wide.ends_with(&name), "{name:?} is the tail of {wide:?}");
        assert!(
            name.chars().all(|char| char == '€'),
            "whole characters only: {name:?}"
        );
    }

    /// A working directory that makes no legal name leaves the host labelled
    /// by its id, which is what a client did with every host before names.
    #[test]
    fn a_path_that_makes_no_legal_name_yields_none() {
        assert_eq!(derive_host_name(Path::new("/srv/two\nlines"), None), None);
        assert_eq!(derive_host_name(Path::new(""), None), None);
    }
}
