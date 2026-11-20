//! One live session: the composed [`SessionCore`], the state the host
//! publishes about it, and the channel its driver takes requests on.
//!
//! **Lock ordering.** The session log's async mutex is the outer
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
use aj_session::SessionMetadata;
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
    GoalInference {
        cancel: tokio_util::sync::CancellationToken,
        reply: oneshot::Sender<Result<u64, aj_agent::goal::GoalError>>,
    },
    GoalTool {
        action: aj_agent::goal::GoalAction,
        cancel: tokio_util::sync::CancellationToken,
        revision: u64,
        reply: oneshot::Sender<Result<Option<aj_agent::goal::Goal>, aj_agent::goal::GoalError>>,
    },
    Command {
        command: Command,
        reply: oneshot::Sender<Result<CommandOutcome, HostError>>,
    },
    /// Stop the session as a host that died stops it: close it for
    /// writing, abort its turns, let its task drivers go. The driver
    /// returns afterwards, which is what releases the session lock.
    Shutdown,
    /// Wind down and return, but only if the session is [`releasable`] and has
    /// a log on disk to be handed back to.
    ///
    /// The driver answers this rather than the caller deciding, because it
    /// answers at its own position in the request queue: a command enqueued
    /// ahead of this has already taken effect, so the session it judges is the
    /// one the command left behind.
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
    /// The activity stamp the session's cold row carries: when this
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
    /// switch. Never persisted, so a host restart invalidates every cursor.
    pub(crate) epoch: String,
    /// The highest durable position this host has **published** a frame for.
    ///
    /// Not the log's own `last_seq`: the driver advances this as it publishes,
    /// so it lags an append whose event has not reached the driver's event arm
    /// yet. The two marks share a name and surface differently: `caught_up`
    /// carries the log's mark (read under its lock, so it covers everything on
    /// disk), while `list` frames carry this one. A client is never harmed by
    /// the lag, since a `list` position is glyph data and never a cursor.
    pub(crate) last_seq: u64,
    /// Whether the **main** agent has a turn in flight. Says nothing about
    /// sub-agents, whose liveness travels through lifecycle events.
    pub(crate) working: bool,
    /// Whether the process that ran Main's last turn ended before the turn
    /// did, cached off the session's stored turn state.
    pub(crate) turn_interrupted: bool,
    pub(crate) turn_outcome: Option<aj_wire::TurnOutcome>,
    /// Session-wide activity for directory rows, independent of Main's turn.
    pub(crate) session_working: bool,
    /// The settings the next main turn runs against, cached off the run
    /// config so a `state` frame needs no lock of its own.
    pub(crate) settings: AgentSettings,
    /// The Oracle settings staged for the next main turn, cached beside main.
    pub(crate) oracle_settings: Option<AgentSettings>,
    pub(crate) goal: Option<aj_agent::goal::Goal>,
    pub(crate) last_activity: DateTime<Utc>,
    /// The session's label, read from its sidecar when the session
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

    /// The `state` frame this status describes.
    fn frame(&self, session: &str) -> Frame {
        Frame::State {
            session: session.to_string(),
            epoch: self.epoch.clone(),
            opens_block: false,
            working: self.working,
            settings: self.settings.clone(),
            oracle_settings: self.oracle_settings.clone(),
            goal: self.goal.clone(),
            turn_outcome: self.turn_outcome,
            turn_interrupted: self.turn_interrupted,
            credential_warning: None,
        }
    }
}

/// Read the main and Oracle settings from the run config, the authority for
/// the next main turn rather than the agent's last-turn copy.
pub(crate) fn settings_of(
    run_config: &StdMutex<RunConfigSnapshot>,
) -> (AgentSettings, Option<AgentSettings>) {
    let run = run_config.lock().expect("run config mutex poisoned");
    (run.main.settings(), Some(run.oracle.settings()))
}

/// A session the host holds live.
pub(crate) struct LiveSession {
    pub(crate) core: SessionCore,
    status: StdMutex<SessionStatus>,
    requests: UnboundedSender<Request>,
    /// Set before shutdown is queued, so work becoming ready behind an
    /// in-flight command cannot start another turn while the request waits.
    draining: AtomicBool,
}

impl LiveSession {
    pub(crate) fn new(
        core: SessionCore,
        status: SessionStatus,
        requests: UnboundedSender<Request>,
    ) -> Self {
        Self {
            core,
            status: StdMutex::new(status),
            requests,
            draining: AtomicBool::new(false),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.core.session_id
    }

    /// The sub-agents with a turn this process started and has not ended,
    /// and the ones whose parent's tool call still waits for them. A
    /// backfill leaves the boxes of both open, and tells clients the first
    /// are running.
    ///
    /// The turn state is stored with the entry that starts a run, so a
    /// caller that reads this under the log lock, with its snapshot, never
    /// sees a run's spawn record without its turn. A box is then never
    /// concluded for a sub-agent that is still running.
    pub(crate) fn live_subs(&self) -> (BTreeSet<usize>, BTreeSet<usize>) {
        let running = self.core.turn.working_subs().into_iter().collect();
        let registry = &self.core.registry;
        let assigned = registry.ids().into_iter();
        let assigned = assigned.filter(|n| registry.assignment(*n).is_some());
        (running, assigned.collect())
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
    /// lossy coalescing is newest-wins by queue position: a frame
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

    /// Whether the session has a pending input for `agent`.
    pub(crate) fn has_pending(&self, agent: AgentId) -> bool {
        self.core.message_queues.has_pending(agent)
    }
}

/// Whether `session` is releasable: nothing running, no pending input, no
/// undelivered task notification, nobody attached.
///
/// Pending inputs and task notifications hold a session live: releasing it would
/// drop the copy in memory, and the saved one is restored only when the
/// session is next opened. Attachment is the retention signal, so a
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
        if status.working {
            return false;
        }
    }
    // A sub-agent's turn: nested in a main turn, driven on its own, or a
    // background run.
    if !session.core.turn.working_subs().is_empty() {
        return false;
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
