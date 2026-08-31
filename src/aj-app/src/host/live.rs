//! One live session: the composed [`SessionCore`], the state the host
//! publishes about it, and the channel its driver takes requests on.
//!
//! **Lock ordering.** The conversation log's async mutex is the outer
//! lock, [`LiveSession::status`] the inner one. Anything that needs both
//! takes the log first (an attach, a head switch), and nothing takes the
//! log lock while holding `status`. `status` is a std mutex precisely so
//! it cannot be held across an await, which is what keeps that rule
//! mechanical rather than a discipline.
//!
//! `status` is also what serializes the session's `state` publishers
//! against each other, see [`LiveSession::publish_state`].

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};
use std::time::Instant;

use aj_agent::events::{AgentId, AgentSettings};
use aj_session::{AppendHandoff, SessionMetadata};
use aj_wire::Frame;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::host::fanout::Fanout;
use crate::host::{Command, CommandOutcome, HostError};
use crate::session::SessionCore;
use crate::session_setup::RunConfigSnapshot;

/// What a session's driver accepts. Every mutation goes through here, so
/// the driver is the single writer of the turn set, the lifecycle, and the
/// published status, and no command can race a turn's own bookkeeping.
pub(crate) enum Request {
    Command {
        command: Command,
        reply: oneshot::Sender<Result<CommandOutcome, HostError>>,
    },
    /// Wind the session down: cancel its turns through the graceful path,
    /// quiesce background tasks, flush the log. The driver returns
    /// afterwards, which is what releases the session lock.
    Shutdown,
    /// Wind down and return, but only if the session is [`releasable`] and has
    /// a log on disk to be handed back to.
    ///
    /// The driver answers this rather than the caller deciding, because it
    /// answers at its own position in the request queue: a command enqueued
    /// ahead of this has already taken effect, so the session it judges is the
    /// one the command left behind (spec section 5's serialization rule).
    Release {
        reply: oneshot::Sender<ReleaseOutcome>,
    },
}

/// What a driver answers a [`Request::Release`] with.
pub(crate) enum ReleaseOutcome {
    /// The session was releasable. The driver has wound down and is returning,
    /// which is what releases the session's advisory lock.
    Released { row: ReleasedRow },
    /// The session was not releasable. It stays live and keeps its lock.
    Declined,
}

/// What a released session leaves for the host's directory to report.
///
/// Read after the release flush and while the driver still holds the
/// session's lock, so it describes the file state the release actually left
/// and no rival writer can have moved it in between. A release the driver
/// cannot produce one for does not happen at all: the host would have no row
/// to serve the session with (see [`ReleaseOutcome`]).
pub(crate) struct ReleasedRow {
    /// The file the release left behind, which is the fingerprint the
    /// directory caches its format verdict under.
    pub(crate) file: SessionMetadata,
    /// The activity stamp the session's cold row carries (spec 6.8): when this
    /// driver last saw the session do something.
    ///
    /// Not [`Self::file`]'s modification time, which answers a different
    /// question, when the bytes landed. The two disagree in both directions.
    /// An append moves the mtime a moment before the driver observes it, and
    /// the release flush moves it a whole idle grace after the work that
    /// buffered the entry. The host's own answer is the one about the session.
    pub(crate) last_activity: DateTime<Utc>,
    /// The session's label as the driver held it, so the cold row keeps it
    /// without waiting for an enumeration. A tag set while the session was
    /// live is newer than anything the directory cache read.
    pub(crate) tag: Option<String>,
    /// The session's archived bit as the driver held it, for the same reason
    /// and on the same terms as [`Self::tag`].
    pub(crate) archived: bool,
}

/// The per-session state the host publishes, readable without awaiting.
///
/// This is exactly what a `state` frame carries plus what an attach and
/// the session list need. The driver is its single writer, everyone else
/// reads. Keeping it here rather than asking the driver is what lets an
/// attach and a list build run without a round trip through a task that
/// may be mid-turn.
pub(crate) struct SessionStatus {
    /// Opaque token minted per materialization and replaced on a head
    /// switch. Never persisted (spec 6.5).
    pub(crate) epoch: String,
    /// The highest durable position this host has **published** a frame for.
    ///
    /// Not the log's own `last_seq`: the driver advances this as it publishes,
    /// so it lags an append whose event has not reached the driver's event arm
    /// yet. The two marks share a name and surface differently: `caught_up`
    /// and an attach block's `state` frame carry the log's mark (read under
    /// its lock, so it covers everything on disk), while `list` frames and an
    /// on-change `state` frame carry this one. A client is never harmed by the
    /// lag, since a `list` position is glyph data and never a cursor
    /// (spec 6.5).
    pub(crate) last_seq: u64,
    /// Whether the **main** agent has a turn in flight (spec 6.3).
    pub(crate) working: bool,
    /// The settings the next main turn runs against, cached off the run
    /// config so a `state` frame needs no lock of its own.
    pub(crate) settings: AgentSettings,
    /// The sub-agents the host has observed going idle, plus every one the
    /// log already named when the session was materialized (nothing runs at
    /// that point, so they are all finished).
    ///
    /// Monotone: it only ever grows, because a sub-agent id is minted once
    /// per session. A backfill's `live_subs` is derived from it as "in the
    /// log and not in here", plus [`Self::driven_subs`], which is what puts
    /// the lag in the safe direction. The alternative, tracking the live set
    /// directly, lags the log: a spawn root reaches disk several bus emits
    /// before the host consumes the `AgentStart` that would record the run as
    /// live, and a backfill served in that window would fabricate a
    /// conclusion for a sub-agent that is still running (spec 6.5).
    pub(crate) finished_subs: BTreeSet<usize>,
    /// The sub-agents the host is driving a turn for.
    ///
    /// A continuation prompt re-opens the run of a sub-agent that already
    /// finished, and `finished_subs` is monotone, so this is what says "live
    /// again". It is recorded before the turn's task exists, so no append of
    /// the new run can land while the run still reads as finished.
    pub(crate) driven_subs: BTreeSet<usize>,
    pub(crate) last_activity: DateTime<Utc>,
    /// The session's label (spec 6.8), read from its sidecar when the session
    /// was materialized and kept current by the tag command.
    ///
    /// Held here so a directory refresh, which runs on a coalescing tick, can
    /// answer for a live session without going near the filesystem.
    pub(crate) tag: Option<String>,
    /// Whether the user has put the session away, read from its sidecar at
    /// materialization and kept current by the archive command.
    ///
    /// Held here for the same reason [`Self::tag`] is. Display metadata with
    /// no lifecycle meaning: nothing in this module consults it, and no turn,
    /// release or head switch changes it.
    pub(crate) archived: bool,
    /// The same instant on the monotonic clock, for the host's own release
    /// timer.
    ///
    /// Two clocks because they answer different questions. `last_activity` is
    /// what a client renders, so it has to be wall-clock. The release timer
    /// measures a duration this process cares about, and a wall-clock step
    /// backwards would hold every idle session, and its lock, for the length of
    /// the step.
    pub(crate) last_work: Instant,
}

impl SessionStatus {
    /// Record that the session just did something durable.
    ///
    /// The stamp only ever moves forward. A wall clock can step backwards, and
    /// this one is published: a stamp older than one a client was already
    /// served reads as "nothing new here" for the length of the step, which
    /// for the unseen-output glyph means real output goes unannounced (spec
    /// 6.8).
    pub(crate) fn note_activity(&mut self) {
        self.last_activity = Utc::now().max(self.last_activity);
        self.last_work = Instant::now();
    }

    /// The `state` frame this status describes (spec 6.3).
    fn frame(&self, session: &str) -> Frame {
        Frame::State {
            session: session.to_string(),
            epoch: self.epoch.clone(),
            working: self.working,
            settings: self.settings.clone(),
            last_seq: self.last_seq,
        }
    }
}

/// Read the settings identity a `state` frame reports off a session's run
/// config. The run config is what the next main turn is stamped from, so it
/// is the authority for "the active model", not the agent (whose copy lags
/// by one turn).
pub(crate) fn settings_of(run_config: &StdMutex<RunConfigSnapshot>) -> AgentSettings {
    run_config
        .lock()
        .expect("run config mutex poisoned")
        .settings()
}

/// A session the host holds live.
pub(crate) struct LiveSession {
    pub(crate) core: SessionCore,
    /// The compaction append handoff shared with the session's event
    /// forwarder, so a compaction's `CompactionEnd` reaches the fan-out
    /// tagged with the checkpoint entry it belongs to.
    pub(crate) handoff: AppendHandoff,
    status: StdMutex<SessionStatus>,
    requests: UnboundedSender<Request>,
    /// Set before shutdown is queued, so work becoming ready behind an
    /// in-flight command cannot start another turn while the request waits.
    draining: AtomicBool,
}

impl LiveSession {
    pub(crate) fn new(
        core: SessionCore,
        handoff: AppendHandoff,
        status: SessionStatus,
        requests: UnboundedSender<Request>,
    ) -> Self {
        Self {
            core,
            handoff,
            status: StdMutex::new(status),
            requests,
            draining: AtomicBool::new(false),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.core.session_id
    }

    /// The published status. Never hold this across an await, and never
    /// take the log lock while holding it (see the module docs).
    pub(crate) fn status(&self) -> MutexGuard<'_, SessionStatus> {
        self.status.lock().expect("session status mutex poisoned")
    }

    /// Apply `update` to the published status and publish the `state` frame it
    /// describes, unless `update` reports nothing changed.
    ///
    /// The status lock spans the update, the decision and the publish because
    /// lossy coalescing is newest-wins by queue position (spec 6.9): a frame
    /// built from an older snapshot but enqueued later drops the queued newer
    /// one and leaves every subscriber holding the stale snapshot. Holding
    /// `status` is what serializes the session's publishers, its driver and an
    /// attach's post-block refresh, so the frame enqueued last is always the
    /// one built from the newest status.
    ///
    /// The enqueue is part of what the lock protects. Building the frame under
    /// the lock and publishing after releasing it leaves the same window open,
    /// only narrower: two publishers can still reach `Fanout::publish` out of
    /// status order.
    ///
    /// NOTE: nothing observes that ordering from outside. The window is a few
    /// instructions wide and the only publisher competing with an attach's
    /// refresh is the session's own driver, so no test can be made to fail on
    /// it reliably. This rule is the guard.
    ///
    /// `update` runs under the status lock, so it must take no other lock that
    /// anything holds while reading the status (the log's above all, see the
    /// module docs).
    pub(crate) fn publish_state(
        &self,
        fanout: &Fanout,
        update: impl FnOnce(&mut SessionStatus) -> bool,
    ) {
        let mut status = self.status();
        if update(&mut status) {
            fanout.publish(status.frame(self.id()));
        }
    }

    /// Hand `request` to the session's driver. `false` when the driver has
    /// already stopped.
    pub(crate) fn send(&self, request: Request) -> bool {
        self.requests.send(request).is_ok()
    }

    /// Enter terminal drain mode before asking the driver to wind down.
    pub(crate) fn request_shutdown(&self) -> bool {
        self.start_draining();
        self.send(Request::Shutdown)
    }

    /// Suppress every wake path from this point through driver teardown.
    pub(crate) fn start_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Whether the driver has returned without the host asking it to, which
    /// only a fused log (or a panic) leaves behind. Such an entry serves
    /// nothing and is reaped rather than released.
    pub(crate) fn driver_gone(&self) -> bool {
        self.requests.is_closed()
    }

    /// Whether the session has anything queued for `agent`.
    pub(crate) fn has_queued(&self, agent: AgentId) -> bool {
        self.core.message_queues.has_pending(agent)
    }
}

/// Whether `session` is releasable: nothing running, nothing queued, no
/// undelivered task notice, nobody attached (spec section 5).
///
/// Queued messages and task notices hold a session live because both live in
/// memory only: releasing a session holding one would discard something the
/// user or a finished task handed us. Attachment is the retention signal, so a
/// client that keeps a session attached keeps its lock, deliberately.
///
/// Read off the published status and the session's own registries, so the
/// host's sweeper and the session's driver can both ask it. The driver's
/// answer is the one that decides, not because it knows more but because of
/// when it asks (see [`Request::Release`]). The driver also adds the one
/// condition this cannot see, that the log has a file to come back to.
pub(crate) fn releasable(session: &LiveSession, fanout: &Fanout) -> bool {
    {
        let status = session.status();
        // `working` covers the main agent, and a sub-agent's turn is either
        // nested in a main turn or driven on its own, which `driven_subs`
        // records. A background sub-agent shows up in the task registry below.
        if status.working || !status.driven_subs.is_empty() {
            return false;
        }
    }
    if session.core.message_queues.pending_counts() != (0, 0)
        || session.core.task_registry.has_any_notices()
    {
        return false;
    }
    let running = session
        .core
        .task_registry
        .snapshot()
        .into_iter()
        .any(|task| task.status == aj_agent::tool::TaskStatus::Running);
    !running && !fanout.attached(session.id())
}
