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
//! must never be able to fail a turn. Lock order is log, then session
//! status, then the subscriber registry; the latter two are std mutexes so
//! they cannot be held across an await.

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
use aj_agent::{BoxError, SubAgentRegistry, TaskRegistry};
use aj_conf::{AgentEnv, Config};
use aj_models::ThinkingConfig;
use aj_models::auth::AuthStorage;
use aj_models::registry::ModelInfo;
use aj_models::types::{Speed, UserContent};
use aj_session::{
    AppendHandoff, ConversationLog, ConversationPersistence, EntryId, SessionLock, project_suffix,
};
use aj_wire::{
    AgentQueue, Cursor, DurableEvent, Frame, Hello, PROTOCOL_VERSION, QueueCounts, QueueState,
    SessionList, SessionSummary, SessionTree, TaskSummary, TaskTable, TreeSegment,
};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::host::driver::Driver;
use crate::host::live::{LiveSession, Request, SessionStatus, settings_of};
use crate::session::{SessionCore, SessionEntry, SessionSpec, SubAgentOverrides};
use crate::session_setup::{RestoreContext, RunConfigSnapshot};
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
/// onto the status vocabulary of spec 6.1 (404 for the unknown cases, 409
/// for a conflict or a lock, 400 for an invalid request).
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

/// A withdrawal or a clear of an agent's pending message.
///
/// The queues hold at most one message per agent (the "one message, one
/// kind" invariant), so a withdrawal names the agent rather than a slot.
pub enum QueueOp {
    Remove { agent: AgentId },
    Clear { agent: AgentId },
}

/// Which settings axis a change moves, and to what.
pub enum SettingsAxis {
    Model(ModelInfo),
    Thinking(Option<ThinkingConfig>),
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
pub struct AttachRequest {
    pub session: String,
    /// The last durable position the client committed. A cursor from
    /// another epoch, or beyond the session's high-water mark, means a full
    /// backfill (spec 6.5).
    pub cursor: Option<Cursor>,
}

/// Direct handles into one live session, for a client attached in process.
///
/// Spec section 5 sanctions this: the local frontend attaches "through
/// direct handles and channels, not through HTTP". It reads draw-time state
/// off these (the pending-message box re-reads the live queues, the footer
/// the run config) while every mutation still goes through
/// [`SessionHost::command`], which is what keeps the local and the remote
/// path one path. Nothing outside this process can have them, so no
/// protocol rule may come to depend on them.
pub struct LocalHandles {
    pub session_id: String,
    pub queues: MessageQueues,
    pub task_registry: TaskRegistry,
    pub registry: SubAgentRegistry,
    pub log: Arc<TokioMutex<ConversationLog>>,
    pub run_config: Arc<StdMutex<RunConfigSnapshot>>,
    pub sub_overrides: Arc<StdMutex<HashMap<usize, SubAgentOverrides>>>,
    pub env: AgentEnv,
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
        // documented teardown; this only bounds the damage, loudly.
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
        self.alive()?;
        let mut sessions = self.inner.sessions.lock().await;
        let session = self
            .materialize(&mut sessions, None)
            .await?
            .id()
            .to_string();
        Ok(session)
    }

    /// Open a stream and serve an attach block for every named session.
    ///
    /// Per session, in order: the subscriber is registered, the log is
    /// snapshotted under its lock, the durable suffix is projected outside
    /// it, and the block (`state`, backfill, `caught_up`, then the
    /// conclusion sweep) is written in one critical section. Registering
    /// before snapshotting is what makes the pair atomic with respect to
    /// the session's event flow: a durable frame published in between is
    /// either already in the backfill (and filtered against the boundary)
    /// or above it (and delivered).
    pub async fn attach(&self, requests: &[AttachRequest]) -> Result<Attachment, HostError> {
        self.alive()?;
        // Materialize everything up front: a failure must not leave a
        // half-served stream behind.
        let mut live = Vec::with_capacity(requests.len());
        for request in requests {
            live.push((request, self.live(&request.session).await?));
        }
        let names: Vec<String> = requests
            .iter()
            .map(|request| request.session.clone())
            .collect();
        let (id, frames) = self.inner.shared.fanout.register(&names);
        let attachment = Attachment::new(id, frames, Arc::clone(&self.inner.shared.fanout));
        for (request, session) in live {
            self.serve_block(id, request, &session).await;
        }
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
            summaries.insert(
                metadata.session_id.clone(),
                SessionSummary {
                    id: metadata.session_id,
                    live: false,
                    working: false,
                    queued: QueueCounts::default(),
                    tasks: 0,
                    last_seq: 0,
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
    pub async fn tasks(&self, session: &str) -> Result<TaskTable, HostError> {
        let live = self.live(session).await?;
        let now_wall = Utc::now();
        let now = Instant::now();
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
                started_at: wall_clock(now_wall, now, task.started_at),
            })
            .collect();
        Ok(TaskTable { tasks })
    }

    /// The session's pending steering and follow-up messages.
    pub async fn queue(&self, session: &str) -> Result<QueueState, HostError> {
        let live = self.live(session).await?;
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
        let live = self.materialize(&mut sessions, Some(session)).await?;
        drop(sessions);
        Ok(live)
    }

    /// Take a session's advisory lock, refusing when another writer holds
    /// it.
    fn acquire(&self, id: &str) -> Result<SessionLock, HostError> {
        SessionLock::try_acquire(&self.inner.persistence, id)
            .map_err(|err| HostError::Internal(Box::new(err)))?
            .ok_or_else(|| HostError::Locked(id.to_string()))
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
        // handful of sessions; if it starts to hurt, the build moves to the
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
            self.inner.base_run_config.clone(),
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
    ) {
        // The snapshot and the epoch are read under the log lock, because a
        // head switch moves both under it: reading them separately could
        // pair the old projection with the new epoch.
        let (snapshot, epoch, working_seen, settings_seen, finished_subs) = {
            let log = session.core.log.lock().await;
            let status = session.status();
            (
                log.snapshot(),
                status.epoch.clone(),
                status.working,
                status.settings.clone(),
                status.finished_subs.clone(),
            )
        };
        let boundary = snapshot.last_seq();
        let cursor = request
            .cursor
            .as_ref()
            .filter(|cursor| cursor.epoch == epoch)
            .map(|cursor| cursor.seq);
        // A run the log names and the host has not seen finish is live, so
        // its bracket stays open. Deriving it this way rather than tracking
        // the live set keeps the one unavoidable lag (a spawn root reaches
        // disk before the host consumes the run's `AgentStart`) on the safe
        // side: the worst case is a bracket left open a moment too long,
        // which the live `SubAgentEnd` closes, instead of a fabricated
        // conclusion for a running sub-agent.
        let live_subs: std::collections::BTreeSet<usize> = snapshot
            .sub_agent_ids()
            .difference(&finished_subs)
            .copied()
            .collect();
        // Projected outside the log lock: a full backfill walks the whole
        // log, and holding the lock would stall the session's next append
        // for the length of it.
        let backfill = project_suffix(&snapshot, cursor, &live_subs);

        let mut block = Vec::with_capacity(backfill.events.len() + 2);
        block.push(Frame::State {
            session: session.id().to_string(),
            epoch: epoch.clone(),
            working: working_seen,
            settings: settings_seen.clone(),
            last_seq: boundary,
        });
        for tagged in backfill.events {
            block.push(Frame::Event {
                session: session.id().to_string(),
                epoch: epoch.clone(),
                durability: tagged.entry.map(|entry| DurableEvent {
                    seq: entry.seq,
                    entry_id: entry.id,
                }),
                event: tagged.event.into(),
            });
        }
        block.push(Frame::CaughtUp {
            session: session.id().to_string(),
            epoch: epoch.clone(),
            last_seq: boundary,
        });
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
            block.push(Frame::Event {
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
            });
        }
        self.inner
            .shared
            .fanout
            .deliver_block(id, session.id(), block, boundary);

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
    }
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

/// Project a monotonic `Instant` onto wall clock, using the pair of clocks
/// read at the same moment. Exact enough for a task table: the two reads
/// are microseconds apart.
fn wall_clock(now_wall: DateTime<Utc>, now: Instant, at: Instant) -> DateTime<Utc> {
    let elapsed = now.saturating_duration_since(at);
    now_wall - chrono::Duration::from_std(elapsed).unwrap_or_default()
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
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(HostError::Internal(Box::new(err))),
    }
    let id = format!("{:032x}", rand::random::<u128>());
    std::fs::create_dir_all(sessions_dir).map_err(|err| HostError::Internal(Box::new(err)))?;
    std::fs::write(&path, format!("{id}\n")).map_err(|err| HostError::Internal(Box::new(err)))?;
    Ok(id)
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
