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
//! them.

pub(crate) mod driver;
mod fanout;
mod live;

use std::collections::{BTreeMap, HashMap};
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
    AppendHandoff, ConversationLog, ConversationPersistence, EntryId, SessionLock, project_suffix,
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
use crate::host::live::{LiveSession, Request, SessionStatus, settings_of};
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
    #[error("session {0} is held by another writer")]
    Locked(String),
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
    /// Switch the session's head to `entry`. Refused while work is live.
    Head {
        entry: EntryId,
    },
    KillTask {
        task: TaskId,
    },
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

/// Direct handles into one live session, for a client attached in process.
///
/// Spec section 5 sanctions this: the local frontend attaches "through direct
/// handles and channels, not through HTTP". It is a **read** surface. The
/// pending-message box re-reads the live queues at draw time, the footer the
/// run config and the task registry, and none of that goes through a command.
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
}

/// One live session plus the task driving it.
struct LiveEntry {
    session: Arc<LiveSession>,
    /// Awaited at shutdown. The driver holds the session's advisory lock,
    /// so joining the task is what releases it.
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
    /// Per-session durable high-water marks for sessions that are not live,
    /// keyed by the log-file fingerprint they were counted from.
    ///
    /// Entries for sessions that have since been removed from the store
    /// linger, which is bounded by the store's session count and costs a
    /// `u64` each.
    cold_last_seq: StdMutex<HashMap<String, CachedLastSeq>>,
    /// Set by [`SessionHost::shutdown`], and never cleared: a host is torn
    /// down once. Every operation refuses afterwards (see
    /// [`SessionHost::alive`]).
    shut_down: AtomicBool,
}

/// One cold session's entry count, plus the file fingerprint it was counted
/// from. A file whose modification time and size have not moved cannot have
/// grown an entry.
struct CachedLastSeq {
    modified: DateTime<Utc>,
    size: u64,
    last_seq: u64,
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
        } = setup;
        let host_id = resolve_host_id(persistence.sessions_dir())?;
        let inner = Arc::new(HostInner {
            shared: Arc::new(HostShared {
                config,
                layers,
                catalog,
                auth,
                restore,
                fanout: Arc::new(Fanout::default()),
            }),
            persistence,
            base_run_config: run_config,
            host_id,
            working_directory,
            sessions: TokioMutex::new(HashMap::new()),
            clock_anchor: (Utc::now(), Instant::now()),
            cold_last_seq: StdMutex::new(HashMap::new()),
            shut_down: AtomicBool::new(false),
        });
        spawn_list_publisher(&inner);
        Ok(Self { inner })
    }

    /// Protocol identity and capabilities (spec 6.1). Phase 1 advertises
    /// none: there is no transport yet, so there is nothing to negotiate.
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
        self.create_with(None, None).await
    }

    /// Creates a session with creator-selected settings and a first prompt.
    ///
    /// Every setting and the prompt are validated before a log is created, so
    /// a refused request leaves no discoverable empty session behind.
    pub async fn create_with(
        &self,
        settings: Option<SessionSettings>,
        prompt: Option<Vec<UserContent>>,
    ) -> Result<String, HostError> {
        self.alive()?;
        if let Some(content) = prompt.as_deref() {
            validate_prompt(content)?;
        }
        let run_config = self.resolve_creator_settings(settings.as_ref())?;
        let mut sessions = self.inner.sessions.lock().await;
        let live = self
            .materialize(&mut sessions, None, Some(run_config))
            .await?;
        let session = live.id().to_string();
        drop(sessions);
        if let Some(content) = prompt {
            self.command(
                &session,
                Command::Prompt {
                    agent: AgentId::Main,
                    content,
                },
            )
            .await?;
        }
        Ok(session)
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

    /// Open a stream and serve an attach block for every named session.
    ///
    /// Registering the subscriber and projecting a session's suffix are
    /// atomic with respect to that session's event flow: a durable frame
    /// published in between is either already in the backfill (and filtered
    /// against its boundary) or above it (and delivered), so a client can
    /// neither miss one nor be served one twice. That is what makes attaching
    /// a single round trip with no client-side buffer-and-reconcile dance
    /// (spec 6.5).
    ///
    /// Returning successfully means every named session's block will be
    /// written, which is what [`Attachment::attached`] reports and what a
    /// client arms its fold from.
    pub async fn attach(&self, requests: &[AttachRequest]) -> Result<Attachment, HostError> {
        self.alive()?;
        // One block per named session is the client contract (spec 6.5), and
        // a duplicate would be served two: the second would open a block
        // the client is not expecting and quiesce state it just applied.
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
        // Materialize everything up front: a failure must not leave a
        // half-served stream behind.
        let mut live = Vec::with_capacity(requests.len());
        for request in requests {
            live.push((request.clone(), self.live(&request.session).await?));
        }
        let (id, live_frames, cancelled) = self.inner.shared.fanout.register(&names);
        let (block_tx, block_rx) = channel(1);
        let attachment = Attachment::new(
            id,
            block_rx,
            live_frames,
            cancelled.clone(),
            names,
            Arc::clone(&self.inner.shared.fanout),
        );
        // Registered above before any block is projected: from here on every
        // frame this host publishes is either queued behind the block or filtered
        // against its boundary, which is the atomicity the doc promises.
        let host = self.clone();
        tokio::spawn(async move {
            for (request, session) in live {
                if !host
                    .serve_block(id, &request, &session, &block_tx, &cancelled)
                    .await
                {
                    break;
                }
            }
        });
        Ok(attachment)
    }

    /// Apply a command to `session`, materializing it if it is only on disk.
    pub async fn command(
        &self,
        session: &str,
        command: Command,
    ) -> Result<CommandOutcome, HostError> {
        let live = self.live(session).await?;
        let (reply, outcome) = oneshot::channel();
        if !live.send(Request::Command { command, reply }) {
            return Err(HostError::Internal("session driver is gone".into()));
        }
        outcome
            .await
            .map_err(|_| HostError::Internal("session driver dropped the request".into()))?
    }

    /// Every session of the host's working directory, on-disk ones as well
    /// as live ones. The discovery surface (spec 6.7): there is no separate
    /// on-disk listing.
    pub async fn sessions(&self) -> Result<SessionList, HostError> {
        self.alive()?;
        // The store scan is blocking IO, so it runs before the map lock is
        // taken rather than under it.
        let on_disk = self
            .inner
            .persistence
            .list_sessions()
            .map_err(|err| HostError::Internal(Box::new(err)))?;
        let live: Vec<Arc<LiveSession>> = self
            .inner
            .sessions
            .lock()
            .await
            .values()
            .map(|entry| Arc::clone(&entry.session))
            .collect();

        let mut summaries: BTreeMap<String, SessionSummary> = BTreeMap::new();
        for metadata in on_disk {
            let last_seq = self.cold_last_seq(&metadata);
            summaries.insert(
                metadata.session_id.clone(),
                SessionSummary {
                    id: metadata.session_id,
                    live: false,
                    working: false,
                    queued: QueueCounts::default(),
                    tasks: 0,
                    last_seq,
                    last_activity: metadata.modified_at,
                    unreachable: false,
                },
            );
        }
        for session in live {
            summaries.insert(session.id().to_string(), summarize(&session));
        }
        // Latest first: session ids are minted as timestamps, so their
        // descending order is chronological.
        let mut sessions: Vec<SessionSummary> = summaries.into_values().collect();
        sessions.sort_by(|left, right| right.id.cmp(&left.id));
        Ok(SessionList { sessions })
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
        })
    }

    /// The session's accumulated token usage, for an end-of-run report.
    ///
    /// `None` for a session that is not live: usage is per process, so a
    /// session this host never held spent nothing. Locks the agent, so a
    /// turn in flight holds this up for the length of that turn.
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
        self.alive()?;
        let mut sessions = self.inner.sessions.lock().await;
        let live = self.materialize(&mut sessions, Some(session), None).await?;
        drop(sessions);
        Ok(live)
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
        if let Some(entry) = self.inner.sessions.lock().await.get(session) {
            return Ok(Some(Arc::clone(&entry.session)));
        }
        if !self.on_disk(session)? {
            return Err(HostError::UnknownSession(session.to_string()));
        }
        Ok(None)
    }

    /// The durable high-water mark of a session that is not live, counted
    /// off its log.
    ///
    /// Derived rather than reported as zero, because the unseen-output glyph
    /// a client derives (spec 6.8) is about exactly the sessions it has not
    /// attached, which is most of them. Counting is O(file) and a list tick
    /// covers the whole store, hence the cache against the file's
    /// fingerprint: a log whose modification time and size have not moved
    /// cannot have grown an entry.
    ///
    /// A log that cannot be read counts zero: a directory listing must not
    /// fail over one unreadable file.
    fn cold_last_seq(&self, metadata: &aj_session::SessionMetadata) -> u64 {
        let fingerprint = (metadata.modified_at, metadata.size_bytes);
        {
            let cache = self.cold_last_seq_cache();
            if let Some(cached) = cache.get(&metadata.session_id)
                && (cached.modified, cached.size) == fingerprint
            {
                return cached.last_seq;
            }
        }
        let last_seq = match self.inner.persistence.stored_last_seq(&metadata.session_id) {
            Ok(last_seq) => last_seq,
            Err(err) => {
                tracing::warn!(
                    session = metadata.session_id,
                    "could not count the log's entries: {err}"
                );
                return 0;
            }
        };
        self.cold_last_seq_cache().insert(
            metadata.session_id.clone(),
            CachedLastSeq {
                modified: fingerprint.0,
                size: fingerprint.1,
                last_seq,
            },
        );
        last_seq
    }

    fn cold_last_seq_cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CachedLastSeq>> {
        self.inner
            .cold_last_seq
            .lock()
            .expect("cold last-seq cache mutex poisoned")
    }

    /// Take a session's advisory lock, refusing when another writer holds
    /// it.
    fn acquire(&self, id: &str) -> Result<SessionLock, HostError> {
        SessionLock::try_acquire(&self.inner.persistence, id)
            .map_err(|err| HostError::Internal(Box::new(err)))?
            .ok_or_else(|| HostError::Locked(id.to_string()))
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
                head: None,
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
            last_activity: Utc::now(),
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

    fn on_disk(&self, id: &str) -> Result<bool, HostError> {
        Ok(self
            .inner
            .persistence
            .list_sessions()
            .map_err(|err| HostError::Internal(Box::new(err)))?
            .iter()
            .any(|metadata| metadata.session_id == id))
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
        let refreshed = {
            let status = session.status();
            (status.working != working_seen || status.settings != settings_seen).then(|| {
                Frame::State {
                    session: session.id().to_string(),
                    epoch: status.epoch.clone(),
                    working: status.working,
                    settings: status.settings.clone(),
                    last_seq: status.last_seq,
                }
            })
        };
        if let Some(refreshed) = refreshed {
            self.inner.shared.fanout.publish(refreshed);
        }
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
        last_seq: status.last_seq,
        last_activity: status.last_activity,
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
            match host.sessions().await {
                Ok(list) => fanout.publish(Frame::List {
                    sessions: list.sessions,
                }),
                Err(err) => tracing::warn!("failed to build the session list: {err}"),
            }
        }
    });
}
