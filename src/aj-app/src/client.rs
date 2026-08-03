//! Client-side fold of one session's frame stream (spec 6.5).
//!
//! [`SessionClient`] is the consumer contract a session host has to
//! satisfy. It turns the frames of one session into reducer calls and
//! keeps the state no transcript carries: the session's epoch, the two
//! cursor positions, the lifecycle sets, and the settings, queue and
//! background-task snapshots a remote frontend cannot read off live
//! handles.
//!
//! [`ChatState`] stays outside the client. A frontend can hold it behind
//! widgets it cannot repoint, so the fold takes it as a parameter.
//!
//! Host-level frames are deliberately not this type's business. `list`,
//! `vms` and `heartbeat` carry no `session` field, so they belong to
//! whatever owns the session directory and the connection, not to one
//! session's fold. Unknown frame kinds never arrive here at all:
//! `aj-wire` decodes them into `DecodedFrame::Unknown`, which an endpoint
//! client discards (spec 6.10, only a gateway forwards them).
//!
//! Nothing in the fold can fail. A frame is either applied or dropped, so
//! no operation here returns a `Result`.

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_wire::{AgentQueue, Cursor, DecodedAgentEvent, Frame, QueueState, TaskTable};

use crate::chat::{ChatState, Redraw, reduce};
use crate::session::AgentLifecycle;

/// Where the client stands relative to an attach block (spec 6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attach {
    /// No attach outstanding. Every frame is a live one.
    Live,
    /// The client asked for an attach. The next `state` frame for this
    /// session opens the block it asked for.
    Requested,
    /// Inside the block. Durable frames apply without advancing the
    /// cursor, because the block is atomic: its `caught_up` commits it.
    Applying,
}

/// Client-side bookkeeping for one attached session.
///
/// Frames are applied through [`SessionClient::apply`], which folds them
/// into a caller-owned [`ChatState`]. Everything else here is the state a
/// remote frontend needs and cannot derive from the transcript.
#[derive(Debug)]
pub struct SessionClient {
    session: String,
    lifecycle: AgentLifecycle,
    /// The epoch adopted from the last attach block, `None` until the
    /// first one arrives. Session-scoped frames from any other epoch are
    /// dropped.
    epoch: Option<String>,
    /// The seq offered on re-attach. It lags `applied` by one durable
    /// frame, because a log entry can project trailing untagged events (an
    /// assistant entry's `UsageUpdate`, a tool-result entry's bracket) and
    /// a drop in between would otherwise make the client claim an entry it
    /// only half applied.
    committed: Option<u64>,
    /// The durable high-water mark the cursor invariant compares against.
    applied: Option<u64>,
    attach: Attach,
    settings: Option<AgentSettings>,
    working: bool,
    queue: QueueState,
    tasks: TaskTable,
    needs_task_refetch: bool,
    needs_queue_refetch: bool,
    needs_reattach: bool,
}

impl SessionClient {
    /// A client that has attached nothing yet.
    pub fn new(session: String) -> Self {
        Self {
            session,
            lifecycle: AgentLifecycle::default(),
            epoch: None,
            committed: None,
            applied: None,
            attach: Attach::Live,
            settings: None,
            working: false,
            queue: QueueState::default(),
            tasks: TaskTable::default(),
            needs_task_refetch: false,
            needs_queue_refetch: false,
            needs_reattach: false,
        }
    }

    /// The session these frames belong to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Arm the client for the attach block it is about to request.
    ///
    /// The client is the one that asks for an attach, so the request is
    /// what identifies the block's opening `state` frame. Nothing in the
    /// frame itself can: the host re-emits `state` whenever any of it
    /// changes (spec 6.3), and an on-change re-emission must neither adopt
    /// an epoch nor quiesce.
    ///
    /// Contract: arm only once the attach has been served, from what the
    /// server reports it attached (`Attachment::attached` in process). An
    /// arm for a block that never arrives is what makes the next on-change
    /// `state` frame look like one: the fold would quiesce, enter the block
    /// phase, and stop advancing its cursor until a `caught_up` that never
    /// comes.
    ///
    /// Every session the server did attach has to be armed. An unarmed
    /// session's block folds as live frames instead: its `caught_up` is
    /// ignored and its durable frames advance the cursor in projection
    /// order, which is not seq order.
    ///
    /// Arming also satisfies [`Self::needs_reattach`].
    pub fn expect_attach(&mut self) {
        self.attach = Attach::Requested;
        self.needs_reattach = false;
    }

    /// Fold one frame, updating `chat` and the client's own bookkeeping.
    ///
    /// The frame is consumed so its payloads move into the model instead
    /// of being cloned, as [`reduce`] does. Frames for another session,
    /// from another epoch, or below the cursor are dropped.
    pub fn apply(&mut self, chat: &mut ChatState, frame: Frame) -> Redraw {
        match frame {
            Frame::Event {
                session,
                epoch,
                durability,
                event,
            } => {
                if !self.is_ours(&session) || !self.epoch_matches(&epoch) {
                    return Redraw(false);
                }
                if let Some(durability) = &durability {
                    // Cursor invariant: within an epoch a durable frame at
                    // or below the high-water mark is a duplicate. This is
                    // de-duplication, not the correctness mechanism, which
                    // is idempotent application: the invariant cannot
                    // protect an entry's trailing untagged events, since
                    // those carry no seq to compare.
                    if self
                        .applied
                        .is_some_and(|applied| durability.seq <= applied)
                    {
                        return Redraw(false);
                    }
                    // Inside an attach block the cursor does not move per
                    // frame. Spec 6.5 makes the block atomic and its
                    // `caught_up` commits it once, which is what lets the
                    // projection order its events by thread bracketing
                    // rather than by seq. Today's projection happens to tag
                    // entries in increasing seq order, so this guard changes
                    // nothing, but the fold must not come to depend on that:
                    // a projection free to interleave (a thread-scoped
                    // backfill would) plus a per-frame advance would drop
                    // every frame that came out below an earlier one.
                    if self.attach != Attach::Applying {
                        self.committed = self.applied;
                        self.applied = Some(durability.seq);
                    }
                }
                // An unknown event type is skipped before the reducer, but
                // its envelope applied above: dropping it without
                // advancing the cursor would make every reconnect refetch
                // an event this client will never understand (spec 6.10).
                let DecodedAgentEvent::Known(known) = event else {
                    return Redraw(false);
                };
                let event = known.into_value();
                if let AgentEvent::QueueUpdate {
                    agent_id,
                    steering,
                    follow_up,
                } = &event
                {
                    // The reducer treats this event as a pure redraw ping
                    // and drops the payload, because the local view
                    // re-reads the live queues at draw time. A remote
                    // client has no such handle, so the snapshot is kept
                    // here.
                    self.note_queue(*agent_id, steering.clone(), follow_up.clone());
                }
                reduce(
                    chat,
                    &mut self.lifecycle,
                    event,
                    durability.as_ref().map(|durability| &durability.entry_id),
                )
            }
            Frame::State {
                session,
                epoch,
                working,
                settings,
                ..
            } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                if self.attach == Attach::Requested {
                    self.open_attach_block(chat, epoch, working);
                } else if !self.epoch_matches(&epoch) {
                    return Redraw(false);
                }
                // The host is authoritative for both of these, at every
                // emission: neither is derivable from projected events.
                self.settings = Some(settings);
                self.working = working;
                Redraw(true)
            }
            Frame::CaughtUp {
                session,
                epoch,
                last_seq,
            } => {
                // Only the block this client asked for ends here. A
                // `caught_up` outside one names a position whose entries the
                // client never applied, and committing it would silently
                // skip them.
                if !self.is_ours(&session)
                    || !self.epoch_matches(&epoch)
                    || self.attach != Attach::Applying
                {
                    return Redraw(false);
                }
                // The block was applied whole, so both positions rebase on
                // its high-water mark. Leaving `applied` behind would let
                // the next live durable frame commit a seq whose block tail
                // this client has not seen.
                self.applied = Some(last_seq);
                self.committed = Some(last_seq);
                self.attach = Attach::Live;
                // Neither task events nor queue updates are replayable, so
                // both tables have to come from their reads (spec 6.7).
                self.needs_task_refetch = true;
                self.needs_queue_refetch = true;
                Redraw(true)
            }
            Frame::Reset { session } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                // Continuity is broken, but the cursor stays valid to
                // offer: the server decides whether it can resume from it.
                // An armed attach stays armed, so a `reset` that overtakes
                // the block the client already asked for cannot disarm it.
                self.needs_reattach = true;
                Redraw(true)
            }
            Frame::List { .. } | Frame::Heartbeat | Frame::Vms { .. } => Redraw(false),
        }
    }

    /// Fold an event the client raised itself, outside the stream.
    ///
    /// A frontend still has notices of its own: a config diagnostic, the
    /// outcome of a login, a refused gesture. They carry no envelope, so
    /// the epoch and cursor rules have nothing to say about them, and no
    /// durable identity, so they are appended rather than reconciled. They
    /// go through this instead of straight to [`reduce`] so they share the
    /// client's lifecycle sets, which is what keeps the two from drifting
    /// apart.
    ///
    /// Only for events with no host behind them. An event the host
    /// published belongs in [`Self::apply`], envelope and all.
    pub fn apply_local(&mut self, chat: &mut ChatState, event: AgentEvent) -> Redraw {
        reduce(chat, &mut self.lifecycle, event, None)
    }

    /// The cursor to offer on re-attach, absent until a durable position
    /// under a known epoch has been committed.
    pub fn cursor(&self) -> Option<Cursor> {
        Some(Cursor {
            epoch: self.epoch.clone()?,
            seq: self.committed?,
        })
    }

    /// The lifecycle sets the fold maintains: which agents are running,
    /// which are compacting.
    pub fn lifecycle(&self) -> &AgentLifecycle {
        &self.lifecycle
    }

    /// The active settings, as of the last `state` frame.
    pub fn settings(&self) -> Option<&AgentSettings> {
        self.settings.as_ref()
    }

    /// Whether the host reported a turn in flight, as of the last `state`
    /// frame. The lifecycle sets are the authority for spinners, this is
    /// the host's own flag.
    pub fn working(&self) -> bool {
        self.working
    }

    /// Pending steering and follow-up messages, from `QueueUpdate` frames
    /// and [`Self::set_queue`].
    pub fn queue(&self) -> &QueueState {
        &self.queue
    }

    /// Replace the queue snapshot from the queue read (spec 6.7), which is
    /// how a mid-session joiner learns about messages queued before it
    /// attached. Clears [`Self::needs_queue_refetch`].
    pub fn set_queue(&mut self, queue: QueueState) {
        self.queue = queue;
        self.needs_queue_refetch = false;
    }

    /// The background-task table, from the tasks read.
    pub fn tasks(&self) -> &TaskTable {
        &self.tasks
    }

    /// Replace the task table from the tasks read, clearing
    /// [`Self::needs_task_refetch`].
    pub fn set_tasks(&mut self, tasks: TaskTable) {
        self.tasks = tasks;
        self.needs_task_refetch = false;
    }

    /// Whether the task table is stale and the caller owes the tasks read.
    ///
    /// Set by every `caught_up`, because task events are not replayable
    /// and a backfill can carry none of them.
    pub fn needs_task_refetch(&self) -> bool {
        self.needs_task_refetch
    }

    /// Whether the queue snapshot is stale and the caller owes the queue
    /// read.
    ///
    /// Set by every `caught_up`, for the same reason as the task table:
    /// `QueueUpdate` is reliable-transient, so a backfill regenerates none
    /// of it and a joiner would show no pending messages at all.
    pub fn needs_queue_refetch(&self) -> bool {
        self.needs_queue_refetch
    }

    /// Whether continuity was broken and the caller owes a re-attach.
    pub fn needs_reattach(&self) -> bool {
        self.needs_reattach
    }

    /// Adopt the epoch of the attach block this client asked for, and
    /// prepare `chat` for the backfill that follows.
    fn open_attach_block(&mut self, chat: &mut ChatState, epoch: String, working: bool) {
        match &self.epoch {
            Some(current) if *current == epoch => {
                // A re-attach into the epoch we already applied under: the
                // suffix re-projects entries we saw only partly live, so
                // the transient detail painted around them goes first.
                chat.quiesce(&mut self.lifecycle);
            }
            Some(_) => {
                // A different epoch. Our seqs, and everything we derived
                // from them, describe a history this session no longer
                // has, so the fold restarts from the full backfill.
                chat.reset(&mut self.lifecycle);
                self.committed = None;
                self.applied = None;
            }
            // A first attach has nothing of its own to quiesce.
            None => {}
        }
        self.epoch = Some(epoch);
        self.attach = Attach::Applying;
        self.seed_lifecycle(working);
    }

    /// Seed the main agent's running mark from the block's `working` flag.
    ///
    /// A client whose stream died before an `AgentEnd` would otherwise
    /// spin forever: no projected event carries a lifecycle bracket. After
    /// the block, live lifecycle events are authoritative again.
    ///
    /// Scoped to `Main`, because `working` says nothing about sub-agents
    /// (spec 6.3). Clearing their marks here would undercount the running
    /// agents in the footer and stop a background sub's spinner after every
    /// re-attach, while its box still reads `Running`. A sub whose
    /// `AgentEnd` this client missed is cleared by the host's
    /// post-`caught_up` conclusion sweep, which is the designed mechanism.
    fn seed_lifecycle(&mut self, working: bool) {
        if working {
            self.lifecycle.mark_running(AgentId::Main);
        } else {
            self.lifecycle.mark_idle(AgentId::Main);
        }
    }

    fn note_queue(
        &mut self,
        agent_id: AgentId,
        steering: Vec<aj_agent::message::AgentMessage>,
        follow_up: Vec<aj_agent::message::AgentMessage>,
    ) {
        let updated = AgentQueue {
            agent_id,
            steering,
            follow_up,
        };
        match self
            .queue
            .queues
            .iter_mut()
            .find(|queue| queue.agent_id == agent_id)
        {
            // The event carries a full snapshot of both queues, so it
            // replaces the agent's entry rather than merging into it.
            Some(existing) => *existing = updated,
            None => self.queue.queues.push(updated),
        }
    }

    fn is_ours(&self, session: &str) -> bool {
        session == self.session
    }

    fn epoch_matches(&self, epoch: &str) -> bool {
        self.epoch.as_deref() == Some(epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use aj_agent::events::CompactionReason;
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::{TaskKind, TaskStatus, ToolDetails};
    use aj_models::streaming::AssistantMessageEvent;
    use aj_models::types::{
        AssistantContent, AssistantMessage as WireAssistantMessage, Message, StopReason,
        TextContent, Usage, UserMessage,
    };
    use aj_wire::TaskSummary;
    use chrono::Utc;

    use crate::chat::{EntryKind, NoticeLevel};

    const SESSION: &str = "session-1";
    const EPOCH: &str = "epoch-1";

    fn settings() -> AgentSettings {
        AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    fn chat() -> ChatState {
        ChatState::new(settings(), 200_000, Arc::new(Vec::new()))
    }

    /// A client that has attached an empty session: the opening `state`,
    /// an empty backfill, and `caught_up` at seq 0. Every remote client
    /// starts here, so the unit tests do too.
    fn attached() -> (SessionClient, ChatState) {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        (client, chat)
    }

    fn state(epoch: &str, working: bool) -> Frame {
        state_with(epoch, working, settings())
    }

    fn state_with(epoch: &str, working: bool, settings: AgentSettings) -> Frame {
        Frame::State {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            working,
            settings,
            last_seq: 0,
        }
    }

    fn caught_up(epoch: &str, last_seq: u64) -> Frame {
        Frame::CaughtUp {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            last_seq,
        }
    }

    fn durable(epoch: &str, seq: u64, entry_id: &str, event: AgentEvent) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            durability: Some(aj_wire::DurableEvent {
                seq,
                entry_id: entry_id.to_string(),
            }),
            event: event.into(),
        }
    }

    fn live(epoch: &str, event: AgentEvent) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            durability: None,
            event: event.into(),
        }
    }

    /// A durable event with a body: the projected settings notice, which
    /// takes its whole identity from the frame's `entry_id`.
    fn notice(text: &str) -> AgentEvent {
        AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: text.to_string(),
        }
    }

    fn compaction_start() -> AgentEvent {
        AgentEvent::CompactionStart {
            agent_id: AgentId::Main,
            reason: CompactionReason::Manual,
        }
    }

    /// A painting `MessageUpdate`, which is what opens an unfinalized
    /// streaming row (the thing quiesce drops).
    fn streaming_text(text: &str) -> AgentEvent {
        let partial = WireAssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        };
        AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(partial.clone())),
            event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.to_string(),
                partial,
            },
        }
    }

    fn task_output(task_id: usize) -> AgentEvent {
        AgentEvent::TaskOutput {
            agent_id: AgentId::Main,
            task_id,
            call_id: "call-1".into(),
            partial: ToolDetails::Text {
                summary: "running".into(),
                body: String::new(),
            },
        }
    }

    fn task_summary(id: usize) -> TaskSummary {
        TaskSummary {
            id,
            owner: AgentId::Main,
            call_id: "call-1".into(),
            kind: TaskKind::Bash {
                command: "sleep 1".into(),
            },
            label: "sleep 1".into(),
            status: TaskStatus::Running,
            started_at: Utc::now(),
        }
    }

    fn queued(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    /// The Main transcript's notice rows, which is what the durable
    /// `Notice` frames above land as.
    fn notices(chat: &ChatState) -> Vec<String> {
        chat.transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Notice(notice) => {
                            assert_eq!(notice.level, NoticeLevel::Info);
                            Some(notice.text.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether an unfinalized streaming row is open in the Main
    /// transcript.
    fn streaming(chat: &ChatState) -> bool {
        chat.transcript(AgentId::Main).is_some_and(|transcript| {
            transcript
                .entries()
                .iter()
                .any(|entry| matches!(&entry.kind, EntryKind::Assistant(a) if !a.finalized))
        })
    }

    #[test]
    fn a_frame_for_another_session_is_ignored() {
        let (mut client, mut chat) = attached();
        let mut frame = durable(EPOCH, 4, "entry-4", notice("elsewhere"));
        if let Frame::Event { session, .. } = &mut frame {
            *session = "session-2".to_string();
        }

        assert!(!client.apply(&mut chat, frame).0);
        assert!(notices(&chat).is_empty());
        assert_eq!(client.cursor().map(|cursor| cursor.seq), Some(0));
    }

    #[test]
    fn host_level_frames_are_not_one_session_s_business() {
        let (mut client, mut chat) = attached();
        let before = client.cursor();

        for frame in [
            Frame::Heartbeat,
            Frame::List {
                sessions: Vec::new(),
            },
            Frame::Vms { vms: Vec::new() },
        ] {
            assert!(!client.apply(&mut chat, frame).0);
        }

        assert_eq!(client.cursor(), before);
    }

    #[test]
    fn an_attach_block_under_a_new_epoch_replaces_earlier_state() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            durable(EPOCH, 3, "entry-3", notice("under the old epoch")),
        );
        assert_eq!(notices(&chat), vec!["under the old epoch"]);

        // The host restarted, so it serves the whole history under a fresh
        // epoch. Entry 1 is below the old high-water mark and would be
        // dropped as a duplicate if adoption had not reset the cursor.
        client.expect_attach();
        let _ = client.apply(&mut chat, state("epoch-2", false));
        let _ = client.apply(
            &mut chat,
            durable("epoch-2", 1, "entry-1", notice("under the new epoch")),
        );
        let _ = client.apply(&mut chat, caught_up("epoch-2", 1));

        assert_eq!(
            notices(&chat),
            vec!["under the new epoch"],
            "the old epoch's rows are gone",
        );
        assert_eq!(
            client.cursor(),
            Some(Cursor {
                epoch: "epoch-2".to_string(),
                seq: 1,
            }),
        );
    }

    #[test]
    fn an_on_change_state_re_emission_keeps_state_and_cursor() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("one")));
        let _ = client.apply(&mut chat, durable(EPOCH, 4, "entry-4", notice("two")));
        let cursor = client.cursor();
        assert_eq!(
            cursor.as_ref().map(|cursor| cursor.seq),
            Some(3),
            "committed lags applied",
        );

        // The host re-emits `state` whenever any of it changes. The client
        // asked for no attach, so nothing is adopted and nothing resets.
        let mut changed = settings();
        changed.model_id = "other-model".to_string();
        let _ = client.apply(&mut chat, state_with(EPOCH, true, changed.clone()));

        assert_eq!(notices(&chat), vec!["one", "two"]);
        assert_eq!(client.cursor(), cursor);
        assert_eq!(client.settings(), Some(&changed));
        assert!(client.working());
    }

    #[test]
    fn stale_epoch_frames_are_dropped_outside_an_attach_block() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("ours")));
        client.set_tasks(TaskTable::default());
        let cursor = client.cursor();

        let mut abandoned = settings();
        abandoned.model_id = "abandoned-branch".to_string();
        let _ = client.apply(
            &mut chat,
            durable("epoch-0", 9, "entry-9", notice("from an abandoned branch")),
        );
        let _ = client.apply(&mut chat, state_with("epoch-0", true, abandoned));
        let _ = client.apply(&mut chat, caught_up("epoch-0", 99));

        assert_eq!(notices(&chat), vec!["ours"]);
        assert_eq!(client.cursor(), cursor);
        assert_eq!(client.settings(), Some(&settings()));
        assert!(!client.working());
        assert!(
            !client.needs_task_refetch(),
            "a dropped caught_up ends no block",
        );
    }

    #[test]
    fn a_durable_frame_at_or_below_applied_is_dropped() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("first")));

        // The same entry re-delivered. Dropped, so the row keeps the text
        // it was applied with instead of being updated in place.
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("second")));
        // And an older entry.
        let _ = client.apply(&mut chat, durable(EPOCH, 4, "entry-4", notice("earlier")));

        assert_eq!(notices(&chat), vec!["first"]);
    }

    #[test]
    fn the_committed_cursor_lags_applied_by_one_durable_frame() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("five")));
        let _ = client.apply(&mut chat, durable(EPOCH, 9, "entry-9", notice("nine")));

        // Entry 9's trailing untagged events may still be in flight, so
        // the client claims only entry 5.
        assert_eq!(client.cursor().map(|cursor| cursor.seq), Some(5));

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 12));

        assert_eq!(
            client.cursor().map(|cursor| cursor.seq),
            Some(12),
            "a block commits whole",
        );
    }

    #[test]
    fn an_unknown_durable_event_advances_the_cursor() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("five")));

        // Decoded through the wire boundary, which is the only place an
        // unknown event type comes from.
        let frame: Frame = serde_json::from_str(&format!(
            r#"{{"kind":"event","session":"{SESSION}","epoch":"{EPOCH}","seq":9,
                 "entry_id":"entry-9","event":{{"type":"telepathy","thought":"hello"}}}}"#
        ))
        .expect("the frame decodes with an unknown event type");

        assert!(
            !client.apply(&mut chat, frame).0,
            "an unknown event renders nothing",
        );
        assert_eq!(notices(&chat), vec!["five"], "it never reaches the reducer",);
        assert_eq!(client.cursor().map(|cursor| cursor.seq), Some(5));

        // Its envelope applied, so entry 9 is the high-water mark now and
        // a reconnect will not be served it again.
        let _ = client.apply(&mut chat, durable(EPOCH, 9, "entry-9", notice("nine")));
        assert_eq!(notices(&chat), vec!["five"]);
    }

    #[test]
    fn a_re_attach_quiesces_once_before_the_backfill() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, live(EPOCH, streaming_text("half a sen")));
        let _ = client.apply(&mut chat, live(EPOCH, compaction_start()));
        assert!(streaming(&chat), "the fold has transient detail");
        assert!(client.lifecycle().is_compacting(AgentId::Main));

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));

        assert!(!streaming(&chat), "the block's opening state quiesced");
        assert!(!client.lifecycle().is_compacting(AgentId::Main));

        // Nothing quiesces again inside the block, so what the block
        // applies survives to the end of it. The compaction mark is the
        // witness because quiesce clears it and one event restores it.
        let _ = client.apply(&mut chat, live(EPOCH, compaction_start()));
        let _ = client.apply(
            &mut chat,
            durable(EPOCH, 1, "entry-1", notice("backfilled")),
        );
        let _ = client.apply(&mut chat, caught_up(EPOCH, 1));

        assert!(
            client.lifecycle().is_compacting(AgentId::Main),
            "the block quiesced once, before its frames",
        );
        assert_eq!(notices(&chat), vec!["backfilled"]);

        // An on-change `state` re-emission is not an attach block.
        let _ = client.apply(&mut chat, live(EPOCH, streaming_text("more")));
        let _ = client.apply(&mut chat, state(EPOCH, true));
        assert!(
            streaming(&chat),
            "a re-emitted state frame does not quiesce"
        );
    }

    #[test]
    fn a_first_attach_does_not_quiesce() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        // Transient state this client did not build. Having adopted no
        // epoch, a first attach has nothing of its own to quiesce and
        // leaves it alone.
        let mut local = AgentLifecycle::default();
        let _ = reduce(&mut chat, &mut local, streaming_text("local"), None);

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));

        assert!(streaming(&chat));
    }

    #[test]
    fn state_working_seeds_the_spinner() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Main,
                },
            ),
        );
        assert!(client.lifecycle().is_running(AgentId::Main));

        // The stream died before the turn's `AgentEnd`, and no projected
        // event carries a lifecycle bracket, so without the seed this
        // spinner would run forever.
        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        assert!(!client.lifecycle().is_running(AgentId::Main));
        assert!(!client.working());
    }

    #[test]
    fn state_working_spins_for_a_joiner_mid_turn() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, true));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 3));

        assert!(client.lifecycle().is_running(AgentId::Main));
        assert!(client.working());
    }

    #[test]
    fn caught_up_flags_a_task_refetch_that_set_tasks_clears() {
        let (mut client, mut chat) = attached();
        assert!(
            client.needs_task_refetch(),
            "task events are not replayable",
        );

        client.set_tasks(TaskTable {
            tasks: vec![task_summary(7)],
        });

        assert!(!client.needs_task_refetch());
        assert_eq!(client.tasks().tasks.len(), 1);

        // In the interim between `caught_up` and the read landing, a
        // snapshot for a task the client does not know is inert: the
        // reducer freezes output for an untracked task, so the client needs
        // no filter of its own.
        assert!(!client.apply(&mut chat, live(EPOCH, task_output(9))).0);
        assert!(chat.tasks().is_empty());
    }

    /// The queue read is owed after every block too: `QueueUpdate` is
    /// reliable-transient, so a backfill regenerates none of it and a joiner
    /// would show no pending messages at all.
    #[test]
    fn caught_up_flags_a_queue_refetch_that_set_queue_clears() {
        let (mut client, _chat) = attached();
        assert!(client.needs_queue_refetch());
        assert!(client.queue().queues.is_empty());

        client.set_queue(QueueState {
            queues: vec![AgentQueue {
                agent_id: AgentId::Main,
                steering: Vec::new(),
                follow_up: vec![queued("from the read")],
            }],
        });

        assert!(!client.needs_queue_refetch());
        assert_eq!(client.queue().queues.len(), 1);
    }

    /// A re-attach seeds the main agent's mark and leaves the sub-agents'
    /// alone: `working` says nothing about them (spec 6.3), and clearing a
    /// running background sub's mark would stop its spinner and undercount
    /// the footer's running agents until it ends.
    #[test]
    fn a_re_attach_seed_leaves_a_running_sub_agent_marked() {
        let (mut client, mut chat) = attached();
        for agent in [AgentId::Main, AgentId::Sub(1)] {
            let _ = client.apply(
                &mut chat,
                live(EPOCH, AgentEvent::AgentStart { agent_id: agent }),
            );
        }
        assert!(client.lifecycle().is_running(AgentId::Sub(1)));

        // The main turn ended in the gap, so the block reports idle. The
        // background sub is still going.
        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        assert!(!client.lifecycle().is_running(AgentId::Main));
        assert!(
            client.lifecycle().is_running(AgentId::Sub(1)),
            "the sub keeps its mark until its own AgentEnd or the host's sweep",
        );

        // And the host's conclusion sweep is what clears it.
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    messages: Vec::new(),
                },
            ),
        );
        assert!(!client.lifecycle().is_running(AgentId::Sub(1)));
    }

    /// An attach that was never served must arm nothing: the host's next
    /// on-change `state` frame would otherwise be mistaken for a block, and
    /// the fold would quiesce and stop advancing its cursor until a
    /// `caught_up` that never comes.
    #[test]
    fn an_unarmed_client_treats_state_frames_as_live() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("one")));
        let _ = client.apply(&mut chat, live(EPOCH, streaming_text("half a sen")));

        // The attach was refused, so nothing was armed.
        let _ = client.apply(&mut chat, state(EPOCH, true));

        assert!(streaming(&chat), "no quiesce");
        let _ = client.apply(&mut chat, durable(EPOCH, 4, "entry-4", notice("two")));
        assert_eq!(
            client.cursor().map(|cursor| cursor.seq),
            Some(3),
            "durable frames keep advancing the cursor",
        );
        assert_eq!(notices(&chat), vec!["one", "two"]);
    }

    #[test]
    fn a_queue_update_frame_updates_the_queue_snapshot() {
        let (mut client, mut chat) = attached();
        assert!(client.queue().queues.is_empty());

        assert!(
            client
                .apply(
                    &mut chat,
                    live(
                        EPOCH,
                        AgentEvent::QueueUpdate {
                            agent_id: AgentId::Main,
                            steering: Vec::new(),
                            follow_up: vec![queued("later")],
                        },
                    ),
                )
                .0
        );

        assert_eq!(client.queue().queues.len(), 1);
        assert_eq!(client.queue().queues[0].agent_id, AgentId::Main);
        assert_eq!(client.queue().queues[0].follow_up.len(), 1);

        // Each event carries a full snapshot, so the next one replaces the
        // agent's entry rather than adding to it.
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::QueueUpdate {
                    agent_id: AgentId::Main,
                    steering: vec![queued("now")],
                    follow_up: Vec::new(),
                },
            ),
        );

        assert_eq!(client.queue().queues.len(), 1);
        assert_eq!(client.queue().queues[0].steering.len(), 1);
        assert!(client.queue().queues[0].follow_up.is_empty());

        client.set_queue(QueueState::default());
        assert!(client.queue().queues.is_empty());
    }

    #[test]
    fn a_reset_frame_requires_a_re_attach_and_keeps_the_cursor() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("one")));
        let _ = client.apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("two")));
        assert!(!client.needs_reattach());

        let _ = client.apply(
            &mut chat,
            Frame::Reset {
                session: SESSION.to_string(),
            },
        );

        assert!(client.needs_reattach());
        assert_eq!(
            client.cursor(),
            Some(Cursor {
                epoch: EPOCH.to_string(),
                seq: 2,
            }),
            "the cursor stays valid to offer",
        );
        assert_eq!(notices(&chat), vec!["one", "two"]);

        client.expect_attach();
        assert!(
            !client.needs_reattach(),
            "asking for the attach discharges it",
        );
    }

    #[test]
    fn a_local_event_folds_without_an_envelope() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            durable(EPOCH, 2, "entry-2", notice("from the host")),
        );

        // A frontend's own notice: no epoch, no seq, so neither the epoch
        // filter nor the cursor has anything to say about it.
        assert!(client.apply_local(&mut chat, notice("raised locally")).0);

        assert_eq!(notices(&chat), vec!["from the host", "raised locally"]);
        assert_eq!(
            client.cursor().map(|cursor| cursor.seq),
            Some(0),
            "a local event moves no cursor",
        );

        // And it shares the client's lifecycle rather than a second one.
        let _ = client.apply_local(
            &mut chat,
            AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(client.lifecycle().is_running(AgentId::Main));
    }

    #[test]
    fn a_caught_up_outside_a_block_commits_nothing() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("five")));
        let cursor = client.cursor();

        // No attach was asked for, so this names entries the client never
        // applied. Committing it would silently skip 6..40 on the next
        // re-attach.
        let _ = client.apply(&mut chat, caught_up(EPOCH, 40));

        assert_eq!(client.cursor(), cursor);
    }

    #[test]
    fn non_contiguous_seqs_apply_without_gap_detection() {
        let (mut client, mut chat) = attached();
        for (seq, text) in [(2, "two"), (3, "three"), (7, "seven")] {
            let _ = client.apply(
                &mut chat,
                durable(EPOCH, seq, &format!("entry-{seq}"), notice(text)),
            );
        }

        assert_eq!(notices(&chat), vec!["two", "three", "seven"]);
        assert_eq!(client.cursor().map(|cursor| cursor.seq), Some(3));

        // Entry 7 is the high-water mark despite the gap below it.
        let _ = client.apply(&mut chat, durable(EPOCH, 7, "entry-7", notice("again")));
        assert_eq!(notices(&chat), vec!["two", "three", "seven"]);
    }
}
