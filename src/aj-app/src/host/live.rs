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
use std::sync::{Mutex as StdMutex, MutexGuard};

use aj_agent::events::{AgentId, AgentSettings};
use aj_session::AppendHandoff;
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
}

impl SessionStatus {
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

    /// Whether the session has anything queued for `agent`.
    pub(crate) fn has_queued(&self, agent: AgentId) -> bool {
        self.core.message_queues.has_pending(agent)
    }
}
