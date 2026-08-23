//! The subscriber registry and the fan-out into it.
//!
//! One subscriber is one stream: a producer-paced attach channel, a bounded
//! live queue, and the backfill boundary for each attached session.
//!
//! Publishers only touch the bounded live queue and never await. The attach
//! producer awaits its separate capacity-one channel, so HTTP backpressure
//! paces a large backfill without ever stalling a session driver.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_wire::{Frame, SessionSummary};
use tokio::sync::Notify;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio_util::sync::CancellationToken;

/// Enough burst room for normal clients while bounding a stalled stream.
const DEFAULT_LIVE_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).expect("non-zero");

/// Identity of one attached subscriber within a host.
pub(crate) type SubscriberId = u64;

/// A subscriber's view of one session it attached.
enum AttachState {
    /// The attach block has not been written yet, so live frames are held
    /// to keep the block contiguous and ordered on this stream.
    ///
    /// Lossy frames are dropped rather than queued (spec 6.5): a
    /// cumulative snapshot delivered after the durable frame that
    /// superseded it resurrects stale transient state, and a
    /// `MessageUpdate` for a message the backfill already finalized would
    /// paint a second, unfinalized copy of it. The cost is at most one
    /// coalescing tick of streaming text, which the next live snapshot
    /// restores.
    Attaching,
    /// Live delivery. A durable frame at or below `boundary` is already in
    /// the backfill this stream was served, so it is dropped rather than
    /// re-delivered.
    Live { boundary: u64 },
}

struct Subscriber {
    live: LiveSender,
    attached: HashMap<String, AttachState>,
    /// The directory this subscriber was last sent, if it has been sent one.
    ///
    /// Per subscriber, because the claim it makes is about what this client has
    /// seen: a subscriber that just registered has seen nothing, and one whose
    /// queue dropped a snapshot has not seen that one.
    sent_list: Option<Vec<SessionSummary>>,
}

impl Subscriber {
    /// Queue `frame` for this subscriber, applying the attach rules of the
    /// session it belongs to.
    fn offer(&mut self, frame: &Frame) -> bool {
        self.deliver(frame) != Offered::Evicted
    }

    /// Queue a directory, unless this subscriber already has it.
    ///
    /// The frequent trigger for a refresh is a session event, and most events
    /// move nothing a directory row shows, so the steady state during a turn is
    /// a payload identical to the last one. `list` is cumulative and the latest
    /// frame supersedes (spec 6.4), so an unchanged snapshot carries no
    /// information. Compared on the payload rather than on what produced it,
    /// because the payload is what a client sees.
    fn offer_list(&mut self, sessions: &[SessionSummary]) -> bool {
        if self.sent_list.as_deref() == Some(sessions) {
            return true;
        }
        let frame = Frame::List {
            sessions: sessions.to_vec(),
            // A plain host's rows are all its own, so it names no hosts
            // (spec 7.1).
            hosts: Vec::new(),
        };
        match self.deliver(&frame) {
            Offered::Queued => {
                self.sent_list = Some(sessions.to_vec());
                true
            }
            // Not delivered, so not remembered: the next refresh offers this
            // subscriber the directory again, unchanged or not.
            Offered::Dropped => true,
            Offered::Evicted => false,
        }
    }

    fn deliver(&mut self, frame: &Frame) -> Offered {
        let Some(session) = frame.session() else {
            // Host-level frames (`list`, `heartbeat`) belong to the
            // connection, not to a session, so no attach state gates them.
            return self.live.offer(frame.clone());
        };
        match self.attached.get_mut(session) {
            None => {
                // A session this stream did not name produces nothing but its
                // row in `list` frames (spec 6.5). Its events would apply to
                // no state this client holds, its seqs may not be used as
                // cursors, and its reliable-transient frames are undroppable
                // by class, so delivering them would let a busy session evict
                // a client that never asked to watch it.
                Offered::Dropped
            }
            Some(AttachState::Attaching) => {
                if !frame.is_lossy() {
                    return self.live.offer(frame.clone());
                }
                Offered::Dropped
            }
            Some(AttachState::Live { boundary }) => {
                if frame.durable_seq().is_some_and(|seq| seq <= *boundary) {
                    // Already in the backfill this stream was served, so the
                    // subscriber has it.
                    return Offered::Queued;
                }
                self.live.offer(frame.clone())
            }
        }
    }
}

/// Identity of one cumulative snapshot in the live queue.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LossyKey {
    Message(String, AgentId),
    Tool(String, String),
    Task(String, TaskId),
    State(String),
    List,
    Vms,
}

fn lossy_key(frame: &Frame) -> Option<LossyKey> {
    match frame {
        Frame::Event { session, event, .. } => match event.known()? {
            AgentEvent::MessageUpdate { agent_id, .. } => {
                Some(LossyKey::Message(session.clone(), *agent_id))
            }
            AgentEvent::ToolExecutionUpdate { call_id, .. } => {
                Some(LossyKey::Tool(session.clone(), call_id.clone()))
            }
            AgentEvent::TaskOutput { task_id, .. } => {
                Some(LossyKey::Task(session.clone(), *task_id))
            }
            _ => None,
        },
        Frame::State { session, .. } => Some(LossyKey::State(session.clone())),
        Frame::List { .. } => Some(LossyKey::List),
        Frame::Vms { .. } => Some(LossyKey::Vms),
        Frame::CaughtUp { .. } | Frame::Error { .. } | Frame::Reset { .. } | Frame::Heartbeat => {
            None
        }
    }
}

struct LiveQueueState {
    frames: VecDeque<Frame>,
    closed: bool,
}

struct LiveQueue {
    capacity: NonZeroUsize,
    state: StdMutex<LiveQueueState>,
    ready: Notify,
    cancelled: CancellationToken,
}

#[derive(Clone)]
struct LiveSender(Arc<LiveQueue>);

pub(crate) struct LiveReceiver(Arc<LiveQueue>);

fn live_channel(capacity: NonZeroUsize) -> (LiveSender, LiveReceiver, CancellationToken) {
    let cancelled = CancellationToken::new();
    let queue = Arc::new(LiveQueue {
        capacity,
        state: StdMutex::new(LiveQueueState {
            frames: VecDeque::with_capacity(capacity.get()),
            closed: false,
        }),
        ready: Notify::new(),
        cancelled: cancelled.clone(),
    });
    (
        LiveSender(Arc::clone(&queue)),
        LiveReceiver(queue),
        cancelled,
    )
}

/// What became of an offered frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Offered {
    /// The subscriber has it: queued, coalesced onto a queued frame of the same
    /// lossy key, or already in the backfill it was served.
    Queued,
    /// Not delivered, and the subscriber stays. A lossy frame met a full queue,
    /// or the session it belongs to is one this subscriber is not watching.
    Dropped,
    /// The subscriber is gone: its queue overflowed with frames that may not be
    /// dropped, or it had already been closed.
    Evicted,
}

impl LiveSender {
    /// Queues a frame without blocking. Reliable overflow closes the stream.
    fn offer(&self, frame: Frame) -> Offered {
        let key = lossy_key(&frame);
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        if state.closed {
            return Offered::Evicted;
        }
        if let Some(key) = key {
            if let Some(index) = state
                .frames
                .iter()
                .position(|queued| lossy_key(queued).as_ref() == Some(&key))
            {
                state.frames.remove(index);
            } else if state.frames.len() >= self.0.capacity.get() {
                return Offered::Dropped;
            }
        } else if state.frames.len() >= self.0.capacity.get() {
            state.frames.clear();
            state.closed = true;
            drop(state);
            self.0.cancelled.cancel();
            self.0.ready.notify_waiters();
            return Offered::Evicted;
        }
        state.frames.push_back(frame);
        drop(state);
        self.0.ready.notify_one();
        Offered::Queued
    }

    fn retain(&self, mut keep: impl FnMut(&Frame) -> bool) {
        self.0
            .state
            .lock()
            .expect("live queue mutex poisoned")
            .frames
            .retain(|frame| keep(frame));
    }

    fn close(&self) {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        state.closed = true;
        drop(state);
        self.0.ready.notify_waiters();
    }

    fn evict(&self) {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        state.frames.clear();
        state.closed = true;
        drop(state);
        self.0.cancelled.cancel();
        self.0.ready.notify_waiters();
    }
}

// The queue is behind a mutex, so neither receive needs `&mut` to be sound.
// They take it to say there is one reader: two tasks receiving concurrently
// would interleave a session's frames, and the stream's whole contract is that
// they arrive in order.
#[allow(clippy::needless_pass_by_ref_mut)]
impl LiveReceiver {
    async fn recv(&mut self) -> Option<Frame> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut state = self.0.state.lock().expect("live queue mutex poisoned");
                if let Some(frame) = state.frames.pop_front() {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            ready.await;
        }
    }

    fn try_recv(&mut self) -> Option<Frame> {
        self.0
            .state
            .lock()
            .expect("live queue mutex poisoned")
            .frames
            .pop_front()
    }
}

/// The host's subscriber registry.
pub(crate) struct Fanout {
    subscribers: StdMutex<HashMap<SubscriberId, Subscriber>>,
    next_id: AtomicU64,
    /// Pinged whenever the session directory changed. The list publisher
    /// coalesces on it (spec 6.8).
    list_dirty: Notify,
    live_capacity: NonZeroUsize,
}

impl Default for Fanout {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Fanout {
    /// A registry whose per-client live queue holds `live_capacity` frames,
    /// or [`DEFAULT_LIVE_CAPACITY`] when the caller has no opinion.
    pub(crate) fn new(live_capacity: Option<NonZeroUsize>) -> Self {
        Self {
            subscribers: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            list_dirty: Notify::new(),
            live_capacity: live_capacity.unwrap_or(DEFAULT_LIVE_CAPACITY),
        }
    }

    /// Register a subscriber that is about to be served attach blocks for
    /// `sessions`.
    ///
    /// Registration happens before the blocks are projected, which is what
    /// makes an attach atomic with respect to the session's event flow:
    /// every frame published from here on is either queued behind the block or
    /// filtered against its boundary, so none can be missed.
    pub(crate) fn register(
        &self,
        sessions: &[String],
    ) -> (SubscriberId, LiveReceiver, CancellationToken) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (live, receiver, cancelled) = live_channel(self.live_capacity);
        let attached = sessions
            .iter()
            .map(|session| (session.clone(), AttachState::Attaching))
            .collect();
        self.lock().insert(
            id,
            Subscriber {
                live,
                attached,
                sent_list: None,
            },
        );
        (id, receiver, cancelled)
    }

    pub(crate) fn deregister(&self, id: SubscriberId) {
        if let Some(subscriber) = self.lock().remove(&id) {
            subscriber.live.evict();
        }
    }

    /// Take `session` back off a subscriber's attach set, for one its attach
    /// could not resolve (spec 6.5).
    ///
    /// A subscriber is registered for every session its request names before
    /// any of them is resolved, which is what makes an attach in flight count
    /// as use. A session that then turns out to be unservable has to come back
    /// out: this host may hold it later, for another client, and a
    /// session-scoped frame is undroppable by class, so it would count against
    /// a bound this client never asked to spend and could evict it over traffic
    /// it never asked for.
    pub(crate) fn detach(&self, id: SubscriberId, session: &str) {
        if let Some(subscriber) = self.lock().get_mut(&id) {
            subscriber.attached.remove(session);
            // Anything already queued for it goes too. Resolving a session
            // takes a moment (a materialization reads its log), and another
            // client can make it live in that window, so the registration this
            // is undoing may have caught frames of its own.
            subscriber
                .live
                .retain(|frame| frame.session() != Some(session));
        }
    }

    /// Fan `frame` out to every subscriber.
    pub(crate) fn publish(&self, frame: Frame) {
        self.lock().retain(|_, subscriber| subscriber.offer(&frame));
    }

    /// Fan a directory out to every subscriber that does not already have it
    /// (see [`Subscriber::offer_list`]).
    pub(crate) fn publish_list(&self, sessions: Vec<SessionSummary>) {
        self.lock()
            .retain(|_, subscriber| subscriber.offer_list(&sessions));
    }

    /// Switches a session to live delivery and filters duplicate durables.
    pub(crate) fn finish_block(&self, id: SubscriberId, session: &str, boundary: u64) {
        let mut subscribers = self.lock();
        let Some(subscriber) = subscribers.get_mut(&id) else {
            return;
        };
        subscriber.live.retain(|frame| {
            frame.session() != Some(session)
                || !frame.durable_seq().is_some_and(|seq| seq <= boundary)
        });
        subscriber
            .attached
            .insert(session.to_string(), AttachState::Live { boundary });
    }

    /// Drop every subscriber, closing its stream.
    pub(crate) fn close(&self) {
        for (_, subscriber) in self.lock().drain() {
            subscriber.live.close();
        }
    }

    /// Whether any subscriber is attached to `session`.
    ///
    /// True from the moment a subscriber registers, not from when its attach
    /// block completes, which is what lets the release path treat an attach in
    /// flight as use (spec section 5: attachment is the retention signal).
    pub(crate) fn attached(&self, session: &str) -> bool {
        self.lock()
            .values()
            .any(|subscriber| subscriber.attached.contains_key(session))
    }

    /// Note that the session directory changed, waking the list publisher.
    pub(crate) fn mark_list_dirty(&self) {
        self.list_dirty.notify_one();
    }

    pub(crate) fn list_dirty(&self) -> &Notify {
        &self.list_dirty
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SubscriberId, Subscriber>> {
        self.subscribers
            .lock()
            .expect("fanout subscribers mutex poisoned")
    }
}

/// One client's frame stream.
///
/// Dropping it deregisters the subscriber, so a client that goes away
/// stops costing the host anything.
pub struct Attachment {
    id: SubscriberId,
    block: Receiver<Frame>,
    block_done: bool,
    live: LiveReceiver,
    cancelled: CancellationToken,
    attached: Vec<String>,
    fanout: Arc<Fanout>,
}

impl Attachment {
    /// Build an attachment and the sender the block is written to.
    ///
    /// The channel is made here, with capacity one, because that capacity is
    /// part of what an attachment is rather than a choice its caller makes:
    /// the projection that fills the block gets to run exactly one frame
    /// ahead of the client reading it, so a slow client paces the projection
    /// instead of letting it build the whole backfill in memory. A caller
    /// that passed its own channel could pick any depth and lose that.
    pub(crate) fn new(
        id: SubscriberId,
        live: LiveReceiver,
        cancelled: CancellationToken,
        attached: Vec<String>,
        fanout: Arc<Fanout>,
    ) -> (Self, Sender<Frame>) {
        let (block_tx, block) = channel(1);
        let attachment = Self {
            id,
            block,
            block_done: false,
            live,
            cancelled,
            attached,
            fanout,
        };
        (attachment, block_tx)
    }

    /// The sessions this stream was served an attach block for.
    ///
    /// A client arms its fold from this rather than from what it asked for:
    /// a session the attach could not resolve is answered with an `error`
    /// frame instead of a block (spec 6.5), so it is not here, and arming for
    /// a block that never comes strands that session's fold.
    pub fn attached(&self) -> &[String] {
        &self.attached
    }

    /// The next frame, or `None` once the host closed the stream.
    pub async fn recv(&mut self) -> Option<Frame> {
        if self.cancelled.is_cancelled() {
            return None;
        }
        if !self.block_done {
            let next = tokio::select! {
                biased;
                _ = self.cancelled.cancelled() => return None,
                next = self.block.recv() => next,
            };
            if next.is_some() {
                return next;
            }
            self.block_done = true;
        }
        tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => None,
            next = self.live.recv() => next,
        }
    }

    /// The next already-queued frame, without waiting. `None` both when
    /// the queue is empty and when the stream closed, which a caller
    /// draining what it has does not need to distinguish.
    pub fn try_recv(&mut self) -> Option<Frame> {
        if self.cancelled.is_cancelled() {
            return None;
        }
        if !self.block_done {
            match self.block.try_recv() {
                Ok(frame) => return Some(frame),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => self.block_done = true,
            }
        }
        self.live.try_recv()
    }
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The frames in flight are not worth rendering, and the registry
        // behind `fanout` is not `Debug` on purpose (it holds live senders).
        write!(f, "Attachment({})", self.id)
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.fanout.deregister(self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::ToolDetails;
    use aj_models::streaming::AssistantMessageEvent;
    use aj_models::types::{AssistantMessage, Message};

    use super::*;

    const SESSION: &str = "session-1";
    /// The second session of the multi-session tests.
    const OTHER: &str = "session-2";
    const EPOCH: &str = "epoch-1";

    fn durable(seq: u64) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            durability: Some(aj_wire::DurableEvent {
                seq,
                entry_id: format!("entry-{seq}"),
            }),
            event: AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: format!("entry {seq}"),
            }
            .into(),
        }
    }

    /// A reliable-transient frame: one-shot, never droppable.
    fn reliable(text: &str) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            durability: None,
            event: AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: text.to_string(),
            }
            .into(),
        }
    }

    /// A lossy frame: a cumulative snapshot a later one supersedes.
    fn lossy(last_seq: u64) -> Frame {
        Frame::State {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            working: true,
            settings: AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            last_seq,
        }
    }

    fn caught_up(last_seq: u64) -> Frame {
        Frame::CaughtUp {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            last_seq,
        }
    }

    /// A streaming message update: the highest-volume lossy class, keyed by the
    /// session and the agent whose message it is about.
    fn streaming(agent_id: AgentId) -> Frame {
        let partial = AssistantMessage::empty();
        Frame::Event {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            durability: None,
            event: AgentEvent::MessageUpdate {
                agent_id,
                message: AgentMessage::wire(Message::Assistant(partial.clone())),
                event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "tick".to_string(),
                    partial,
                },
            }
            .into(),
        }
    }

    /// A background task's cumulative output snapshot, keyed by the session and
    /// the task it is about.
    /// A streaming tool update for `call_id`, the frame class whose key has to
    /// discriminate one in-flight tool call from another.
    fn tool_update(call_id: &str) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            durability: None,
            event: AgentEvent::ToolExecutionUpdate {
                agent_id: AgentId::Main,
                call_id: call_id.to_string(),
                tool: "bash".to_string(),
                args: serde_json::json!({}),
                partial: ToolDetails::Text {
                    summary: "running".to_string(),
                    body: String::new(),
                },
                content: Arc::from(Vec::new()),
            }
            .into(),
        }
    }

    fn task_output(task_id: TaskId) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: EPOCH.to_string(),
            durability: None,
            event: AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id,
                call_id: "call-1".to_string(),
                partial: ToolDetails::Text {
                    summary: "running".to_string(),
                    body: String::new(),
                },
            }
            .into(),
        }
    }

    /// A session-scoped refusal (spec 6.3), reliable-transient like the rest of
    /// its class.
    fn refusal(code: &str) -> Frame {
        Frame::Error {
            session: SESSION.to_string(),
            epoch: None,
            code: code.to_string(),
            message: format!("no {code} here"),
        }
    }

    /// Everything queued on `rx`, rendered as a comparable summary.
    fn drained(rx: &mut LiveReceiver) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(frame) = rx.try_recv() {
            out.push(match &frame {
                Frame::Event {
                    durability: Some(durability),
                    ..
                } => format!("durable {}", durability.seq),
                Frame::Event { event, .. } => match event.known() {
                    Some(AgentEvent::Warning { text, .. }) => format!("warning {text}"),
                    Some(AgentEvent::MessageUpdate { agent_id, .. }) => {
                        format!("update {agent_id:?}")
                    }
                    Some(AgentEvent::TaskOutput { task_id, .. }) => format!("task {task_id}"),
                    Some(AgentEvent::ToolExecutionUpdate { call_id, .. }) => {
                        format!("tool {call_id}")
                    }
                    other => format!("event {other:?}"),
                },
                Frame::State { last_seq, .. } => format!("state {last_seq}"),
                Frame::CaughtUp { last_seq, .. } => format!("caught_up {last_seq}"),
                Frame::Error { code, .. } => format!("error {code}"),
                Frame::Reset { .. } => "reset".to_string(),
                Frame::List { .. } => "list".to_string(),
                other => format!("{other:?}"),
            });
        }
        out
    }

    /// How many frames are waiting on `id`'s live queue.
    ///
    /// The bound is on this queue, so a test about the bound has to be able to
    /// say that its fixture reached it, and a test about coalescing that two
    /// snapshots stayed two frames.
    fn queued(fanout: &Fanout, id: SubscriberId) -> usize {
        fanout.lock()[&id]
            .live
            .0
            .state
            .lock()
            .expect("live queue mutex poisoned")
            .frames
            .len()
    }

    /// Reliable live frames collect while a block is produced. Lossy frames
    /// are dropped, and duplicate durable frames are removed at transition.
    #[test]
    fn an_attach_transition_filters_the_bounded_live_queue() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);

        fanout.publish(durable(3));
        fanout.publish(reliable("held"));
        fanout.publish(lossy(1));

        fanout.finish_block(id, SESSION, 5);

        assert_eq!(
            drained(&mut rx),
            vec!["warning held"],
            "entry 3 is covered by the block and the lossy frame was dropped",
        );
    }

    /// A durable frame still in flight in the fan-out when the block was
    /// served is dropped if the backfill already covered it, and delivered
    /// if it did not.
    #[test]
    fn a_live_stream_filters_durable_frames_at_or_below_its_boundary() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 5);

        fanout.publish(durable(4));
        fanout.publish(durable(5));
        fanout.publish(reliable("after"));
        fanout.publish(durable(6));

        assert_eq!(drained(&mut rx), vec!["warning after", "durable 6"]);
    }

    /// One directory row, enough to tell two payloads apart.
    fn directory(last_seq: u64) -> Vec<SessionSummary> {
        vec![SessionSummary {
            id: SESSION.to_string(),
            live: true,
            working: false,
            queued: aj_wire::QueueCounts::default(),
            tasks: 0,
            last_seq: Some(last_seq),
            last_activity: chrono::DateTime::UNIX_EPOCH,
            tag: None,
            host: None,
            unreachable: false,
            archived: false,
            locked: false,
        }]
    }

    /// A directory a subscriber already has is not sent again, and a changed one
    /// is.
    #[test]
    fn an_unchanged_directory_is_not_offered_twice() {
        let fanout = Fanout::default();
        let (_id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);

        fanout.publish_list(directory(1));
        fanout.publish_list(directory(1));
        fanout.publish_list(directory(1));
        assert_eq!(drained(&mut rx), vec!["list"], "one directory, sent once");

        fanout.publish_list(directory(2));
        assert_eq!(drained(&mut rx), vec!["list"], "a real change gets through");
    }

    /// A directory a subscriber's full queue dropped is offered again, unchanged
    /// or not. Suppression records what was delivered, so a lossy frame the
    /// bound turned away does not count as sent.
    #[test]
    fn a_dropped_directory_is_offered_again() {
        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);

        // The client is not reading, so its queue fills with frames that may
        // not be dropped.
        fanout.publish(reliable("one"));
        fanout.publish(reliable("two"));
        fanout.publish_list(directory(1));
        assert!(
            !cancelled.is_cancelled(),
            "a lossy frame meeting the bound drops rather than evicting",
        );
        assert_eq!(
            drained(&mut rx),
            vec!["warning one", "warning two"],
            "the directory did not fit",
        );

        // Caught up, and the directory has not moved since.
        fanout.publish_list(directory(1));
        assert_eq!(
            drained(&mut rx),
            vec!["list"],
            "the subscriber is offered the directory it never got",
        );
    }

    /// A fresh subscriber is sent the directory even though every other
    /// subscriber already has it: suppression is a claim about one client.
    #[test]
    fn a_fresh_subscriber_is_offered_a_directory_the_others_have() {
        let fanout = Fanout::default();
        let (_settled, mut settled_rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.publish_list(directory(1));
        assert_eq!(drained(&mut settled_rx), vec!["list"]);

        let (_fresh, mut fresh_rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.publish_list(directory(1));
        assert_eq!(
            drained(&mut fresh_rx),
            vec!["list"],
            "the new subscriber has seen no directory",
        );
        assert!(
            drained(&mut settled_rx).is_empty(),
            "and the one that has is not sent it again",
        );
    }

    /// A session this stream did not attach produces nothing on it, not even
    /// its durable and reliable-transient frames (spec 6.5). The host-level
    /// `list` frame still flows: it belongs to the connection.
    #[test]
    fn an_unattached_session_produces_nothing_but_the_list() {
        let fanout = Fanout::default();
        let (_id, mut rx, _cancelled) = fanout.register(&[]);

        fanout.publish(durable(1));
        fanout.publish(reliable("dropped"));
        fanout.publish(lossy(0));
        fanout.publish(Frame::Reset {
            session: SESSION.to_string(),
        });
        fanout.publish(Frame::List {
            sessions: Vec::new(),
            hosts: Vec::new(),
        });

        assert_eq!(drained(&mut rx), vec!["list"]);
    }

    /// One stream, two sessions: each session's backfill boundary applies to
    /// its own seq space only. A subscriber-wide boundary would swallow the
    /// durable frames of whichever session's block finished with the lower
    /// mark.
    #[test]
    fn one_stream_gates_each_attached_session_separately() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);

        // Two live sessions with different boundaries, which is the case a
        // shared one gets wrong.
        fanout.finish_block(id, SESSION, 5);
        fanout.finish_block(id, OTHER, 1);

        fanout.publish(durable(5));
        fanout.publish(durable(6));
        // At or below the *other* session's boundary, and well below this
        // one's: delivered, because it is not this session's seq space.
        fanout.publish(other(durable(3)));
        fanout.publish(other(durable(1)));

        assert_eq!(
            drained(&mut rx),
            vec!["durable 6", "durable 3"],
            "each session filters against its own boundary",
        );
    }

    /// Retag a session-scoped frame onto the second session of the
    /// multi-session tests.
    fn other(mut frame: Frame) -> Frame {
        match &mut frame {
            Frame::Event { session, .. }
            | Frame::State { session, .. }
            | Frame::CaughtUp { session, .. }
            | Frame::Error { session, .. }
            | Frame::Reset { session } => OTHER.clone_into(session),
            Frame::List { .. } | Frame::Heartbeat | Frame::Vms { .. } => {
                panic!("a host-level frame belongs to no session")
            }
        }
        frame
    }

    /// A session an attach could not resolve is taken back off the stream's
    /// attach set, frames and all.
    ///
    /// The registration covers every session the request named, before any of
    /// them is resolved, so that an attach in flight counts as use. Resolving
    /// one takes a moment, and another client can make a session live in that
    /// window, so undoing the registration has to cover what it caught as well
    /// as what would come later: this stream was never served that session's
    /// block, and its frames are undroppable by class, so they would count
    /// against a bound this client never asked to spend (spec 6.5, 6.9).
    #[test]
    fn a_detached_session_leaves_nothing_of_itself_on_the_stream() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        fanout.finish_block(id, SESSION, 0);
        fanout.finish_block(id, OTHER, 0);
        // Live before the attach that named it got as far as refusing it.
        fanout.publish(reliable("caught in the window"));
        fanout.publish(other(reliable("someone else's session")));

        fanout.detach(id, SESSION);

        fanout.publish(reliable("after"));
        assert_eq!(
            drained(&mut rx),
            vec!["warning someone else's session"],
            "the refused session left frames behind, or took another session's \
             with it",
        );
        assert!(
            !fanout.attached(SESSION),
            "and the stream is no longer counted as holding it",
        );
        assert!(fanout.attached(OTHER));
    }

    /// A refusal is reliable-transient (spec 6.4), so neither queue rule that
    /// exists for lossy frames may touch it: it is held behind an attach block
    /// rather than dropped, and at the bound it evicts rather than being lost.
    /// A dropped refusal is a client left waiting for an attach block that was
    /// already answered.
    #[test]
    fn a_refusal_is_neither_dropped_during_an_attach_nor_at_the_bound() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.publish(refusal("unknown_session"));

        fanout.finish_block(id, SESSION, 0);

        assert_eq!(
            drained(&mut rx),
            vec!["error unknown_session"],
            "a lossy frame published during an attach would have been dropped",
        );

        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);
        fanout.publish(reliable("one"));
        fanout.publish(reliable("two"));

        fanout.publish(refusal("unknown_session"));

        assert!(
            cancelled.is_cancelled(),
            "a refusal that met the bound was dropped instead of evicting: {:?}",
            drained(&mut rx),
        );
    }

    /// A `reset` published during an attach remains in the live queue.
    #[test]
    fn a_reset_during_an_attach_is_delivered_behind_the_block() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.publish(reliable("held"));
        fanout.publish(Frame::Reset {
            session: SESSION.to_string(),
        });

        fanout.finish_block(id, SESSION, 0);

        assert_eq!(drained(&mut rx), vec!["warning held", "reset"]);
    }

    /// Replacing a lossy frame removes the old one and appends the new one at
    /// the tail, so it cannot jump a reliable boundary.
    #[test]
    fn lossy_replacement_moves_to_the_queue_tail() {
        let fanout = Fanout::new(NonZeroUsize::new(3));
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);

        fanout.publish(reliable("before"));
        fanout.publish(lossy(1));
        fanout.publish(reliable("after"));
        fanout.publish(lossy(2));

        assert_eq!(
            drained(&mut rx),
            vec!["warning before", "warning after", "state 2"]
        );
    }

    /// Lossy overflow drops the incoming snapshot. Reliable overflow evicts
    /// the subscriber instead of silently losing the frame.
    #[test]
    fn live_overflow_drops_lossy_and_evicts_on_reliable() {
        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);
        fanout.publish(reliable("one"));
        fanout.publish(reliable("two"));
        assert_eq!(
            queued(&fanout, id),
            2,
            "the queue is at the bound, or nothing below this measures anything",
        );
        fanout.publish(Frame::List {
            sessions: Vec::new(),
            hosts: Vec::new(),
        });
        assert!(!cancelled.is_cancelled(), "lossy overflow is only dropped");
        assert_eq!(
            queued(&fanout, id),
            2,
            "and the snapshot the bound turned away did not land anyway",
        );

        fanout.publish(reliable("three"));
        assert!(cancelled.is_cancelled());
        assert!(fanout.lock().is_empty(), "the subscriber was evicted");
        assert!(drained(&mut rx).is_empty(), "eviction closes and clears");
    }

    /// A durable frame may not be coalesced or dropped, whatever its event is
    /// (spec 6.4). Nothing re-sends one: a client that lost a durable frame is
    /// missing a log entry with nothing to tell it so. At the bound the
    /// subscriber is evicted instead, and the backfill of its re-attach carries
    /// the entry from its cursor.
    #[test]
    fn a_durable_frame_is_neither_coalesced_nor_dropped() {
        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);

        fanout.publish(durable(1));
        fanout.publish(durable(2));
        assert_eq!(
            queued(&fanout, id),
            2,
            "two durable frames of one session are two frames, and the queue is \
             at the bound, or the eviction below measures nothing",
        );

        fanout.publish(durable(3));

        assert!(
            cancelled.is_cancelled(),
            "a durable frame that met the bound was dropped instead of evicting: {:?}",
            drained(&mut rx),
        );
    }

    /// A snapshot supersedes the queued one of its own key and no other. The key
    /// names what the snapshot is about: the session, and within it the agent or
    /// the task. A key blind to any of those would let one session's, agent's or
    /// task's snapshot swallow another's, and a swallowed snapshot is never
    /// re-sent, so the client holds stale state for it until it re-attaches.
    #[test]
    fn coalescing_discriminates_by_what_a_snapshot_is_about() {
        let fanout = Fanout::default();
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        fanout.finish_block(id, SESSION, 0);
        fanout.finish_block(id, OTHER, 0);

        fanout.publish(lossy(1));
        fanout.publish(other(lossy(2)));
        fanout.publish(streaming(AgentId::Main));
        fanout.publish(streaming(AgentId::Sub(1)));
        fanout.publish(task_output(7));
        fanout.publish(task_output(8));
        fanout.publish(tool_update("call-a"));
        fanout.publish(tool_update("call-b"));

        assert_eq!(
            drained(&mut rx),
            vec![
                "state 1",
                "state 2",
                "update Main",
                "update Sub(1)",
                "task 7",
                "task 8",
                "tool call-a",
                "tool call-b",
            ],
            "two sessions, two agents, two tasks and two tool calls are eight \
             keys, not one",
        );

        // The same key does supersede, which is what says the six above are six
        // keys rather than a queue that coalesces nothing at all.
        fanout.publish(streaming(AgentId::Main));
        fanout.publish(streaming(AgentId::Main));
        assert_eq!(
            drained(&mut rx),
            vec!["update Main"],
            "one key, one queued snapshot",
        );
    }

    /// The attach channel has capacity one and live frames remain hidden until
    /// its sender closes.
    #[test]
    fn attachment_is_producer_paced_and_reads_the_block_before_live() {
        let fanout = Arc::new(Fanout::default());
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string()]);
        let (mut attachment, block_tx) = Attachment::new(
            id,
            live,
            cancelled,
            vec![SESSION.to_string()],
            Arc::clone(&fanout),
        );
        block_tx.try_send(lossy(1)).expect("first block frame");
        // The channel is the attachment's own, so this measures the depth a
        // real attach runs with rather than one the test chose.
        assert!(
            block_tx.try_send(caught_up(1)).is_err(),
            "the producer cannot preload a second frame",
        );
        fanout.publish(reliable("live"));

        assert!(matches!(attachment.try_recv(), Some(Frame::State { .. })));
        assert!(
            attachment.try_recv().is_none(),
            "live frames stay behind an unfinished block",
        );
        drop(block_tx);
        assert!(matches!(attachment.try_recv(), Some(Frame::Event { .. })));
    }

    /// A stream parked on an empty queue is woken by the next live frame, and
    /// ends when the host closes it.
    ///
    /// Every other test here drains with `try_recv`, which never parks, so a
    /// lost wakeup or a queue that forgets to end would pass all of them and
    /// hang a real client: a stalled stream is indistinguishable from a quiet
    /// one until something else happens to wake it.
    #[tokio::test]
    async fn a_parked_stream_is_woken_by_a_frame_and_ended_by_a_close() {
        let fanout = Arc::new(Fanout::default());
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);

        // Published from another task, after this one has parked on `recv`: the
        // sleep can only elapse while this task is waiting, so the frame has to
        // carry the wakeup with it.
        let publishing = tokio::spawn({
            let fanout = Arc::clone(&fanout);
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                fanout.publish(reliable("late"));
            }
        });
        let woken = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the parked stream to be woken by the live frame");
        assert!(matches!(woken, Some(Frame::Event { .. })));
        publishing.await.expect("the publishing task");

        let closing = tokio::spawn({
            let fanout = Arc::clone(&fanout);
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                fanout.close();
            }
        });
        let ended = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the parked stream to end when the host closes it");
        assert!(ended.is_none(), "a closed stream hands nothing back");
        closing.await.expect("the closing task");
    }

    /// Dropping the stream deregisters it, so the host stops paying for a
    /// client that went away.
    #[test]
    fn dropping_an_attachment_deregisters_it() {
        let fanout = Arc::new(Fanout::default());
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string()]);
        let (attachment, _block_tx) = Attachment::new(
            id,
            live,
            cancelled,
            vec![SESSION.to_string()],
            Arc::clone(&fanout),
        );
        assert_eq!(fanout.lock().len(), 1);

        drop(attachment);

        assert!(fanout.lock().is_empty());
    }
}
