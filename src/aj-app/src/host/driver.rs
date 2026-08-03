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

use std::sync::Arc;
use std::time::Duration;

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_models::types::UserContent;
use aj_session::{EntryRef, TaggedEvent, ThreadFilter, repair_interrupted_tool_uses};
use aj_wire::{DurableEvent, Frame};
use chrono::Utc;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::host::live::{LiveSession, Request, settings_of};
use crate::host::{
    Command, CommandOutcome, HostError, HostShared, QueueOp, SettingsAxis, SettingsChange,
    mint_epoch,
};
use crate::session::AgentLifecycle;
use crate::turn::{TurnStart, Turns, running_work_counts};

/// How long a shutdown waits for cancelled turns to wind themselves down
/// before falling back to aborting them.
///
/// The graceful path is what keeps a transcript consistent (a cancelled
/// turn emits its synthetic aborted `MessageEnd`s and error tool results),
/// so we prefer it. A wedged turn must not hang the process, hence the
/// bound.
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
                request = self.requests.recv() => match request {
                    Some(Request::Command { command, reply }) => {
                        let outcome = self.command(command).await;
                        let _ = reply.send(outcome);
                    }
                    // The host asked us to stop, or dropped the session.
                    Some(Request::Shutdown) | None => {
                        self.wind_down().await;
                        return;
                    }
                },

                Some(tagged) = self.events.recv() => self.on_event(tagged),

                joined = self.turns.join_next() => self.on_join(joined),
            }
        }
    }

    // -- events ----------------------------------------------------------

    /// Fold one event of the session's tagged stream: update the
    /// lifecycle, advance the durable high-water mark, publish the frame,
    /// and start a wake if the event earned one.
    fn on_event(&mut self, tagged: TaggedEvent) {
        let TaggedEvent { entry, event } = tagged;
        // The wake trigger is captured before the lifecycle transition and
        // evaluated after, which is the interactive loop's ordering read
        // off its reducer: `AgentEnd` marks the owner idle, and
        // `Turns::spawn_wake` only wakes an idle owner, so evaluating
        // first would find the owner busy and drop the wake. `TaskEnd`
        // wakes unconditionally so a completion notice reaches the model
        // the moment its task finishes; `AgentEnd` only when something is
        // actually queued, because a sub-agent's initial run ends inside
        // its parent's turn and never reaches the join arm.
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
            AgentEvent::AgentEnd { agent_id, .. } => self.lifecycle.mark_idle(*agent_id),
            AgentEvent::CompactionStart { agent_id, .. } => {
                self.lifecycle.mark_compacting(*agent_id);
            }
            AgentEvent::CompactionEnd { agent_id, .. } => {
                self.lifecycle.clear_compacting(*agent_id);
            }
            _ => return,
        }
        self.publish_live_subs();
    }

    /// Republish which sub-agents the host believes are running, which is
    /// what a backfill consults to decide whose bracket stays open.
    fn publish_live_subs(&self) {
        let subs = self
            .lifecycle
            .running_agents()
            .into_iter()
            .filter_map(|agent| match agent {
                AgentId::Sub(n) => Some(n),
                AgentId::Main => None,
            })
            .collect();
        self.session.status().live_subs = subs;
    }

    /// Handle one completed turn: reap, conclude the sub-agent boxes the
    /// reap swept, wake on queued work, and surface the outcome.
    fn on_join(
        &mut self,
        joined: Result<(AgentId, Result<(), TurnError>), tokio::task::JoinError>,
    ) {
        let (id, result) = match joined {
            Ok(joined) => joined,
            Err(err) => {
                // A panicked turn task is fatal for the turn but not for
                // the host: other sessions keep running, and this one has
                // no in-flight state left to corrupt. The join carries no
                // agent id, so the cancel entry can only be cleaned up once
                // nothing is driven at all, otherwise the agent would read
                // busy forever and the session would wedge.
                self.turns.forget_driven_if_idle();
                self.publish_event(
                    None,
                    AgentEvent::Error {
                        agent_id: AgentId::Main,
                        text: format!("agent task panicked: {err}"),
                    },
                );
                self.refresh_state();
                return;
            }
        };
        for idled in self
            .turns
            .reap(&mut self.lifecycle, &self.session.core.task_registry, id)
        {
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
        self.publish_live_subs();
        if !self.draining
            && (self.session.core.task_registry.has_notices(id) || self.session.has_queued(id))
        {
            self.wake(id);
        }
        match result {
            Ok(()) => {}
            Err(TurnError::Aborted) => {
                // The agent already emitted the synthetic aborted
                // `MessageEnd`s, so the transcript is consistent; the
                // notice only confirms the cancel took effect.
                self.publish_event(
                    None,
                    AgentEvent::Notice {
                        agent_id: id,
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
                        agent_id: id,
                        text: format!("{err}"),
                    },
                );
            }
        }
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
    }

    fn wake(&mut self, owner: AgentId) {
        self.turns.spawn_wake(
            owner,
            &self.session.core,
            &self.lifecycle,
            &self.shared.config,
        );
        self.refresh_state();
    }

    // -- publishing ------------------------------------------------------

    fn publish_event(&self, entry: Option<EntryRef>, event: AgentEvent) {
        let (epoch, durability) = {
            let mut status = self.session.status();
            if let Some(entry) = &entry {
                debug_assert!(
                    entry.seq > status.last_seq,
                    "durable seqs must be monotone per session: {} after {}",
                    entry.seq,
                    status.last_seq
                );
                status.last_seq = entry.seq;
                status.last_activity = Utc::now();
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
        let changed = {
            let mut status = self.session.status();
            let changed = status.working != working;
            status.working = working;
            changed
        };
        if changed {
            self.publish_state();
        }
    }

    fn publish_state(&self) {
        let frame = {
            let status = self.session.status();
            Frame::State {
                session: self.session.id().to_string(),
                epoch: status.epoch.clone(),
                working: status.working,
                settings: status.settings.clone(),
                last_seq: status.last_seq,
            }
        };
        self.shared.fanout.publish(frame);
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
            Command::Head { entry } => self.head_switch(entry).await,
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

    fn cancel(&mut self, agent: AgentId) -> CommandOutcome {
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

    fn queue_op(&mut self, op: QueueOp) -> CommandOutcome {
        match op {
            QueueOp::Remove { agent } => {
                let text = self.session.core.message_queues.take_pending(agent);
                self.publish_queue(agent);
                // Handed back so a client can restore it to its editor,
                // which is what the local dequeue gesture does.
                CommandOutcome::Withdrawn(text)
            }
            QueueOp::Clear { agent } => {
                self.session.core.message_queues.clear(agent);
                self.publish_queue(agent);
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

    fn kill_task(&mut self, task: TaskId) -> Result<CommandOutcome, HostError> {
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
        if !self
            .turns
            .spawn(&self.session.core, &self.shared.config, agent, start)
        {
            return Err(HostError::Conflict {
                reason: format!("{agent:?} has no live handle and cannot be prompted"),
            });
        }
        self.refresh_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    /// Apply a settings change and synthesize its frames: the projected
    /// notice tagged with the entry the change appended, then a refreshed
    /// `state`.
    async fn settings(&mut self, change: SettingsChange) -> Result<CommandOutcome, HostError> {
        let SettingsChange {
            agent,
            persist,
            axis,
        } = change;
        let core = &self.session.core;
        let shared = &self.shared;
        let (entry, notes) = match (axis, agent) {
            (SettingsAxis::Thinking(level), AgentId::Main) => {
                let confirm = crate::settings::confirm_thinking_for_main(
                    level,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await;
                (confirm.entry, confirm.notes)
            }
            (SettingsAxis::Thinking(level), AgentId::Sub(n)) => {
                let tracked = self.tracked_model();
                let confirm =
                    crate::settings::confirm_thinking_for_sub(level, n, tracked, core).await;
                (confirm.entry, confirm.notes)
            }
            (SettingsAxis::Model(info), AgentId::Main) => {
                let confirm = crate::settings::confirm_model_for_main(
                    info,
                    persist,
                    &shared.auth,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await;
                (confirm.entry, confirm.notes)
            }
            (SettingsAxis::Model(info), AgentId::Sub(n)) => {
                let speed = {
                    let cfg = core.run_config.lock().expect("run config mutex poisoned");
                    cfg.speed
                };
                let confirm =
                    crate::settings::confirm_model_for_sub(&info, n, &shared.auth, speed, core)
                        .await;
                (confirm.entry, confirm.notes)
            }
            (SettingsAxis::Speed(speed), AgentId::Main) => {
                match crate::settings::confirm_speed_for_main(
                    speed,
                    persist,
                    &shared.auth,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await
                {
                    crate::settings::SpeedConfirm::Applied { notes, entry, .. } => (entry, notes),
                    crate::settings::SpeedConfirm::Failed { notice, .. } => {
                        return Err(HostError::Invalid(notice));
                    }
                }
            }
            (SettingsAxis::Verbosity(verbosity), AgentId::Main) => {
                let confirm = crate::settings::confirm_verbosity_for_main(
                    verbosity,
                    persist,
                    &core.run_config,
                    &shared.config,
                    &shared.layers,
                    core,
                )
                .await;
                (confirm.entry, confirm.notes)
            }
            (SettingsAxis::Speed(_) | SettingsAxis::Verbosity(_), AgentId::Sub(n)) => {
                return Err(HostError::Invalid(format!(
                    "speed and verbosity are session-wide and cannot be set for agent {n}"
                )));
            }
        };

        if let Some(entry) = entry {
            // Ask the projection what a backfill would render rather than
            // restating the wording, and publish nothing when it renders
            // nothing: a settings entry before its thread's first message
            // projects no notice, and a live frame no backfill regenerates
            // would leave a joiner permanently out of step. The snapshot is
            // taken under the log lock and projected outside it, because
            // the projection walks the whole log.
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
                    self.publish_notice(&entry, notice.clone());
                    spliced = true;
                }
                self.on_event(tagged);
            }
            if !spliced {
                self.publish_notice(&entry, notice);
            }
            drop(guard);
        }
        for note in notes {
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
        self.session.status().settings = settings_of(&self.session.core.run_config);
        self.publish_state();
        Ok(CommandOutcome::Accepted)
    }

    /// Publish a settings entry's projected notice, or, when it projects
    /// none, just account for the entry it appended.
    fn publish_notice(&mut self, entry: &EntryRef, notice: Option<AgentEvent>) {
        match notice {
            Some(event) => self.publish_event(Some(entry.clone()), event),
            None => {
                let mut status = self.session.status();
                status.last_seq = status.last_seq.max(entry.seq);
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

    /// Switch the session's head to `entry`, in place.
    ///
    /// Refused while any turn is driven or any background task is live: a
    /// mid-turn switch would let the running turn persist onto the wrong
    /// branch. On success the queues are cleared, the agent is reseeded
    /// from the new branch, a fresh epoch is minted and `reset` published.
    async fn head_switch(&mut self, entry: String) -> Result<CommandOutcome, HostError> {
        let (agents, bash) = running_work_counts(
            self.turns.driven(),
            &self.session.core.task_registry.snapshot(),
        );
        if agents + bash > 0 {
            return Err(HostError::Conflict {
                reason: format!("{agents} agents and {bash} background tasks are still running"),
            });
        }

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
            log.set_head(entry.clone()).map_err(|err| match err {
                aj_session::ConversationError::InvalidHead(_) => HostError::UnknownEntry(entry),
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
            status.last_activity = Utc::now();
            drop(status);
            conversation.agent_messages()
        };
        self.session.status().settings = settings_of(&self.session.core.run_config);
        // Uncontended: nothing is driven, which the refusal above ensured.
        self.session
            .core
            .agent
            .lock()
            .await
            .reseed_transcript(transcript);

        self.shared.fanout.reset_boundaries(self.session.id());
        self.shared.fanout.publish(Frame::Reset {
            session: self.session.id().to_string(),
        });
        self.publish_state();
        self.shared.fanout.mark_list_dirty();
        Ok(CommandOutcome::Accepted)
    }

    // -- teardown --------------------------------------------------------

    /// Cancel the session's turns and let them wind themselves down, then
    /// quiesce background tasks and flush the log.
    ///
    /// The cancellation tokens rather than `JoinSet::shutdown`: an abort
    /// leaves a transcript with an unfinished message and an unanswered
    /// tool call, while a cancelled turn emits its synthetic aborted
    /// `MessageEnd`s. We keep consuming the event stream while draining so
    /// those reach attached clients before their streams close.
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
