//! The per-session driver: one task per live session, owning what the
//! interactive drive loop owns today.
//!
//! It is the session's single writer. The turn set, the agent lifecycle
//! and the published status are plain fields here rather than shared
//! state, and every mutation arrives as a [`Request`], so a command can
//! never race a turn's own bookkeeping. Reads that need none of that (an
//! attach, a session list) go straight to [`LiveSession::status`] instead
//! of round-tripping through this task, which matters because the task can
//! be busy for the length of a head switch.
//!
//! It is also the session's single publisher. Frames leave here in one
//! order, which is what makes "live durable frames reach a stream in
//! strictly increasing seq order" true by construction. Every append to a
//! hosted log goes through the session's journal, which sends what the
//! append publishes under the lock that appended. This task learns of an
//! entry only from that stream, in [`Driver::on_committed`], so it
//! publishes positions in the order they were committed. A write this task
//! makes itself (a settings change, a goal record) takes the same path.
//!
//! **Request ordering.** Requests are answered one at a time in arrival
//! order: the loop awaits a command's completion before taking the next.
//! A caller that got its acceptance therefore knows every command accepted
//! before it has already taken effect, which is what lets a client sequence
//! gestures (a settings change and then the prompt that should run under it)
//! without a barrier of its own. The cost is that a slow command (a head
//! switch) delays every other client's, which is why refusals are cheap and
//! independent reads bypass this task entirely.

use std::sync::Arc;

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::queue::QueueEdit;
use aj_agent::tool::{TaskId, TaskStatus};
use aj_models::types::UserContent;
use aj_session::{
    Committed, PersistenceFailure, TaggedEvent, ThreadFilter, TurnState, turn_unanswered,
};
use aj_wire::{DurableEvent, Frame};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

use crate::compaction::CompactionFailure;
use crate::host::live::{self, LiveSession, ReleaseOutcome, ReleasedRow, Request, settings_of};
use crate::host::{
    Command, CommandOutcome, HeadTarget, HostError, HostShared, PERSISTENCE_FAILED_CODE, QueueOp,
    SettingsAxis, SettingsChange, mint_epoch, persistence_failure_message,
};
use crate::session::AgentLifecycle;
use crate::settings::{ConfirmOutcome, PersistAction};
use crate::turn::{Joined, TurnStart, Turns, running_work_counts};

mod goal;

#[cfg(test)]
mod compaction_tests;
pub(crate) use goal::context as goal_context;

/// Resolve a head target against `log` to the entry the head moves to.
///
/// A `before` target answers its entry's parent, which is what makes a branch
/// replace the message it was taken from rather than continue after it. An
/// entry the log does not hold is a 404, and one with no parent is refused:
/// branching before a root would leave the session with no history at all,
/// and no transcript gesture can legitimately ask for it.
pub(super) fn resolve_head_target(
    log: &aj_session::SessionLog,
    target: &HeadTarget,
) -> Result<String, HostError> {
    let known = |id: &String| Ok::<_, HostError>(log.find(id).map_err(internal)?.is_some());
    let entry = match target {
        HeadTarget::Entry(entry) => Ok(entry.clone()),
        // The entry may be one a client paged back to, which the log does
        // not hold in memory.
        HeadTarget::Before(entry) => match log.find(entry).map_err(internal)? {
            Some(found) => match found.parent_id {
                Some(parent) if known(&parent)? => Ok(parent),
                // The client named a real entry, so a parent id the log does
                // not hold is a torn log rather than a bad request.
                Some(parent) => Err(HostError::Internal(
                    format!("entry {entry} names parent {parent}, which the log does not hold")
                        .into(),
                )),
                None => Err(HostError::Invalid(format!(
                    "entry {entry} starts the session, so there is nothing before it"
                ))),
            },
            None => Err(HostError::UnknownEntry(entry.clone())),
        },
    }?;
    let named = known(&entry)?;
    log.validate_head(&entry).map_err(|err| match err {
        aj_session::SessionError::InvalidHead(_) if named => {
            // A before request names the child, not the rejected parent.
            HostError::Invalid(match target {
                HeadTarget::Entry(entry) => {
                    format!("entry {entry} is not on the main thread and cannot be a head")
                }
                HeadTarget::Before(entry) => format!(
                    "cannot branch before entry {entry}: its parent is not on the main thread"
                ),
            })
        }
        aj_session::SessionError::InvalidHead(_) => {
            HostError::UnknownEntry(target.named().to_string())
        }
        other => internal(other),
    })?;
    Ok(entry)
}

pub(crate) struct Driver {
    session: Arc<LiveSession>,
    shared: Arc<HostShared>,
    turns: Turns,
    lifecycle: AgentLifecycle,
    events: UnboundedReceiver<Committed>,
    requests: UnboundedReceiver<Request>,
    /// Fired once by the session log when a write fails and fuses it
    /// (see `SessionLog::install_failure_signal`). The log reports from
    /// the fuse itself, so a failure inside a detached task's append reaches
    /// this task the same way a foreground turn's does.
    persistence_failure: oneshot::Receiver<PersistenceFailure>,
    /// Set once the failure above has been consumed, so the receiver is never
    /// polled again and a turn that failed on that same write does not report
    /// the storage error a second time.
    persistence_failed: bool,
    /// Failed compaction leaves Main idle for inspection. Retain queued input
    /// and task notifications until the user starts work again, rather than waking
    /// into the same oversized context. This is not a session-persistent pause.
    compaction_failed: bool,
    /// Explicit yield. Only synthetic goal continuation is held: queued input
    /// and task notifications retain their ordinary wake paths.
    waiting_for_input: bool,
    /// User goal input not yet admitted by Main. A yield decided against
    /// the earlier objective must not swallow that request to continue.
    goal_resume_pending: bool,
    goal: goal::GoalRun,
    /// The agents whose turn the open found interrupted and that still
    /// wait to be resumed (see [`crate::recovery::RESUME`]). An agent
    /// leaves when a turn starts on it.
    interrupted: Vec<AgentId>,
    /// Set once this materialization was ended from inside a request (see
    /// [`Self::stop_with`]). The driver loop returns when it sees it.
    stopped: bool,
    /// Zero means no goal history. User mutations advance this revision, which
    /// admission returns to fence tools from an older objective or stop request.
    goal_revision: u64,
}

impl Driver {
    pub(crate) fn new(
        session: Arc<LiveSession>,
        shared: Arc<HostShared>,
        events: UnboundedReceiver<Committed>,
        requests: UnboundedReceiver<Request>,
        persistence_failure: oneshot::Receiver<PersistenceFailure>,
        goal_revision: u64,
    ) -> Self {
        let turns = Turns::new();
        let goal = goal::GoalRun::new(session.status().goal.clone());
        let interrupted = session.core.interrupted.clone();
        Self {
            session,
            shared,
            turns,
            lifecycle: AgentLifecycle::default(),
            events,
            requests,
            persistence_failure,
            persistence_failed: false,
            compaction_failed: false,
            waiting_for_input: false,
            goal_resume_pending: false,
            goal,
            interrupted,
            stopped: false,
            goal_revision,
        }
    }

    pub(crate) async fn run(mut self) {
        self.apply_resume_policy().await;
        loop {
            tokio::select! {
                biased;

                // A fused log first: nothing this task could do for a request
                // is safe once the log refuses to append, and the client owes
                // the storage error before any refusal of its own.
                failure = &mut self.persistence_failure, if !self.persistence_failed => {
                    self.persistence_failed = true;
                    if let Ok(failure) = failure {
                        self.stop_for_persistence(failure).await;
                        return;
                    }
                }

                // Requests first: a command's refusal has to reflect the
                // state as of now, and the event arm can be arbitrarily
                // busy during a streaming turn.
                request = self.requests.recv() => {
                    // The host asked us to stop, or dropped the session.
                    // Nothing already committed is folded first: the store
                    // has it, and the next open reads it from there.
                    if matches!(&request, Some(Request::Shutdown) | None) {
                        self.wind_down().await;
                        return;
                    }
                    // A client that keeps commands in flight would
                    // otherwise starve the two arms below for as long as it
                    // keeps going: no frame would reach any client and no
                    // turn would be reaped. Catching up here also makes the
                    // refusal below reflect the events already in the
                    // channel rather than the state before them.
                    self.catch_up();
                    match request {
                        Some(Request::GoalInference { cancel, reply }) => {
                            let outcome = if cancel.is_cancelled() || reply.is_closed() {
                                Err(aj_agent::goal::GoalError::Conflict("The originating turn was cancelled.".into()))
                            } else {
                                Ok(self.admit_goal_inference())
                            };
                            let _ = reply.send(outcome);
                        }
                        Some(Request::GoalTool { action, cancel, revision, reply }) => {
                            let outcome = if cancel.is_cancelled() || reply.is_closed() {
                                Err(aj_agent::goal::GoalError::Conflict("The originating turn was cancelled.".into()))
                            } else if !matches!(action, aj_agent::goal::GoalAction::Get)
                                && revision != self.goal_revision
                            {
                                Err(aj_agent::goal::GoalError::Conflict("The user changed the goal after this inference started. Read the current goal context before updating it.".into()))
                            } else {
                                self.goal_action(action, true).await
                            };
                            let _ = reply.send(outcome);
                        }
                        Some(Request::Command { command, reply }) => {
                            let outcome = self.command(command).await;
                            let _ = reply.send(outcome);
                            if self.stopped {
                                return;
                            }
                        }
                        // Judged here, after the catch-up above, so a command
                        // or an event that arrived before this request has
                        // already moved the state this reads.
                        Some(Request::Release { reply }) => {
                            let row = if live::releasable(&self.session, &self.shared.fanout) {
                                self.row_for_release()
                            } else {
                                None
                            };
                            let Some(row) = row else {
                                let _ = reply.send(ReleaseOutcome::Declined);
                                continue;
                            };
                            self.wind_down().await;
                            let _ = reply.send(ReleaseOutcome::Released { row });
                            return;
                        }
                        Some(Request::Shutdown) | None => unreachable!("handled above"),
                    }
                },

                Some(committed) = self.events.recv() => self.on_committed(committed),

                joined = self.turns.join_next() => self.on_join(joined),
            }
            // What this pass decided about the goal is committed before the
            // next request is taken, and the goal continues only from a
            // committed state.
            if let Err(err) = self.checkpoint_goal().await {
                self.report_goal_unsaved(err).await;
            } else if self.requests.is_empty() && self.events.is_empty() {
                if let Err(err) = self.continue_goal().await {
                    self.report_goal_unsaved(err).await;
                }
            }
        }
    }

    /// Make progress on the arms the request arm's priority skips: publish
    /// everything already on the event stream, then reap the turns that have
    /// already finished.
    ///
    /// Events before joins, because a turn's own events precede its join and
    /// the frames this task emits at reap time (a swept sub's `AgentEnd`, a
    /// cancellation notice) belong after them.
    fn catch_up(&mut self) {
        self.drain_events();
        while let Some(joined) = self.turns.try_join_next() {
            self.on_join(joined);
        }
    }

    // -- events ----------------------------------------------------------

    /// Fold one item of the journal's stream. This is the only place the
    /// driver learns that the log grew.
    fn on_committed(&mut self, committed: Committed) {
        match committed {
            Committed::Event(tagged) => self.on_event(tagged),
            // Entries with nothing to show. Clients get no frame, and one
            // that attaches later gets none from the backfill either.
            Committed::Position(seq) => self.advance(seq),
        }
    }

    /// Fold one event of the session's tagged stream: update the
    /// lifecycle, advance the durable high-water mark, publish the frame,
    /// and start a wake if the event earned one.
    fn on_event(&mut self, tagged: TaggedEvent) {
        let TaggedEvent {
            entry,
            event,
            branch_settings,
        } = tagged;
        self.observe_goal_event(&event);
        // Captured off a borrow, because `publish_event` below takes the
        // event by value. The wake it decides on has to wait until after
        // `apply_lifecycle`: `Turns::spawn_wake` refuses a busy owner, and
        // this `AgentEnd` is what marks the owner idle, so waking any earlier
        // would find it busy and drop the wake. `TaskEnd` wakes
        // unconditionally so a task notification reaches the model the moment
        // its task finishes. `AgentEnd` only when something is actually
        // queued, because a sub-agent's initial run ends inside its parent's
        // turn and never reaches the join arm.
        let trigger = match &event {
            AgentEvent::TaskEnd { agent_id, .. } => Some((*agent_id, false)),
            AgentEvent::AgentEnd { agent_id, .. } => Some((*agent_id, true)),
            _ => None,
        };
        self.apply_lifecycle(&event);
        // Both calls mark the list dirty for what they change on the row.
        self.publish_event(
            entry.map(|entry| super::durable_event(entry, branch_settings)),
            event,
        );
        if !self.session.is_draining()
            && let Some((owner, conditional)) = trigger
            && self.automatic_wake_allowed(owner)
            && (!conditional
                || self.session.core.task_registry.has_notices(owner)
                || self.session.has_pending(owner))
        {
            self.wake(owner);
        }
        self.refresh_state();
    }

    fn apply_lifecycle(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart { agent_id, .. } => {
                self.lifecycle.mark_running(*agent_id);
                self.interrupted.retain(|agent| agent != agent_id);
                if *agent_id == AgentId::Main {
                    self.waiting_for_input = false;
                }
            }
            AgentEvent::AgentEnd {
                agent_id, waiting, ..
            } => {
                self.lifecycle.mark_idle(*agent_id);
                self.mark_idle(*agent_id);
                if *agent_id == AgentId::Main {
                    let resume_pending = std::mem::take(&mut self.goal_resume_pending);
                    self.waiting_for_input = *waiting && !resume_pending;
                }
            }
            AgentEvent::CompactionStart { agent_id, .. } => {
                self.lifecycle.mark_compacting(*agent_id);
            }
            AgentEvent::CompactionEnd { agent_id, .. } => {
                self.lifecycle.clear_compacting(*agent_id);
            }
            _ => {}
        }
    }

    /// Handle one completed turn: reap, conclude the sub-agent boxes the
    /// reap swept, wake on pending work, and surface the outcome.
    fn on_join(&mut self, joined: Joined) {
        // A producer can emit its final events and finish after the select
        // loop polled an empty event channel but before it polls this join.
        // Fold those events before goal accounting and reap-time publication.
        self.drain_events();
        let Joined { agent, outcome } = joined;
        if agent == AgentId::Main {
            // Main remains driven until reap, so no automatic wake can precede
            // this decision. Failure control uses the result, not its UI event.
            if matches!(&outcome, Ok(Err(TurnError::Recoverable(error))) if error.is::<CompactionFailure>())
            {
                self.compaction_failed = true;
            }
            self.goal.finish(&outcome);
        }
        for idled in self
            .turns
            .reap(&mut self.lifecycle, &self.session.core.task_registry, agent)
        {
            self.mark_idle(idled);
            // A sub the reap swept emitted no `AgentEnd` of its own, and a
            // remote client cannot conclude its box by reaching into the
            // model the way the local frontend does, so the conclusion
            // travels as the event it stands for. The reducer's `AgentEnd`
            // arm leaves an already-concluded box alone, so this is
            // idempotent against the real one arriving later.
            if let AgentId::Sub(n) = idled {
                self.publish_event(
                    None,
                    AgentEvent::AgentEnd {
                        agent_id: AgentId::Sub(n),
                        messages: Vec::new(),
                        waiting: false,
                        assignment_pending: self.session.core.registry.assignment(n).is_some(),
                    },
                );
            }
        }
        if agent == AgentId::Main {
            // A main turn's end bounds every run it spawned in the
            // foreground. One that was dropped before it reported a start
            // has its turn marked and nothing else to end it.
            let tasks = &self.session.core.task_registry;
            for n in self.session.core.turn.working_subs() {
                if !tasks.agent_task_running(n) {
                    self.mark_idle(AgentId::Sub(n));
                }
            }
        }
        let result = match outcome {
            Ok(result) => result,
            Err(err) => {
                if agent == AgentId::Main {
                    self.session.status().turn_outcome = Some(aj_wire::TurnOutcome::Failed);
                }
                // A panicked turn task is fatal for the turn but not for
                // the host: other sessions keep running, and the reap
                // above already marked its agent idle, so the session
                // stays usable. No wake, deliberately: it would re-enter
                // the code path that just panicked.
                self.publish_event(
                    None,
                    AgentEvent::Error {
                        agent_id: agent,
                        text: format!("agent task panicked: {err}"),
                    },
                );
                self.announce_waiting(agent);
                self.refresh_state();
                self.shared.fanout.mark_list_dirty();
                return;
            }
        };
        if agent == AgentId::Main {
            self.session.status().turn_outcome = Some(match &result {
                Ok(()) if self.waiting_for_input => aj_wire::TurnOutcome::Waiting,
                Ok(()) => aj_wire::TurnOutcome::Completed,
                Err(TurnError::Aborted) => aj_wire::TurnOutcome::Cancelled,
                Err(_) => aj_wire::TurnOutcome::Failed,
            });
        }
        // A turn that failed fatally is not followed by a wake. It may have
        // failed before it took its pending work, and a wake would then
        // fail the same way, again and again. The work waits for the user.
        let fatal = matches!(result, Err(TurnError::Fatal(_)));
        if !self.session.is_draining()
            && !fatal
            && self.automatic_wake_allowed(agent)
            && (self.session.core.task_registry.has_notices(agent)
                || self.session.has_pending(agent))
        {
            self.wake(agent);
        } else {
            self.announce_waiting(agent);
        }
        match result {
            Ok(()) => {}
            Err(TurnError::Aborted) => {
                // The agent already emitted the synthetic aborted
                // `MessageEnd`s, so the transcript is consistent. The notice
                // only confirms the cancel took effect.
                self.publish_event(
                    None,
                    AgentEvent::Notice {
                        agent_id: agent,
                        text: "Turn cancelled.".to_string(),
                    },
                );
            }
            // A recoverable failure already rendered in transcript order
            // from the turn's terminal `MessageEnd`, so re-reporting it
            // would float it above events still queued behind us.
            Err(TurnError::Recoverable(_)) => {}
            // The turn that tripped the fuse fails on the same error the
            // storage frame already carries.
            Err(TurnError::Fatal(_)) if self.persistence_failed => {}
            Err(TurnError::Fatal(err)) => {
                self.publish_event(
                    None,
                    AgentEvent::Error {
                        agent_id: agent,
                        text: format!("{err}"),
                    },
                );
            }
        }
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
    }

    fn automatic_wake_allowed(&self, owner: AgentId) -> bool {
        owner != AgentId::Main || !self.compaction_failed
    }

    /// Start a wake on `owner`. A no-op for a busy owner and for one with
    /// no live handle. A wake that finds nothing to deliver starts no turn
    /// and stores nothing.
    fn wake(&mut self, owner: AgentId) {
        if !self.turns.is_busy(&self.lifecycle, owner) {
            // Reactive callers check the hold. A user-resumed goal can also
            // wake pending work, and that deliberate start releases it.
            if owner == AgentId::Main {
                self.compaction_failed = false;
            }
            self.prepare_goal_turn(owner, false);
        }
        self.turns.spawn_wake(
            owner,
            &self.session.core,
            &self.lifecycle,
            &self.shared.config,
        );
        self.refresh_state();
    }

    /// Store that `agent`'s turn ended in this process, whether it
    /// finished, failed or was cancelled. Its start was stored by the
    /// journal, on the turn's `AgentStart`, so a turn that
    /// never started has nothing to end and this writes nothing. A no-op
    /// while the agent is busy: a turn the host drives ends when it is
    /// joined.
    ///
    /// A draining session is closed for writing (see [`Self::wind_down`]),
    /// so nothing is stored for it. A turn the host stops stays marked
    /// working, as after a host that died. The next open reads the main
    /// agent's as interrupted and closes a sub-agent's run.
    fn mark_idle(&self, agent: AgentId) {
        if !self.session.is_draining()
            && !self.turns.is_busy(&self.lifecycle, agent)
            && let Err(error) = self.session.core.turn.set(agent, TurnState::Idle)
        {
            tracing::warn!("could not record that {agent:?}'s turn ended: {error}");
        }
    }

    // -- publishing ------------------------------------------------------

    fn publish_event(&self, entry: Option<DurableEvent>, event: AgentEvent) {
        // A `MessageEnd` rides the entry whose id is the message's own id: the
        // wire codec pins the two together and refuses any other pairing
        // (`Frame`'s serializer validates as it writes). Nothing validates on
        // the in-process path, so a host that got this wrong would look healthy
        // locally while every remote client's stream died and reconnected in a
        // loop. The assert puts that where the frame is built.
        if let AgentEvent::MessageEnd { message, .. } = &event {
            debug_assert_eq!(
                entry.as_ref().map(|entry| entry.entry_id.as_str()),
                Some(message.id()),
                "a MessageEnd must be published with its own log entry",
            );
        }
        // A durable event moves the row's `last_seq` and `last_activity`. A
        // transient one, every streaming delta included, moves no row field,
        // so it does not wake the list publisher to rebuild an unchanged
        // directory. `working` is marked where it changes, in
        // `refresh_state`, and the remaining fields by their own commands.
        if let Some(entry) = &entry {
            self.advance(entry.seq);
        }
        let epoch = self.session.status().epoch.clone();
        self.shared.fanout.publish(Frame::Event {
            session: self.session.id().to_string(),
            epoch,
            durability: entry,
            event: event.into(),
        });
    }

    /// Move the published position to `seq`, which the journal's stream
    /// just delivered, and put it on the session's row.
    ///
    /// The stream is in commit order and is the only source of positions,
    /// so `seq` is above every position published before it.
    fn advance(&self, seq: u64) {
        let last_seq = {
            let mut status = self.session.status();
            let last_seq = status.last_seq;
            status.last_seq = seq;
            status.note_activity();
            last_seq
        };
        // Checked outside the guard: a failing assert inside it would
        // poison the status mutex, and every later `status()` would panic on
        // a lock that is only there to publish frames.
        debug_assert!(
            seq > last_seq,
            "the journal's stream delivers positions in commit order: {seq} after {last_seq}",
        );
        self.shared.fanout.mark_list_dirty();
    }

    /// Publish Main's turn state separately from the directory's session-wide
    /// activity. A yielded Main can still own running background work.
    fn refresh_state(&self) {
        let working = self.turns.is_busy(&self.lifecycle, AgentId::Main);
        let session_working = !self.turns.is_empty()
            || !self.lifecycle.running_agents().is_empty()
            || !self.lifecycle.compacting_agents().is_empty()
            || self
                .session
                .core
                .task_registry
                .snapshot()
                .iter()
                .any(|task| task.status == TaskStatus::Running);
        let turn_interrupted = self.session.core.turn.get(AgentId::Main) == TurnState::Interrupted;
        let mut changed = false;
        self.session.publish_state(&self.shared.fanout, |status| {
            changed = status.session_working != session_working;
            status.session_working = session_working;
            let main_changed =
                status.working != working || status.turn_interrupted != turn_interrupted;
            status.working = working;
            status.turn_interrupted = turn_interrupted;
            if working {
                status.turn_outcome = None;
            }
            main_changed
        });
        if changed {
            self.shared.fanout.mark_list_dirty();
        }
    }

    fn publish_state(&self) {
        self.session.publish_state(&self.shared.fanout, |_| true);
    }

    /// Change the pending message of `agent`, which has a turn running or
    /// a message waiting, and publish its queue. The change is saved before
    /// the queue takes it, so a queued message is one a restart still has
    /// and a withdrawn one is gone. A failed save refuses the command and
    /// leaves the queue as it was.
    fn edit_queue(&self, agent: AgentId, edit: QueueEdit<'_>) -> Result<Option<String>, HostError> {
        let core = &self.session.core;
        let taken = crate::session::edit_pending(&core.pending, &core.message_queues, agent, edit)
            .map_err(|error| not_saved("the queued message", error))?;
        self.publish_queue(agent);
        Ok(taken)
    }

    /// Take a prompt for an idle agent: save it as the agent's pending
    /// input, then start the wake turn that takes it. `Accepted` means the
    /// input is saved. A failed save refuses the command, and no turn
    /// starts.
    ///
    /// No `QueueUpdate` announces the input, because the turn takes it at
    /// once and a client would show it as queued for an instant. A turn
    /// that ends without having taken it announces it then (see
    /// [`Self::on_join`]).
    ///
    /// A message already pending for the agent, one restored at an open
    /// for example, takes the new content and is delivered as one message
    /// (see [`aj_agent::queue`] on "one message, one kind").
    ///
    /// The main agent's first prompt in a session that is not stored, or
    /// at a fork point, creates the session or branch in the transaction
    /// that saves it (see [`crate::session::accept_prompt`]).
    async fn start_with(
        &mut self,
        agent: AgentId,
        content: &[UserContent],
    ) -> Result<CommandOutcome, HostError> {
        let core = &self.session.core;
        if core.resolve_agent(agent).is_none() {
            // A command picks its agent up before it prompts it. Checked
            // before the save, so a refusal leaves nothing pending.
            return Err(HostError::Internal(
                format!("{} has no live agent", agent_label(agent)).into(),
            ));
        }
        let (journal, queues) = (&core.journal, &core.message_queues);
        crate::session::accept_prompt(journal, &core.pending, queues, agent, content)
            .await
            .map_err(|error| not_saved("the prompt", error))?;
        self.spawn(agent, TurnStart::Wake)
    }

    /// Tell clients of the message `agent`'s finished turn left pending,
    /// when no wake is taking it. A prompt to an idle agent is not
    /// announced when it is accepted (see [`Self::start_with`]), so this
    /// is where clients learn of one its turn did not take.
    fn announce_waiting(&self, agent: AgentId) {
        if !self.session.is_draining() && self.session.has_pending(agent) {
            self.publish_queue(agent);
        }
    }

    /// Publish `agent`'s queue snapshot. The agent only emits this after a
    /// drain, so the host emits it on the enqueue side too and a second
    /// client learns about a message it did not queue itself.
    fn publish_queue(&self, agent: AgentId) {
        let (steering, follow_up) = self.session.core.message_queues.event_messages(agent);
        self.publish_event(
            None,
            AgentEvent::QueueUpdate {
                agent_id: agent,
                steering,
                follow_up,
            },
        );
    }

    /// Fold everything already on the journal's stream, without waiting
    /// for the event arm to be polled.
    ///
    /// Never needed for the order of positions, which the stream fixes. A
    /// caller drains because something it does next has to come after what
    /// is already committed: a frame it publishes itself, a decision it
    /// takes on the folded state, or a stream it is about to close.
    fn drain_events(&mut self) {
        while let Ok(committed) = self.events.try_recv() {
            self.on_committed(committed);
        }
    }

    // -- commands --------------------------------------------------------

    async fn command(&mut self, command: Command) -> Result<CommandOutcome, HostError> {
        match command {
            Command::Goal(request) => self
                .goal_request(request)
                .await
                .map(CommandOutcome::Goal)
                .map_err(goal::host_error),
            Command::Prompt { agent, content } => {
                self.pick_up(agent).await?;
                self.prompt(agent, content).await
            }
            Command::Steer { agent, text } => {
                self.pick_up(agent).await?;
                self.steer(agent, text).await
            }
            Command::Cancel { agent } => {
                if agent == AgentId::Main {
                    self.pause_goal().await?;
                }
                self.cancel(agent)
            }
            Command::KillAgent { agent } => self.kill_agent(agent),
            Command::Queue(op) => self.queue_op(op),
            Command::Compact { instructions } => self.compact(instructions),
            Command::Settings(change) => self.settings(change).await,
            Command::Account { provider, account } => self.account(provider, account).await,
            Command::Env { key, value } => self.environment(key, value).await,
            Command::Tag { tag } => self.tag(tag).await,
            Command::Archive { archived } => self.archive(archived).await,
            Command::Head { target, changes } => self.head_switch(target, changes).await,
            Command::KillTask { task } => self.kill_task(task),
            Command::Continue => self.continue_turn().await,
        }
    }

    /// Make `agent` one this process can drive. The main agent always is.
    /// A sub-agent it holds no live agent for, one spawned by an earlier
    /// process or dropped at a head switch, is built from its stored run.
    /// One the selected branch shows no run of is refused.
    async fn pick_up(&self, agent: AgentId) -> Result<(), HostError> {
        let AgentId::Sub(n) = agent else {
            return Ok(());
        };
        let core = &self.session.core;
        if core.registry.get(n).is_some() {
            return Ok(());
        }
        let config = {
            let config = self.shared.config.lock().expect("config mutex poisoned");
            config.clone()
        };
        let mut log = core.log.lock().await;
        let built = core.rebuild_sub_agent(&mut log, n, &config, self.shared.restore.as_ref());
        match built {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(HostError::Conflict {
                reason: format!("{} has no run on this branch", agent_label(agent)),
            }),
            Err(error) => Err(HostError::Internal(error.into())),
        }
    }

    /// The submit gesture. The prompt becomes the agent's pending input
    /// either way: an idle agent is woken to take it, and behind a running
    /// turn it waits as a follow-up.
    async fn prompt(
        &mut self,
        agent: AgentId,
        content: Vec<UserContent>,
    ) -> Result<CommandOutcome, HostError> {
        let text = text_only(&content);
        if text.as_deref().is_some_and(str::is_empty) || content.is_empty() {
            return Err(HostError::Invalid("the prompt is empty".to_string()));
        }
        let attached = text.is_none();
        // A text-only prompt is one trimmed block of text, as a typed
        // submit sends it.
        let content = text.map_or(content, |text| vec![UserContent::text(text)]);
        if !self.turns.is_busy(&self.lifecycle, agent) {
            return self.start_with(agent, &content).await;
        }
        if attached {
            // The pending-message box shows text and a withdrawal hands
            // back text, so a client could neither show a waiting
            // attachment nor take it back. Refusing beats that.
            return Err(HostError::Conflict {
                reason: "attachments cannot be queued while the agent is busy".to_string(),
            });
        }
        self.edit_queue(agent, QueueEdit::FollowUp(&content))?;
        Ok(CommandOutcome::Accepted)
    }

    /// The steer gesture: while busy, queue as steering, or promote the
    /// pending follow-up when there is no text. While idle there is nothing
    /// to steer yet, so text is a prompt.
    async fn steer(&mut self, agent: AgentId, text: String) -> Result<CommandOutcome, HostError> {
        let text = text.trim().to_string();
        if self.turns.is_busy(&self.lifecycle, agent) {
            let edit = if text.is_empty() {
                QueueEdit::Promote
            } else {
                QueueEdit::Steer(&text)
            };
            self.edit_queue(agent, edit)?;
            return Ok(CommandOutcome::Accepted);
        }
        if text.is_empty() {
            return Ok(CommandOutcome::Accepted);
        }
        self.start_with(agent, &[UserContent::text(text)]).await
    }

    /// Route a cancel to the mechanism that owns `agent`'s turn.
    ///
    /// `Accepted` means the owner was signalled to end the turn, or the turn
    /// had already ended. A running mark with no owner is refused and logged
    /// so neither the wire answer nor the host's invariant tripwire claims a
    /// cancellation happened when no mechanism could perform it.
    fn cancel(&self, agent: AgentId) -> Result<CommandOutcome, HostError> {
        // A driven turn first, so a sub-agent's independent continuation
        // keeps its direct route.
        if self.turns.cancel(agent) {
            return Ok(CommandOutcome::Accepted);
        }
        if let AgentId::Sub(n) = agent
            && let Some(assignment) = self.session.core.registry.assignment(n)
        {
            assignment.interrupt();
            return Ok(CommandOutcome::Accepted);
        }
        // A detached background run belongs to the task registry, not to any
        // turn. Delegating a Running entry to the kill path gives both
        // gestures the same conclusion, notice, status, and parked usage. A
        // terminal entry closes the completion race: its AgentEnd may still
        // be queued, but the registry already says there is nothing to end.
        if let AgentId::Sub(n) = agent
            && let Some((task, status)) = self.session.core.task_registry.latest_agent_task(n)
        {
            return if status == TaskStatus::Running {
                self.kill_task(task)
            } else {
                Ok(CommandOutcome::Accepted)
            };
        }
        if self.lifecycle.is_running(agent) {
            // Running by the mark, owned by neither a turn nor a task: no
            // mechanism here can end it. Reachable only through a leaked
            // mark, which is why it refuses out loud instead of accepting.
            // The refusal is logged as well as returned, so the invariant
            // leaves a trace even if the client disconnects before rendering
            // the reason.
            let agent = agent_label(agent);
            tracing::warn!("a cancel of {agent} found a running mark no turn or task owns");
            return Err(HostError::Conflict {
                reason: format!("{agent} is marked running but no turn or task owns it"),
            });
        }
        Ok(CommandOutcome::Accepted)
    }

    fn kill_agent(&self, agent: AgentId) -> Result<CommandOutcome, HostError> {
        let AgentId::Sub(n) = agent else {
            return Err(HostError::Invalid(
                "the main agent is not a kill target".into(),
            ));
        };
        if let Some(assignment) = self.session.core.registry.assignment(n) {
            assignment.cancel();
            return Ok(CommandOutcome::Accepted);
        }
        self.cancel(agent)
    }

    fn queue_op(&self, op: QueueOp) -> Result<CommandOutcome, HostError> {
        Ok(match op {
            QueueOp::Remove { agent } => {
                let pending = self.session.core.message_queues.pending(agent);
                if pending.is_some_and(|(_, input)| input.has_attachments()) {
                    // A withdrawal hands back text. Taking the message
                    // would drop its attachment without a word.
                    return Err(HostError::Conflict {
                        reason: "the pending message has an attachment, which cannot be \
                                 pulled back. Continue sends it, and clearing the queue \
                                 discards it"
                            .to_string(),
                    });
                }
                let text = self.edit_queue(agent, QueueEdit::Withdraw)?;
                // Handed back so a client can restore it to its editor,
                // which is what the local dequeue gesture does.
                CommandOutcome::Withdrawn(text)
            }
            QueueOp::Clear => {
                // Session-wide, so every agent that has
                // something queued gets its own `QueueUpdate`: a client
                // tracks the queues per agent and would otherwise keep
                // showing the ones it was not told about.
                for agent in self.session.core.message_queues.queued_agents() {
                    self.edit_queue(agent, QueueEdit::Withdraw)?;
                }
                CommandOutcome::Accepted
            }
        })
    }

    /// Apply the resume policy to the turns the open found interrupted,
    /// once, as the session goes live. Under
    /// [`Resume::WaitForUser`](crate::recovery::Resume::WaitForUser) they
    /// wait for [`Self::continue_turn`].
    async fn apply_resume_policy(&mut self) {
        match crate::recovery::RESUME {
            crate::recovery::Resume::WaitForUser => {}
            crate::recovery::Resume::Continue => {
                if let Err(error) = self.resume_interrupted().await {
                    tracing::warn!("could not continue an interrupted turn: {error}");
                }
            }
        }
    }

    /// Continue every interrupted turn that still waits, for the main
    /// agent and for sub-agents alike. Returns whether any turn started.
    async fn resume_interrupted(&mut self) -> Result<bool, HostError> {
        let mut resumed = false;
        for agent in self.interrupted.clone() {
            if self.turns.is_busy(&self.lifecycle, agent) {
                continue;
            }
            // The main agent's turn may have been answered since: a prompt
            // or a wake continues it as well as this does.
            let unanswered = match agent {
                AgentId::Main => self.main_turn_unanswered().await,
                AgentId::Sub(_) => true,
            };
            self.interrupted.retain(|waiting| *waiting != agent);
            if unanswered {
                self.pick_up(agent).await?;
                self.spawn(agent, TurnStart::Continue)?;
                resumed = true;
            }
        }
        Ok(resumed)
    }

    /// Whether the main thread leaves input the model never
    /// answered.
    async fn main_turn_unanswered(&self) -> bool {
        let log = self.session.core.log.lock().await;
        log.head()
            .is_some_and(|head| turn_unanswered(&log.linearize(head, ThreadFilter::MAIN)))
    }

    /// The Continue gesture: resume the interrupted turns, of the main
    /// agent and of sub-agents, or else deliver pending work left without
    /// a turn. The host decides which applies, so a client can offer the
    /// action without tracking that.
    async fn continue_turn(&mut self) -> Result<CommandOutcome, HostError> {
        let resumed = self.resume_interrupted().await?;
        if self.turns.is_busy(&self.lifecycle, AgentId::Main) {
            // Busy with the turn this just resumed, or with its own while
            // a sub-agent's was resumed.
            return match resumed {
                true => Ok(CommandOutcome::Accepted),
                false => Err(HostError::Conflict {
                    reason: "a turn is running".to_string(),
                }),
            };
        }
        // A turn can be cut short on a branch the session only came to by
        // a head switch, which the open did not see.
        if self.main_turn_unanswered().await {
            return self.spawn(AgentId::Main, TurnStart::Continue);
        }
        // Work restored after a finished turn has no turn to ride on.
        let core = &self.session.core;
        if core.message_queues.has_pending(AgentId::Main)
            || core.task_registry.has_notices(AgentId::Main)
        {
            return self.spawn(AgentId::Main, TurnStart::Wake);
        }
        if resumed {
            return Ok(CommandOutcome::Accepted);
        }
        Err(HostError::Conflict {
            reason: "the last turn finished, so there is nothing to continue".to_string(),
        })
    }

    fn compact(&mut self, instructions: Option<String>) -> Result<CommandOutcome, HostError> {
        if self.turns.is_busy(&self.lifecycle, AgentId::Main) {
            return Err(HostError::Conflict {
                reason: "a turn is running".to_string(),
            });
        }
        self.spawn(
            AgentId::Main,
            TurnStart::Compact {
                reason: aj_agent::events::CompactionReason::Manual,
                instructions,
            },
        )
    }

    /// Set the session's tag and put the new label on its row.
    ///
    /// The write happens here rather than at the host's surface because this
    /// task holds the session's advisory lock: a tag materializes like any
    /// other command so that two writers of one store cannot interleave on a
    /// session's label. It goes through the log, which commits the label of a
    /// stored session and keeps that of a session not stored yet in its
    /// draft, which the session's first input stores.
    ///
    /// A tag is display metadata and nothing else, so it appends no log entry
    /// and publishes no `state` frame. The session list is where it surfaces,
    /// which is what the dirty mark is for.
    async fn tag(&self, tag: Option<String>) -> Result<CommandOutcome, HostError> {
        let set = self.session.core.log.lock().await.set_tag(tag.as_deref());
        set.map_err(|error| not_saved("the label", error))?;
        // After the write, so a label that would not be written leaves the
        // row saying what the store still says.
        self.session.status().tag = tag;
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Set the session's archived bit and put it on its row.
    ///
    /// Here and through the log for the reasons [`Self::tag`] gives: this
    /// task holds the session's advisory lock, which is what orders two
    /// writers of one store on a session's bit.
    ///
    /// The bit is display metadata and nothing else, so this appends no log
    /// entry, publishes no `state` frame, and touches nothing about the
    /// session's life: a session working through a turn goes on working,
    /// archived. Nothing else in this driver writes it either, so a prompt to
    /// an archived session leaves it archived.
    async fn archive(&self, archived: bool) -> Result<CommandOutcome, HostError> {
        let set = self.session.core.log.lock().await.set_archived(archived);
        set.map_err(|error| not_saved("the archive marker", error))?;
        // After the write, so a bit that would not be written leaves the
        // row saying what the store still says.
        self.session.status().archived = archived;
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    fn kill_task(&self, task: TaskId) -> Result<CommandOutcome, HostError> {
        if !self.session.core.task_registry.kill(task) {
            return Err(HostError::UnknownTask(task));
        }
        // Idempotent for a task that already finished: firing its completed
        // driver's token has no remaining observer.
        Ok(CommandOutcome::Accepted)
    }

    fn spawn(&mut self, agent: AgentId, start: TurnStart) -> Result<CommandOutcome, HostError> {
        self.prepare_goal_turn(agent, matches!(&start, TurnStart::Goal(_)));
        if !self
            .turns
            .spawn(&self.session.core, &self.shared.config, agent, start)
        {
            // A command picks its agent up before it starts a turn on it.
            return Err(HostError::Internal(
                format!("{} has no live agent", agent_label(agent)).into(),
            ));
        }
        if agent == AgentId::Main {
            self.compaction_failed = false;
        }
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    fn validate_settings_persistence(&self, change: &SettingsChange) -> Result<(), HostError> {
        if change.persist != PersistAction::None && change.agent != AgentId::Main {
            return Err(HostError::Invalid(
                "sub-agent settings cannot persist defaults".into(),
            ));
        }
        if matches!(
            change.persist,
            PersistAction::ProjectSet | PersistAction::ProjectClear
        ) && self
            .shared
            .layers
            .lock()
            .expect("config layers mutex poisoned")
            .project_path
            .is_none()
        {
            return Err(HostError::Invalid(
                "Project settings need a git repository on the host.".into(),
            ));
        }
        Ok(())
    }

    /// Apply a settings change and publish its frames: the notice the
    /// change recorded with its entry, then a refreshed `state`.
    ///
    /// A change that did not apply is an error, never an acceptance: the
    /// host staged nothing and has nothing to publish, and a client told
    /// "accepted" would show settings this host never adopted.
    async fn settings(&mut self, change: SettingsChange) -> Result<CommandOutcome, HostError> {
        self.validate_settings_persistence(&change)?;
        let SettingsChange {
            agent,
            persist,
            axis,
        } = change;
        self.pick_up(agent).await?;
        let core = &self.session.core;
        let shared = &self.shared;
        let target = axis.model_target();
        if target == crate::settings::ModelTarget::Oracle && agent != AgentId::Main {
            return Err(HostError::Invalid(
                "Oracle settings belong to the session".into(),
            ));
        }
        let mut transient_confirmation = false;
        let outcome: ConfirmOutcome = match (axis, agent) {
            (
                SettingsAxis::Thinking(level) | SettingsAxis::OracleThinking(level),
                AgentId::Main,
            ) => {
                crate::settings::confirm_thinking(
                    target,
                    level,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
            }
            (SettingsAxis::Thinking(level), AgentId::Sub(n)) => {
                let tracked = self.sub_settings(n).await.model.and_then(|key| {
                    self.shared
                        .catalog
                        .iter()
                        .find(|info| info.provider == key.0 && info.id == key.1)
                        .cloned()
                        .map(Arc::new)
                });
                crate::settings::confirm_thinking_for_sub(level, n, tracked, core).await
            }
            (SettingsAxis::ThinkingDisplay(display), AgentId::Main) => {
                transient_confirmation = true;
                crate::settings::confirm_thinking_display_for_main(
                    display,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                )
            }
            (SettingsAxis::Model(info) | SettingsAxis::OracleModel(info), AgentId::Main) => {
                crate::settings::confirm_model(
                    target,
                    info,
                    persist,
                    &shared.auth,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
            }
            (SettingsAxis::Model(info), AgentId::Sub(n)) => {
                let speed = self
                    .sub_settings(n)
                    .await
                    .speed
                    .as_deref()
                    .and_then(aj_models::speed_from_name)
                    .flatten();
                crate::settings::confirm_model_for_sub(&info, n, &shared.auth, speed, core).await
            }
            (SettingsAxis::Speed(speed), AgentId::Main) => {
                crate::settings::confirm_speed(
                    speed,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
            }
            (SettingsAxis::Speed(speed), AgentId::Sub(n)) => {
                let tracked = self.sub_settings(n).await.model.and_then(|key| {
                    self.shared
                        .catalog
                        .iter()
                        .find(|info| info.provider == key.0 && info.id == key.1)
                        .cloned()
                        .map(Arc::new)
                });
                crate::settings::confirm_speed_for_sub(speed, n, tracked, core).await
            }
            (
                SettingsAxis::Verbosity(verbosity) | SettingsAxis::OracleVerbosity(verbosity),
                AgentId::Main,
            ) => {
                crate::settings::confirm_verbosity(
                    target,
                    verbosity,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
            }
            (
                SettingsAxis::OracleModel(_)
                | SettingsAxis::OracleThinking(_)
                | SettingsAxis::OracleVerbosity(_)
                | SettingsAxis::ThinkingDisplay(_)
                | SettingsAxis::Verbosity(_),
                AgentId::Sub(n),
            ) => {
                // Malformed rather than unservable: these axes are
                // session-wide, so no host could serve this request.
                return Err(HostError::Invalid(format!(
                    "thinking display and verbosity are session-wide and cannot be set for agent {n}"
                )));
            }
        };
        if !outcome.applied {
            return Err(HostError::Unsupported(outcome.notice));
        }

        if transient_confirmation {
            self.publish_event(
                None,
                AgentEvent::Notice {
                    agent_id: AgentId::Main,
                    text: outcome.notice.clone(),
                },
            );
        }

        // The change recorded its notice with its entry. Fold it now, so it
        // is published before the `state` frame below and before the
        // command is answered.
        self.drain_events();
        let incomplete = (!outcome.notes.is_empty()).then(|| {
            format!(
                "Current-session setting applied. {}",
                outcome.notes.join(" ")
            )
        });
        for note in outcome.notes {
            // A failed config write is a live-only
            // diagnostic: no entry exists for a backfill to regenerate it
            // from, so it rides an untagged frame.
            self.publish_event(
                None,
                AgentEvent::Warning {
                    agent_id: agent,
                    text: format!("Current-session setting applied. {note}"),
                },
            );
        }
        let (settings, oracle_settings) = settings_of(&self.session.core.run_config);
        self.session.publish_state(&self.shared.fanout, |status| {
            status.settings = settings;
            status.oracle_settings = oracle_settings;
            true
        });
        for text in outcome.notices {
            self.publish_event(
                None,
                AgentEvent::Notice {
                    agent_id: agent,
                    text,
                },
            );
        }
        Ok(incomplete.map_or(CommandOutcome::Accepted, CommandOutcome::Incomplete))
    }

    async fn account(
        &mut self,
        provider: String,
        account: Option<String>,
    ) -> Result<CommandOutcome, HostError> {
        crate::model::validate_account_selection(&self.shared.auth, &provider, account.as_deref())
            .await
            .map_err(HostError::Invalid)?;
        let choices = self
            .session
            .core
            .run_config
            .lock()
            .expect("run config mutex poisoned")
            .accounts
            .clone();
        if choices.get(&provider) == account {
            return Ok(CommandOutcome::Accepted);
        }
        let selection = match account.as_deref() {
            None => "Provider default".to_string(),
            Some("") => "Unnamed account".to_string(),
            Some(label) => format!("{label:?}"),
        };
        let confirmation = format!("{provider} account: {selection}.");
        self.session
            .core
            .journal
            .record(|log| {
                let entry = log.append_account_change(&provider, account.as_deref())?;
                let events =
                    crate::settings::change_events(log, AgentId::Main, &entry, &confirmation);
                Ok(((), events))
            })
            .await
            .map_err(internal)?;
        choices.set(&provider, account.clone());
        // The change's notice goes out before the `state` frame.
        self.drain_events();
        self.publish_state();
        Ok(CommandOutcome::Accepted)
    }

    async fn environment(
        &mut self,
        key: String,
        value: Option<String>,
    ) -> Result<CommandOutcome, HostError> {
        // Validate removals too, without ever putting a value into a notice.
        aj_session::validate_session_env(&std::collections::BTreeMap::from([(
            key.clone(),
            value.clone().unwrap_or_default(),
        )]))
        .map_err(|err| HostError::Invalid(err.to_string()))?;
        self.require_idle()?;
        let changed = self
            .session
            .core
            .journal
            .record(|log| {
                let current = log.session_env().cloned().unwrap_or_default();
                let mut env = current.clone();
                match &value {
                    Some(value) => {
                        env.insert(key.clone(), value.clone());
                    }
                    None => {
                        env.remove(&key);
                    }
                }
                if env == current {
                    return Ok((None, Vec::new()));
                }
                let entry = log.append_env_change(env.clone())?;
                let events = crate::settings::change_events(
                    log,
                    AgentId::Main,
                    &entry,
                    "Session environment changed",
                );
                Ok((Some(env), events))
            })
            .await
            .map_err(internal)?;
        let Some(env) = changed else {
            return Ok(CommandOutcome::Accepted);
        };
        self.session.core.agent.lock().await.set_session_env(env);
        // The change's notice goes out before the `state` frame.
        self.drain_events();
        self.publish_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// A child's spawn snapshot and recorded edits, without taking its agent
    /// lock, which may be held for a running turn. Staged overrides take
    /// precedence when applying a setting to that child.
    async fn sub_settings(&self, n: usize) -> aj_session::SessionSettings {
        let filter = ThreadFilter::sub(n);
        let log = self.session.core.log.lock().await;
        log.latest_leaf(filter)
            .map(|head| log.linearize(&head, filter).settings())
            .unwrap_or_default()
    }

    fn require_idle(&self) -> Result<(), HostError> {
        let (agents, tasks) = self.running_work();
        if agents + tasks > 0 {
            return Err(HostError::Conflict {
                reason: format!("{agents} agents and {tasks} background tasks are still running"),
            });
        }
        Ok(())
    }

    /// Driven turns and running agent runs, and every other running task.
    fn running_work(&self) -> (usize, usize) {
        let snapshot = self.session.core.task_registry.snapshot();
        running_work_counts(
            self.turns.driven(),
            snapshot.iter().map(|task| (&task.kind, task.status)),
        )
    }

    /// Refuse a head switch unless the session is quiesced: nothing
    /// runs for any agent, and nothing accepted waits to be delivered.
    ///
    /// Live work therefore exists only on the thread the session is
    /// on, and a switch has nothing to carry over, let go of or take up.
    /// The refusal names what is in the way and how to clear it.
    fn require_quiesced(&self) -> Result<(), HostError> {
        let core = &self.session.core;
        let (agents, tasks) = self.running_work();
        let mut blocking = Vec::new();
        if agents + tasks > 0 {
            blocking.push(format!(
                "{agents} agents and {tasks} background tasks are still running: \
                 wait for them or stop them"
            ));
        }
        // Continue delivers the main agent's waiting work. A sub-agent's is
        // delivered by its next turn, which a message to it starts.
        if !core.message_queues.queued_agents().is_empty() {
            blocking.push(
                "a queued message is waiting: withdraw it, or deliver it with Continue \
                 (for a sub-agent, by sending it a message)"
                    .into(),
            );
        }
        if core.task_registry.has_any_notices() {
            blocking.push(
                "a finished task's notification is waiting: deliver it with Continue \
                 (for a sub-agent, by sending it a message)"
                    .to_string(),
            );
        }
        if blocking.is_empty() {
            return Ok(());
        }
        Err(HostError::Conflict {
            reason: format!("the branch was not switched. {}", blocking.join(". ")),
        })
    }

    async fn prepare_head(
        &self,
        log: &aj_session::SessionLog,
        entry: &String,
        changes: &aj_wire::BranchChanges,
    ) -> Result<
        (
            crate::session_setup::RunConfigSnapshot,
            std::collections::BTreeMap<String, String>,
        ),
        HostError,
    > {
        let env = log
            .session_env_at(Some(entry))
            .map_err(internal)?
            .unwrap_or_default();
        let run = self
            .session
            .core
            .run_config
            .lock()
            .expect("run config mutex poisoned")
            .clone();
        super::head::prepare(
            run,
            &log.linearize(entry, ThreadFilter::MAIN).settings(),
            &self.shared,
            env,
            changes,
        )
        .await
    }

    /// Switch the session's head to `target`, in place.
    ///
    /// Refused unless the session is quiesced (see
    /// [`Self::require_quiesced`]): a mid-turn switch would let the running
    /// turn persist onto the wrong branch, and work that waits would be
    /// delivered on a branch it was not accepted on. On success the state
    /// that belonged to the branch being left is reset, the agent is
    /// reseeded from the new branch, a fresh epoch is minted and `reset`
    /// published.
    ///
    /// The switch happens whole or not at all. Everything that can be
    /// refused comes before the head moves, and a refusal there leaves the
    /// session as it was. Moving the head is the point of no return. The
    /// one fallible step after it is the write of what the switch appends
    /// to the branch it lands on, which needs the head there. If that
    /// fails, the log is on the new branch and nothing else is, so the
    /// session is ended as after a storage failure ([`Self::stop_with`])
    /// and clients open it again from what is stored. Every later step
    /// only changes memory and cannot fail.
    async fn head_switch(
        &mut self,
        target: HeadTarget,
        changes: aj_wire::BranchChanges,
    ) -> Result<CommandOutcome, HostError> {
        // Everything already on the event stream belongs to the epoch this
        // switch is about to replace. Published afterwards it would carry
        // the new epoch, which no client's epoch filter can reject, and it
        // would name positions in the history the switch just left.
        //
        // Before the check, because a drained event can start a turn (a
        // task's end wakes its owner). From the check to the head move only
        // this task could start work, and it does not: it takes no request
        // and folds no event until the switch returns.
        self.drain_events();
        self.require_quiesced()?;

        // Cloned so the guard does not borrow `self`.
        let journal = self.session.core.journal.clone();
        let switched = {
            let mut log = journal.lock().await;
            // Resolved here rather than at the caller, under the same lock
            // that moves the head: a parent read outside it could be
            // superseded by an append before the switch lands.
            let entry = resolve_head_target(&log, &target)?;
            // The target branch has its own model context and display
            // tail, which the log may not hold. They are read before the
            // head moves, off the async threads. An earlier entry of the
            // selected branch, after its last compaction, needs no read.
            let left = match log.branch_read(&entry).map_err(internal)? {
                Some(read) => {
                    let branch = tokio::task::spawn_blocking(move || read.read())
                        .await
                        .map_err(internal)?
                        .map_err(internal)?;
                    Some(log.log_mut().install_branch(branch).map_err(internal)?)
                }
                None => None,
            };
            let moved = match self.prepare_head(&log, &entry, &changes).await {
                // The live evaluator belongs to the branch being left. It
                // is shut down while the switch can still be refused. Its
                // values stay, and an evaluator is built again when needed.
                Ok(prepared) => match self.shutdown_evaluator().await {
                    Ok(()) => log
                        .log_mut()
                        .set_head(entry)
                        .map(|()| prepared)
                        .map_err(|error| not_saved("the branch switch", error)),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            let (prepared, env) = match moved {
                Ok(prepared) => prepared,
                Err(error) => {
                    // The head stays where it was, so the log goes back to
                    // holding its branch.
                    if let Some(left) = left {
                        log.log_mut().restore_branch(left);
                    }
                    return Err(error);
                }
            };
            // The head has moved. What the switch appends to the branch it
            // lands on, in one write. At a fork point all of it joins the
            // branch's draft and is stored with the branch's first input. None
            // of it has an event: the `reset` below makes every client read
            // the new branch again.
            let appended = log
                .record(|log| {
                    log.in_draft(|log| {
                        let goal = crate::recovery::pause_goal(log)?;
                        // A fork point can sit between a tool call and its result.
                        // No task launch is left unanswered on the target: those are
                        // answered when a session opens, and none runs across a switch.
                        let head = log.head().cloned().expect("the head was just set");
                        let branch = log.linearize(&head, ThreadFilter::MAIN);
                        let main = AgentId::Main;
                        aj_session::repair_interrupted_tool_uses(log, main, &branch)?;
                        let head = log.head().cloned().expect("repair keeps a head");
                        let branch = log.linearize(&head, ThreadFilter::MAIN);
                        super::head::append_changes(log, &changes, &prepared, &env)?;
                        Ok(((goal, branch), Vec::new()))
                    })
                })
                .await;
            match appended {
                Ok((goal, branch)) => Ok((log, goal, branch, prepared, env)),
                Err(error) => Err(error),
            }
        };
        let (log, goal, branch, prepared, env) = match switched {
            Ok(switched) => switched,
            Err(error) => {
                // The log guard is released, as the teardown needs.
                let reason = format!(
                    "Switching branches failed after the session had moved: {error}. AJ \
                     stopped the session so that it is read again from what is stored. \
                     Check which branch it is on, then switch again if needed."
                );
                self.stop_with(reason.clone()).await;
                return Err(HostError::Internal(reason.into()));
            }
        };
        {
            self.goal = goal::GoalRun::new(goal);
            self.session
                .core
                .message_queues
                .set_context(AgentId::Main, None);
            *self
                .session
                .core
                .run_config
                .lock()
                .expect("run config mutex poisoned") = prepared;
            self.reset_branch_state();
            // Nothing is driven, so reseeding cannot race an inference request.
            {
                let mut agent = self.session.core.agent.lock().await;
                agent.reseed_transcript(branch.agent_messages());
                agent.set_session_env(env);
                agent.clear_todo_list();
                agent.set_code_mode_store(branch.code_mode_store());
            }
            // The position of what the switch appended belongs to the row
            // of the branch it landed on. Nothing else is on the stream:
            // the session is quiesced and this task holds the log.
            self.drain_events();
            // The epoch moves under the log lock, so an attach that
            // snapshots the log cannot pair the new projection with the old
            // epoch.
            let mut status = self.session.status();
            status.goal = self.goal.current.clone();
            (status.settings, status.oracle_settings) = settings_of(&self.session.core.run_config);
            status.epoch = mint_epoch();
            // The turn state is the new branch's thread's.
            status.turn_interrupted =
                self.session.core.turn.get(AgentId::Main) == TurnState::Interrupted;
            status.note_activity();
        }
        drop(log);

        // The backfill boundaries live streams filter against are left
        // alone. The store is append-only and `set_head` renumbers nothing
        // in it, so every position the new epoch publishes sits above
        // every stored position an old boundary covers. A boundary handed
        // out at a fork point also covers the positions of the branch's
        // draft, which a switch away gives up and later records take. Its
        // client attaches again on the `reset` below, and that backfill
        // reads those records.
        self.shared.fanout.publish(Frame::Reset {
            session: self.session.id().to_string(),
        });
        self.publish_state();
        let notices = crate::settings::speed_fallback_notices(
            &self
                .session
                .core
                .run_config
                .lock()
                .expect("run config mutex poisoned"),
        );
        for text in notices {
            self.publish_event(
                None,
                AgentEvent::Notice {
                    agent_id: AgentId::Main,
                    text,
                },
            );
        }
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Drop the per-branch state a head switch leaves behind.
    ///
    /// A session switch in the local frontend replaces the whole
    /// `SessionCore`, so it cannot carry state across a branch. A head
    /// switch keeps the core (its handles are held by an in-process client
    /// and by the log's open file), so what would otherwise leak is dropped
    /// here. Only valid on a quiesced session, which the refusal in
    /// [`Self::head_switch`] establishes.
    ///
    /// What deliberately survives: the agent's sub-agent counter, so ids
    /// stay unique across the switch, and its accumulated usage, which
    /// records tokens this process really spent.
    fn reset_branch_state(&self) {
        self.session.status().turn_outcome = None;
        // A sub-agent the new branch does not show must not be prompted:
        // its run would grow under the new epoch, and every attached
        // client would fold it into the new branch's transcript. Dropping
        // every handle is enough, because a prompt builds the agent again
        // from its stored run, which only a branch that shows the run
        // allows (see `Self::pick_up`).
        self.session.core.registry.clear();
        // A staged override is recorded on its sub-agent's thread,
        // which is where an agent built again reads it from.
        self.session
            .core
            .sub_overrides
            .lock()
            .expect("sub overrides mutex poisoned")
            .clear();
        // The terminal entries of the branch being left. The re-attach's
        // `caught_up` carries the table and would otherwise hand a client
        // the other branch's tasks.
        self.session.core.task_registry.clear();
    }

    // -- teardown --------------------------------------------------------

    /// Shut down the main agent's live Code Mode evaluator ahead of a head
    /// switch. Nothing is driven, so the agent is free.
    async fn shutdown_evaluator(&self) -> Result<(), HostError> {
        let mut agent = self.session.core.agent.lock().await;
        agent
            .shutdown_code_mode()
            .await
            .map_err(HostError::Internal)
    }

    /// End this materialization after its log fused.
    async fn stop_for_persistence(&mut self, failure: PersistenceFailure) {
        self.stop_with(persistence_failure_message(&failure)).await;
    }

    /// End this materialization because its memory can no longer be
    /// trusted to match the store: tell every attached client once with
    /// `message`, detach them, and stop as a shutdown does. The driver loop
    /// returns once the request in hand is answered.
    ///
    /// The error frame is what makes a client let go of this materialization
    /// and ask for the session again, and the ask rebuilds it from
    /// disk because the host treats a draining entry as absent. Detaching
    /// after the frame lets the idle sweeper reap this entry.
    async fn stop_with(&mut self, message: String) {
        self.session.start_draining();
        self.shared.fanout.publish(Frame::Error {
            session: self.session.id().to_string(),
            code: PERSISTENCE_FAILED_CODE.to_string(),
            message,
        });
        self.shared.fanout.detach_all(self.session.id());
        self.wind_down().await;
        self.stopped = true;
    }

    /// Stop the session as a host that died stops it. This is the one way
    /// a host lets go: for a shutdown, a storage failure, a dropped
    /// session, and an idle release, which finds nothing running.
    ///
    /// 1. The session is closed for writing. From here the store holds
    ///    what the next open will find, and this returns no earlier than
    ///    any write that was in progress.
    /// 2. The turns are aborted where they stand. A turn that is cut
    ///    short writes no aborted message and no cancelled tool result,
    ///    which is what tells it from a turn the user cancelled. A
    ///    foreground command ends with its tool call. The turns' tokens
    ///    are fired first, for what runs outside a turn's task and only
    ///    watches the token, a provider request for example.
    /// 3. The task drivers are let go. A task record keeps running
    ///    under its row. An agent run and a Code Mode cell end, and the
    ///    loss each announced when it started stays stored.
    ///
    /// Nothing is recorded about any of it: not that a turn ended, not a
    /// sub-agent's end, not a goal's pause. The next open settles what is
    /// left ([`crate::recovery::recover`]).
    async fn wind_down(&mut self) {
        self.session.start_draining();
        self.session.core.journal.close();
        self.turns.cancel_all();
        self.turns.shutdown().await;
        if !crate::shutdown_background_tasks_quietly(&self.session.core.task_registry).await {
            tracing::warn!(
                session = self.session.id(),
                phase = "background task quiesce",
                "background tasks still running after the shutdown grace; forced abortable drivers while the session owner retains reap responsibility"
            );
        }
    }

    /// The row a release has to hand the store, or `None` when the session may
    /// not go after all.
    ///
    /// Read under the log lock, which this task holds along with the session's
    /// advisory lock, so the row and the file state it describes cannot
    /// disagree and no rival writer can be between them. Nothing can append
    /// between here and the teardown either: a releasable session has no turn,
    /// no live task and nothing queued, and this task is its only appender.
    ///
    /// Every `None` here is a session that stays live, which is the safe
    /// direction. A release the host cannot record a row for would leave the
    /// session out of the directory, or leave a row that predates the
    /// materialization, and both are worse than holding the lock for another
    /// grace.
    fn row_for_release(&self) -> Option<ReleasedRow> {
        // `try_lock`, because the host holds its session map while it waits for
        // this answer: the log can be held for the length of an export render,
        // and a release that queued behind one would stall every other
        // session's materialization for as long. Declining costs one grace.
        let log = self.session.core.log.try_lock().ok()?;
        // A session not stored yet is the one condition `releasable` cannot see.
        // Releasing such a session would not hand it back to the store, which
        // does not know its id, it would drop it.
        if !log.is_durable() {
            return None;
        }
        let file = log.storage_metadata().ok()??;
        // The stamp is what this driver saw, not when the store was last
        // written: a row stamped with its own teardown reads to a client as
        // output it has not seen.
        //
        // The status lock nests under the log lock here. That is the order the
        // driver always takes them in, and nothing takes the log lock while
        // holding the status.
        let (last_activity, tag, archived) = {
            let status = self.session.status();
            (status.last_activity, status.tag.clone(), status.archived)
        };
        Some(ReleasedRow {
            file,
            last_activity,
            tag,
            archived,
        })
    }
}

/// The trimmed text of a prompt whose content is text only, `None` when it
/// carries anything else (an image, say). An empty string means the prompt
/// was blank, which the caller refuses rather than sending.
fn text_only(content: &[UserContent]) -> Option<String> {
    let mut text = String::new();
    for block in content {
        match block {
            UserContent::Text(part) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&part.text);
            }
            UserContent::Image(_) => return None,
        }
    }
    Some(text.trim().to_string())
}

/// Name an agent in a host refusal without exposing Rust's debug syntax.
fn agent_label(agent: AgentId) -> String {
    match agent {
        AgentId::Main => "the main agent".to_string(),
        AgentId::Sub(n) => format!("sub-agent {n}"),
    }
}

/// A change the store refused, which therefore did not happen. A busy
/// database is a state the user can wait out, so it is not an internal
/// error.
fn not_saved(what: &str, error: aj_session::SessionError) -> HostError {
    if error.is_busy() {
        return HostError::Conflict {
            reason: format!(
                "the session database is busy, so {what} was not saved. Try again in a moment"
            ),
        };
    }
    HostError::Internal(format!("{what} could not be saved: {error}").into())
}

fn internal(err: impl std::error::Error + Send + Sync + 'static) -> HostError {
    HostError::Internal(Box::new(err))
}
