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
//! strictly increasing seq order" a property of the code rather than a
//! hope: the event stream is already in append order (the forwarder sends
//! under the guard that appended), and a frame this task emits itself is
//! published only after the stream has been drained up to that append.
//!
//! **Request ordering.** Requests are answered one at a time in arrival
//! order: the loop awaits a command's completion before taking the next.
//! A caller that got its acceptance therefore knows every command accepted
//! before it has already taken effect, which is what lets a client sequence
//! gestures (a settings change and then the prompt that should run under it)
//! without a barrier of its own. The cost is that a slow command (a head
//! switch) delays every other client's, which is why refusals are cheap and
//! reads bypass this task entirely.

use std::sync::Arc;
use std::time::Duration;

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_models::types::UserContent;
use aj_session::{
    EntryRef, SessionMetadata, TaggedEvent, ThreadFilter, repair_interrupted_tool_uses,
};
use aj_wire::{DurableEvent, Frame};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::host::live::{self, LiveSession, ReleaseOutcome, ReleasedRow, Request, settings_of};
use crate::host::{
    Command, CommandOutcome, HeadTarget, HostError, HostShared, QueueOp, SettingsAxis,
    SettingsChange, mint_epoch,
};
use crate::session::AgentLifecycle;
use crate::settings::ConfirmOutcome;
use crate::turn::{Joined, TurnStart, Turns, running_work_counts};

/// Resolve a head target against `log` to the entry the head moves to.
///
/// A `before` target answers its entry's parent, which is what makes a branch
/// replace the message it was taken from rather than continue after it. An
/// entry the log does not hold is a 404, and one with no parent is refused:
/// branching before a root would leave the session with no history at all,
/// and no transcript gesture can legitimately ask for it (spec 6.6).
fn resolve_head_target(
    log: &aj_session::ConversationLog,
    target: &HeadTarget,
) -> Result<String, HostError> {
    match target {
        HeadTarget::Entry(entry) => Ok(entry.clone()),
        HeadTarget::Before(entry) => match log.parent_of(entry) {
            Some(parent) if log.contains(parent) => Ok(parent.clone()),
            // The client named a real entry, so a parent id the log does not
            // hold is a torn log rather than a bad request.
            Some(parent) => Err(HostError::Internal(
                format!("entry {entry} names parent {parent}, which the log does not hold").into(),
            )),
            None if log.contains(entry) => Err(HostError::Invalid(format!(
                "entry {entry} starts the session, so there is nothing before it"
            ))),
            None => Err(HostError::UnknownEntry(entry.clone())),
        },
    }
}

/// How long a shutdown waits for cancelled turns to wind themselves down
/// before falling back to aborting them.
///
/// The graceful path is what keeps a transcript consistent (a cancelled
/// turn emits its synthetic aborted `MessageEnd`s and error tool results),
/// so we prefer it. A wedged turn must not hang the process, hence the
/// bound.
///
/// NOTE: `SessionHost::shutdown` winds its sessions down one at a time, so a
/// host holding several wedged sessions pays this per session. Fine while a
/// host holds a handful. The fix, if it ever matters, is to drive the
/// teardowns concurrently rather than to shorten the grace.
const TURN_DRAIN_GRACE: Duration = Duration::from_secs(5);

pub(crate) struct Driver {
    session: Arc<LiveSession>,
    shared: Arc<HostShared>,
    turns: Turns,
    lifecycle: AgentLifecycle,
    events: UnboundedReceiver<TaggedEvent>,
    requests: UnboundedReceiver<Request>,
    /// Set while winding down, so a reaped turn does not start a wake and
    /// keep the session alive forever.
    draining: bool,
}

impl Driver {
    pub(crate) fn new(
        session: Arc<LiveSession>,
        shared: Arc<HostShared>,
        events: UnboundedReceiver<TaggedEvent>,
        requests: UnboundedReceiver<Request>,
    ) -> Self {
        let turns = Turns::with_handoff(session.handoff.clone());
        Self {
            session,
            shared,
            turns,
            lifecycle: AgentLifecycle::default(),
            events,
            requests,
            draining: false,
        }
    }

    pub(crate) async fn run(mut self) {
        loop {
            tokio::select! {
                biased;

                // Requests first: a command's refusal has to reflect the
                // state as of now, and the event arm can be arbitrarily
                // busy during a streaming turn.
                request = self.requests.recv() => {
                    // A client that keeps commands in flight would
                    // otherwise starve the two arms below for as long as it
                    // keeps going: no frame would reach any client and no
                    // turn would be reaped. Catching up here also makes the
                    // refusal below reflect the events already in the
                    // channel rather than the state before them.
                    self.catch_up();
                    match request {
                        Some(Request::Command { command, reply }) => {
                            let outcome = self.command(command).await;
                            let _ = reply.send(outcome);
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
                        // The host asked us to stop, or dropped the session.
                        Some(Request::Shutdown) | None => {
                            self.wind_down().await;
                            return;
                        }
                    }
                },

                Some(tagged) = self.events.recv() => self.on_event(tagged),

                joined = self.turns.join_next() => self.on_join(joined),
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

    /// Fold one event of the session's tagged stream: update the
    /// lifecycle, advance the durable high-water mark, publish the frame,
    /// and start a wake if the event earned one.
    fn on_event(&mut self, tagged: TaggedEvent) {
        let TaggedEvent { entry, event } = tagged;
        // Captured off a borrow, because `publish_event` below takes the
        // event by value. The wake it decides on has to wait until after
        // `apply_lifecycle`: `Turns::spawn_wake` refuses a busy owner, and
        // this `AgentEnd` is what marks the owner idle, so waking any earlier
        // would find it busy and drop the wake. `TaskEnd` wakes
        // unconditionally so a completion notice reaches the model the moment
        // its task finishes. `AgentEnd` only when something is actually
        // queued, because a sub-agent's initial run ends inside its parent's
        // turn and never reaches the join arm.
        let trigger = match &event {
            AgentEvent::TaskEnd { agent_id, .. } => Some((*agent_id, false)),
            AgentEvent::AgentEnd { agent_id, .. } => Some((*agent_id, true)),
            _ => None,
        };
        self.apply_lifecycle(&event);
        self.publish_event(entry, event);
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
        if let Some((owner, conditional)) = trigger
            && (!conditional
                || self.session.core.task_registry.has_notices(owner)
                || self.session.has_queued(owner))
        {
            self.wake(owner);
        }
    }

    fn apply_lifecycle(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart { agent_id } => self.lifecycle.mark_running(*agent_id),
            AgentEvent::AgentEnd { agent_id, .. } => {
                self.lifecycle.mark_idle(*agent_id);
                self.note_finished(*agent_id);
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

    /// Record that `agent`'s run is over, if it is a sub-agent.
    ///
    /// This is the one fact a backfill's bracketing decisions derive from
    /// (see `SessionStatus::finished_subs`), so every path that observes a
    /// sub going idle has to come through here: the `AgentEnd` on the bus,
    /// and the join-time reap for a sub that emitted none.
    fn note_finished(&self, agent: AgentId) {
        if let AgentId::Sub(n) = agent {
            self.session.status().finished_subs.insert(n);
        }
    }

    /// Record whether the host is driving a turn for sub-agent `agent`.
    ///
    /// Called on both sides of a spawn, and at reap. A continuation of a
    /// finished sub-agent starts appending to its thread as soon as its task
    /// runs, so the record has to exist before the spawn, and has to go away
    /// again when the spawn did not take.
    fn note_driven(&self, agent: AgentId, driven: bool) {
        let AgentId::Sub(n) = agent else { return };
        let mut status = self.session.status();
        if driven {
            status.driven_subs.insert(n);
        } else {
            status.driven_subs.remove(&n);
        }
    }

    /// Handle one completed turn: reap, conclude the sub-agent boxes the
    /// reap swept, wake on queued work, and surface the outcome.
    fn on_join(&mut self, joined: Joined) {
        let Joined { agent, outcome } = joined;
        for idled in self
            .turns
            .reap(&mut self.lifecycle, &self.session.core.task_registry, agent)
        {
            self.note_finished(idled);
            self.note_driven(idled, false);
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
                    },
                );
            }
        }
        let result = match outcome {
            Ok(result) => result,
            Err(err) => {
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
                self.refresh_state();
                self.shared.fanout.mark_list_dirty();
                return;
            }
        };
        if !self.draining
            && (self.session.core.task_registry.has_notices(agent)
                || self.session.has_queued(agent))
        {
            self.wake(agent);
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

    fn wake(&mut self, owner: AgentId) {
        self.note_driven(owner, true);
        self.turns.spawn_wake(
            owner,
            &self.session.core,
            &self.lifecycle,
            &self.shared.config,
        );
        // The wake is a no-op for a busy owner and refused for one with no
        // live handle, so what was actually spawned decides.
        self.note_driven(owner, self.turns.is_driving(owner));
        self.refresh_state();
    }

    // -- publishing ------------------------------------------------------

    fn publish_event(&self, entry: Option<EntryRef>, event: AgentEvent) {
        // A `MessageEnd` rides the entry whose id is the message's own id: the
        // wire codec pins the two together and refuses any other pairing
        // (`Frame`'s serializer validates as it writes). Nothing validates on
        // the in-process path, so a host that got this wrong would look healthy
        // locally while every remote client's stream died and reconnected in a
        // loop. The assert puts that where the frame is built.
        if let AgentEvent::MessageEnd { message, .. } = &event {
            debug_assert_eq!(
                entry.as_ref().map(|entry| entry.id.as_str()),
                Some(message.id()),
                "a MessageEnd must be published with its own log entry",
            );
        }
        // Checked outside the guard below: a failing assert inside it would
        // poison the status mutex, and every later `status()` would panic on
        // a lock that is only there to publish frames.
        if let Some(entry) = &entry {
            let last_seq = self.session.status().last_seq;
            debug_assert!(
                entry.seq > last_seq,
                "durable seqs must be monotone per session: {} after {last_seq}",
                entry.seq,
            );
        }
        let (epoch, durability) = {
            let mut status = self.session.status();
            if let Some(entry) = &entry {
                // Monotone by `max` rather than by assignment: the assert
                // above is a debug-only guard, and a release build must not
                // let one out-of-order append walk the high-water mark
                // backwards and re-serve entries a client already applied.
                status.last_seq = status.last_seq.max(entry.seq);
                status.note_activity();
            }
            (
                status.epoch.clone(),
                entry.map(|entry| DurableEvent {
                    seq: entry.seq,
                    entry_id: entry.id,
                }),
            )
        };
        self.shared.fanout.publish(Frame::Event {
            session: self.session.id().to_string(),
            epoch,
            durability,
            event: event.into(),
        });
    }

    /// Publish a `state` frame when `working` changed, which is the flag a
    /// client seeds its spinner from and the one that self-heals an
    /// `AgentEnd` it missed (spec 6.3).
    ///
    /// The frame's `last_seq` also moves on every durable append, and we
    /// deliberately do not re-emit for that: the durable frame carries the
    /// same position, so a `state` per append would double the frame count
    /// to tell a client something it just learned. The session list is
    /// where a `last_seq` a client is not attached to surfaces.
    fn refresh_state(&self) {
        let working = self.turns.is_busy(&self.lifecycle, AgentId::Main);
        self.session.publish_state(&self.shared.fanout, |status| {
            let changed = status.working != working;
            status.working = working;
            changed
        });
    }

    fn publish_state(&self) {
        self.session.publish_state(&self.shared.fanout, |_| true);
    }

    /// Publish `agent`'s queue snapshot. The agent only emits this after a
    /// drain, so the host emits it on the enqueue side too (spec section 5)
    /// and a second client learns about a message it did not queue itself.
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
        self.shared.fanout.mark_list_dirty();
    }

    /// Publish everything already queued on the event stream.
    ///
    /// A frame this task emits itself carries an append position, and every
    /// entry below it has already reached the stream (the forwarder sends
    /// under the guard that appended). Draining first is therefore what
    /// keeps the published seqs monotone: without it, an append that
    /// happened before ours would be published after it.
    fn drain_events(&mut self) {
        while let Ok(tagged) = self.events.try_recv() {
            self.on_event(tagged);
        }
    }

    // -- commands --------------------------------------------------------

    async fn command(&mut self, command: Command) -> Result<CommandOutcome, HostError> {
        match command {
            Command::Prompt { agent, content } => self.prompt(agent, content),
            Command::Steer { agent, text } => self.steer(agent, text),
            Command::Cancel { agent } => Ok(self.cancel(agent)),
            Command::Queue(op) => Ok(self.queue_op(op)),
            Command::Compact { instructions } => self.compact(instructions),
            Command::Settings(change) => self.settings(change).await,
            Command::Tag { tag } => self.tag(tag),
            Command::Archive { archived } => self.archive(archived),
            Command::Head { target } => self.head_switch(target).await,
            Command::KillTask { task } => self.kill_task(task),
        }
    }

    /// Exactly the local submit gesture: run a turn when the target is
    /// idle, queue a follow-up when it is busy.
    fn prompt(
        &mut self,
        agent: AgentId,
        content: Vec<UserContent>,
    ) -> Result<CommandOutcome, HostError> {
        let text = text_only(&content);
        if text.as_deref().is_some_and(str::is_empty) || content.is_empty() {
            return Err(HostError::Invalid("the prompt is empty".to_string()));
        }
        if self.turns.is_busy(&self.lifecycle, agent) {
            let Some(text) = text else {
                // The pending-message queues hold text, so an attachment
                // cannot be queued. Refusing beats silently dropping it.
                return Err(HostError::Conflict {
                    reason: "attachments cannot be queued while the agent is busy".to_string(),
                });
            };
            self.session
                .core
                .message_queues
                .append_follow_up(agent, &text);
            self.publish_queue(agent);
            return Ok(CommandOutcome::Accepted);
        }
        let start = match text {
            // A text-only prompt takes the same path a typed submit does.
            Some(text) => TurnStart::Prompt(text),
            None => TurnStart::Content(content),
        };
        self.spawn(agent, start)
    }

    /// The steer gesture: while busy, queue as steering, or promote the
    /// pending follow-up when there is no text. While idle there is nothing
    /// to steer yet, so text starts a normal turn.
    fn steer(&mut self, agent: AgentId, text: String) -> Result<CommandOutcome, HostError> {
        let text = text.trim().to_string();
        if self.turns.is_busy(&self.lifecycle, agent) {
            if text.is_empty() {
                self.session.core.message_queues.promote(agent);
            } else {
                self.session
                    .core
                    .message_queues
                    .append_steering(agent, &text);
            }
            self.publish_queue(agent);
            return Ok(CommandOutcome::Accepted);
        }
        if text.is_empty() {
            return Ok(CommandOutcome::Accepted);
        }
        self.spawn(agent, TurnStart::Prompt(text))
    }

    fn cancel(&self, agent: AgentId) -> CommandOutcome {
        if self.turns.cancel(agent) {
            return CommandOutcome::Accepted;
        }
        if self.lifecycle.is_running(agent) {
            // A sub running its initial spawn is owned by the main turn,
            // so cancelling that token is what reaches the child.
            self.turns.cancel(AgentId::Main);
        }
        CommandOutcome::Accepted
    }

    fn queue_op(&self, op: QueueOp) -> CommandOutcome {
        match op {
            QueueOp::Remove { agent } => {
                let text = self.session.core.message_queues.take_pending(agent);
                self.publish_queue(agent);
                // Handed back so a client can restore it to its editor,
                // which is what the local dequeue gesture does.
                CommandOutcome::Withdrawn(text)
            }
            QueueOp::Clear => {
                // Session-wide (spec 6.6), so every agent that has
                // something queued gets its own `QueueUpdate`: a client
                // tracks the queues per agent and would otherwise keep
                // showing the ones it was not told about.
                for agent in self.session.core.message_queues.queued_agents() {
                    self.session.core.message_queues.clear(agent);
                    self.publish_queue(agent);
                }
                CommandOutcome::Accepted
            }
        }
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

    /// Write the session's tag sidecar and put the new label on its row.
    ///
    /// The write happens here rather than at the host's surface because this
    /// task holds the session's advisory lock, which is what spec 6.6 means by
    /// materializing like any other command: two writers of one store cannot
    /// interleave on a session's label.
    ///
    /// A tag is display metadata and nothing else, so it appends no log entry
    /// and publishes no `state` frame. The session list is where it surfaces,
    /// which is what the dirty mark is for.
    fn tag(&self, tag: Option<String>) -> Result<CommandOutcome, HostError> {
        self.shared
            .persistence
            .write_tag(self.session.id(), tag.as_deref())
            .map_err(internal)?;
        // After the write, so a sidecar that would not be written leaves the
        // row saying what the store still says.
        self.session.status().tag = tag;
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Write the session's archived sidecar and put the new bit on its row.
    ///
    /// Here rather than at the host's surface for the reason [`Self::tag`]
    /// gives: this task holds the session's advisory lock, which is what
    /// orders two writers of one store on a session's sidecars.
    ///
    /// The bit is display metadata and nothing else, so this appends no log
    /// entry, publishes no `state` frame, and touches nothing about the
    /// session's life: a session working through a turn goes on working,
    /// archived. Nothing else in this driver writes it either, so a prompt to
    /// an archived session leaves it archived.
    fn archive(&self, archived: bool) -> Result<CommandOutcome, HostError> {
        self.shared
            .persistence
            .write_archived(self.session.id(), archived)
            .map_err(internal)?;
        // After the write, so a sidecar that would not be written leaves the
        // row saying what the store still says.
        self.session.status().archived = archived;
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    fn kill_task(&self, task: TaskId) -> Result<CommandOutcome, HostError> {
        if self.session.core.task_registry.status(task).is_none() {
            return Err(HostError::UnknownTask(task));
        }
        // Idempotent for a task that already finished: the registry ignores
        // a kill of a terminal entry.
        self.session.core.task_registry.kill(task);
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    fn spawn(&mut self, agent: AgentId, start: TurnStart) -> Result<CommandOutcome, HostError> {
        // Before the spawn: a sub-agent's turn task can append to its thread
        // as soon as it runs, and a backfill in between has to see the run as
        // live rather than as the finished one it continues.
        self.note_driven(agent, true);
        if !self
            .turns
            .spawn(&self.session.core, &self.shared.config, agent, start)
        {
            self.note_driven(agent, false);
            return Err(HostError::Conflict {
                reason: format!("{agent:?} has no live handle and cannot be prompted"),
            });
        }
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Apply a settings change and synthesize its frames: the notice tagged
    /// with the entry the change appended, then a refreshed `state`.
    ///
    /// A change that did not apply is an error, never an acceptance: the
    /// host staged nothing and has nothing to publish, and a client told
    /// "accepted" would show settings this host never adopted.
    async fn settings(&mut self, change: SettingsChange) -> Result<CommandOutcome, HostError> {
        let SettingsChange {
            agent,
            persist,
            axis,
        } = change;
        let core = &self.session.core;
        let shared = &self.shared;
        let mut transient_confirmation = false;
        let outcome: ConfirmOutcome = match (axis, agent) {
            (SettingsAxis::Thinking(level), AgentId::Main) => {
                crate::settings::confirm_thinking_for_main(
                    level,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
                .into()
            }
            (SettingsAxis::Thinking(level), AgentId::Sub(n)) => {
                let tracked = self.tracked_model();
                crate::settings::confirm_thinking_for_sub(level, n, tracked, core)
                    .await
                    .into()
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
            (SettingsAxis::Model(info), AgentId::Main) => crate::settings::confirm_model_for_main(
                info,
                persist,
                &shared.auth,
                &core.run_config,
                &shared.config,
                &shared.layers,
                core,
            )
            .await
            .into(),
            (SettingsAxis::Model(info), AgentId::Sub(n)) => {
                let speed = {
                    let cfg = core.run_config.lock().expect("run config mutex poisoned");
                    cfg.speed
                };
                crate::settings::confirm_model_for_sub(&info, n, &shared.auth, speed, core)
                    .await
                    .into()
            }
            (SettingsAxis::Speed(speed), AgentId::Main) => crate::settings::confirm_speed_for_main(
                speed,
                persist,
                &shared.auth,
                &core.run_config,
                &shared.config,
                &shared.layers,
                core,
            )
            .await
            .into(),
            (SettingsAxis::Verbosity(verbosity), AgentId::Main) => {
                crate::settings::confirm_verbosity_for_main(
                    verbosity,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
                .into()
            }
            (
                SettingsAxis::ThinkingDisplay(_)
                | SettingsAxis::Speed(_)
                | SettingsAxis::Verbosity(_),
                AgentId::Sub(n),
            ) => {
                // Malformed rather than unservable: these axes are
                // session-wide, so no host could serve this request.
                return Err(HostError::Invalid(format!(
                    "thinking display, speed, and verbosity are session-wide and cannot be set for agent {n}"
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

        if let Some(entry) = &outcome.entry {
            // Ask the projection what a backfill would render rather than
            // restating the wording. The snapshot is taken under the log
            // lock and projected outside it, because the projection walks
            // the whole log.
            let snapshot = self.session.core.log.lock().await.snapshot();
            let notice = snapshot.project_settings_entry(&entry.id);

            // Splice the notice into the stream at its own append
            // position. The confirm released the log lock before
            // returning, so a background sub-agent's append can already
            // sit in the channel carrying a higher position, and
            // publishing the notice on either side of it unconditionally
            // would break the monotone-per-stream guarantee. Holding the
            // log lock for the splice is what stops a further append from
            // arriving mid-way through it.
            // Held through a clone, so the guard does not borrow `self`
            // and the splice below can still publish.
            let log = Arc::clone(&self.session.core.log);
            let guard = log.lock().await;
            let mut buffered = Vec::new();
            while let Ok(tagged) = self.events.try_recv() {
                buffered.push(tagged);
            }
            let mut spliced = false;
            for tagged in buffered {
                if !spliced
                    && tagged
                        .entry
                        .as_ref()
                        .is_some_and(|buffered| buffered.seq > entry.seq)
                {
                    self.publish_notice(agent, entry, notice.clone(), &outcome.notice);
                    spliced = true;
                }
                self.on_event(tagged);
            }
            if !spliced {
                self.publish_notice(agent, entry, notice, &outcome.notice);
            }
            drop(guard);
        }
        for note in outcome.notes {
            // A failed config write or log record is a live-only
            // diagnostic: no entry exists for a backfill to regenerate it
            // from, so it rides an untagged frame.
            self.publish_event(
                None,
                AgentEvent::Warning {
                    agent_id: agent,
                    text: note,
                },
            );
        }
        let settings = settings_of(&self.session.core.run_config);
        self.session.publish_state(&self.shared.fanout, |status| {
            status.settings = settings;
            true
        });
        Ok(CommandOutcome::Accepted)
    }

    /// Publish a settings entry's notice: the projected one tagged with the
    /// entry, or `confirmation` untagged when the entry projects none.
    ///
    /// A settings entry that lands before its thread's first message
    /// projects no notice, so a tagged frame would name something no
    /// backfill can regenerate. Publishing the confirmation untagged is
    /// what keeps the pre-first-prompt settings gesture from going silent
    /// (spec section 5): live clients see it, and it is a transient notice
    /// like any other. The entry's position still moves the high-water
    /// mark, since the entry is on disk either way.
    fn publish_notice(
        &self,
        agent: AgentId,
        entry: &EntryRef,
        projected: Option<AgentEvent>,
        confirmation: &str,
    ) {
        match projected {
            Some(event) => self.publish_event(Some(entry.clone()), event),
            None => {
                {
                    let mut status = self.session.status();
                    status.last_seq = status.last_seq.max(entry.seq);
                    status.note_activity();
                }
                self.publish_event(
                    None,
                    AgentEvent::Notice {
                        agent_id: agent,
                        text: confirmation.to_string(),
                    },
                );
            }
        }
    }

    /// The catalog entry for the session's active model, the validation
    /// fallback a sub-agent thinking change uses when the sub has no staged
    /// bundle of its own.
    fn tracked_model(&self) -> Option<Arc<aj_models::registry::ModelInfo>> {
        let key = {
            let cfg = self
                .session
                .core
                .run_config
                .lock()
                .expect("run config mutex poisoned");
            cfg.model_key.clone()
        };
        self.shared
            .catalog
            .iter()
            .find(|info| info.provider == key.0 && info.id == key.1)
            .cloned()
            .map(Arc::new)
    }

    /// Switch the session's head to `target`, in place.
    ///
    /// Refused while any turn is driven or any background task is live: a
    /// mid-turn switch would let the running turn persist onto the wrong
    /// branch. On success the queues are cleared, the state that belonged to
    /// the branch being left is reset, the agent is reseeded from the new
    /// branch, a fresh epoch is minted and `reset` published.
    async fn head_switch(&mut self, target: HeadTarget) -> Result<CommandOutcome, HostError> {
        let snapshot = self.session.core.task_registry.snapshot();
        let (agents, bash) = running_work_counts(
            self.turns.driven(),
            snapshot.iter().map(|task| (&task.kind, task.status)),
        );
        if agents + bash > 0 {
            return Err(HostError::Conflict {
                reason: format!("{agents} agents and {bash} background tasks are still running"),
            });
        }
        // Everything already on the event stream belongs to the epoch this
        // switch is about to replace. Published afterwards it would carry
        // the new epoch, which no client's epoch filter can reject, and it
        // would name positions in the history the switch just left.
        self.drain_events();

        // Queued messages belong to the branch being left, so they are
        // cleared once the switch is committed to (`set_head` validated
        // the target) but before the epoch changes: published under the old
        // epoch they still reach clients, whereas a frame minted after the
        // switch would be dropped by their epoch filter.
        // Cloned so the guard does not borrow `self`: the queue updates
        // below are published while it is held.
        let log_handle = Arc::clone(&self.session.core.log);
        let transcript = {
            let mut log = log_handle.lock().await;
            // The abandoned branch's buffered non-punctuation entries
            // belong to it, so they must reach disk before the head moves
            // off them.
            log.flush_pending().map_err(internal)?;
            // Resolved here rather than at the caller, under the same lock
            // that moves the head: a parent read outside it could be
            // superseded by an append before the switch lands (spec 6.6).
            let entry = resolve_head_target(&log, &target)?;
            let known = log.contains(&entry);
            log.set_head(entry).map_err(|err| match err {
                // `set_head` refuses an id it does not know and one whose
                // role cannot be a head (a sub-agent entry) with the same
                // error. The first is a 404 and the second a malformed
                // request, so the two are told apart here (spec 6.1).
                //
                // Both quote the entry the request named. For a `before`
                // target that is not the entry `set_head` rejected, and
                // naming the parent would report an id the client never sent.
                aj_session::ConversationError::InvalidHead(_) if known => {
                    HostError::Invalid(match &target {
                        HeadTarget::Entry(entry) => {
                            format!("entry {entry} is not on the user thread and cannot be a head")
                        }
                        HeadTarget::Before(entry) => format!(
                            "cannot branch before entry {entry}: its parent is not on the \
                             user thread"
                        ),
                    })
                }
                // Only a direct target reaches this: `resolve_head_target`
                // has already established that a `before` target's parent is
                // in the log.
                aj_session::ConversationError::InvalidHead(_) => {
                    HostError::UnknownEntry(target.named().to_string())
                }
                other => internal(other),
            })?;
            for agent in self.session.core.message_queues.queued_agents() {
                self.session.core.message_queues.clear(agent);
                self.publish_queue(agent);
            }
            let head = log.head().cloned().expect("set_head installed one");
            let conversation = log.linearize(&head, ThreadFilter::USER);
            repair_interrupted_tool_uses(&mut log, &conversation).map_err(internal)?;
            let head = log.head().cloned().expect("repair keeps a head");
            let conversation = log.linearize(&head, ThreadFilter::USER);
            // The branch records its own settings, so restoring them
            // mirrors what resuming onto this head would do.
            if let Some(restore) = &self.shared.restore {
                let config = self
                    .shared
                    .config
                    .lock()
                    .expect("config mutex poisoned")
                    .clone();
                crate::session_setup::restore_session_settings(
                    &config,
                    &self.session.core.run_config,
                    &conversation.settings(),
                    restore,
                );
            }
            // The epoch and the high-water mark move under the log lock, so
            // an attach that snapshots the log cannot pair the new
            // projection with the old epoch.
            let mut status = self.session.status();
            status.epoch = mint_epoch();
            status.last_seq = log.last_seq();
            status.note_activity();
            // Every run the log names is finished: the refusal above
            // established that nothing is live, and the runs of the
            // abandoned branch are over by definition.
            status.finished_subs = log.sub_agent_ids();
            drop(status);
            conversation.agent_messages()
        };
        self.reset_branch_state();
        self.session.status().settings = settings_of(&self.session.core.run_config);
        // Uncontended: nothing is driven, which the refusal above ensured.
        {
            let mut agent = self.session.core.agent.lock().await;
            agent.reseed_transcript(transcript);
            agent.clear_todo_list();
        }

        // The backfill boundaries live streams filter against are left
        // alone. The log is append-only and `set_head` renumbers nothing,
        // so every position the new epoch mints sits above every boundary
        // handed out under the old one, and no frame of the new history can
        // be mistaken for one the old backfill already covered.
        self.shared.fanout.publish(Frame::Reset {
            session: self.session.id().to_string(),
        });
        self.publish_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Drop the per-branch state a head switch leaves behind.
    ///
    /// A session switch in the local frontend replaces the whole
    /// `SessionCore`, so it cannot carry state across a branch. A head
    /// switch keeps the core (its handles are held by an in-process client
    /// and by the log's open file), so what would otherwise leak is dropped
    /// here. Only valid with nothing running, which the refusal in
    /// [`Self::head_switch`] establishes.
    ///
    /// What deliberately survives: the agent's sub-agent counter, so ids
    /// stay unique across the switch, and its accumulated usage, which
    /// records tokens this process really spent.
    fn reset_branch_state(&self) {
        // The abandoned branch's sub-agents must stop being promptable: a
        // prompt to one would grow that branch under the new epoch, and
        // every attached client would fold it into the new branch's
        // transcript.
        self.session.core.registry.clear();
        // Inert once the registry is cleared (an override is only read at
        // its sub's turn start, and no such turn can start again), dropped
        // so the map cannot outlive what it describes.
        self.session
            .core
            .sub_overrides
            .lock()
            .expect("sub overrides mutex poisoned")
            .clear();
        // Terminal entries and undelivered notices of the branch being
        // left. A client refetches the table after `caught_up` and would
        // otherwise be handed the other branch's tasks.
        self.session.core.task_registry.clear();
    }

    // -- teardown --------------------------------------------------------

    /// Cancel the session's turns and let them wind themselves down, then
    /// quiesce background tasks and flush the log.
    ///
    /// The cancellation tokens rather than `JoinSet::shutdown`: an abort leaves
    /// a transcript with an unfinished message and an unanswered tool call,
    /// while a cancelled turn emits its synthetic aborted `MessageEnd`s. We keep
    /// consuming the event stream while draining so those reach attached clients
    /// before their streams close.
    ///
    /// The flush is what a release has already done (see
    /// [`Self::row_for_release`]), so on that path this one finds nothing
    /// pending. It still takes the log lock, which a long-running reader can
    /// hold, and a release is waited on with the host's session map held.
    async fn wind_down(&mut self) {
        self.draining = true;
        self.turns.cancel_all();
        let grace = tokio::time::sleep(TURN_DRAIN_GRACE);
        tokio::pin!(grace);
        while !self.turns.is_empty() {
            tokio::select! {
                joined = self.turns.join_next() => self.on_join(joined),
                Some(tagged) = self.events.recv() => self.on_event(tagged),
                () = &mut grace => {
                    tracing::warn!(
                        session = self.session.id(),
                        "turns still running after the cancellation grace; aborting them"
                    );
                    self.turns.shutdown().await;
                    break;
                }
            }
        }
        self.drain_events();
        crate::shutdown_background_tasks(&self.session.core.task_registry).await;
        // Buffered non-punctuation entries (the settings records, spawn
        // roots) are lost with the process otherwise: nothing else forces
        // them out.
        if let Err(err) = self.session.core.log.lock().await.flush_pending() {
            tracing::warn!(
                session = self.session.id(),
                "failed to flush the conversation log at teardown: {err}"
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
        // session's materialization for as long. Declining costs one grace. The
        // teardown flush can still meet a held log, which is the same wait one
        // tick later.
        let mut log = self.session.core.log.try_lock().ok()?;
        // A log with no file yet is the one condition `releasable` cannot see.
        // Releasing such a session would not hand it back to the store, which
        // does not know its id, it would drop it.
        if !log.is_durable() {
            return None;
        }
        // Flushed before anything is torn down, so a log that will not flush
        // declines with the session still intact rather than going with a row
        // nobody can trust.
        if let Err(err) = log.flush_pending() {
            tracing::warn!(
                session = self.session.id(),
                "not releasing a session whose log will not flush: {err}"
            );
            return None;
        }
        let file = std::fs::metadata(log.path()).ok()?;
        let file = SessionMetadata::new(
            self.session.id().to_string(),
            file.modified().ok()?.into(),
            file.len(),
        );
        // The stamp is what this driver saw, not what the file says. The flush
        // above can land buffered entries a whole idle grace after the work
        // that produced them, and a row stamped with its own teardown reads to
        // a client as output it has not seen (spec 6.8).
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

fn internal(err: impl std::error::Error + Send + Sync + 'static) -> HostError {
    HostError::Internal(Box::new(err))
}
