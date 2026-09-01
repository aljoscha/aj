//! The subscriber registry and the fan-out into it.
//!
//! One subscriber is one stream: a producer-paced attach channel, a bounded
//! live queue, and the backfill boundary for each attached session.
//!
//! Publishers only touch the bounded live queue and never await. The attach
//! producer awaits its separate capacity-one channel, so HTTP backpressure
//! paces a large backfill without ever stalling a session driver.

use std::collections::{HashMap, HashSet, VecDeque};
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

/// Where one registered session's promised block stands.
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
    /// This materialization ended before its block could complete. The session
    /// stays registered until terminal delivery removes it, but no producer may
    /// restore it to live delivery.
    Stopped,
}

/// One session registered on a subscriber.
///
/// The block stop is a child of the stream-wide stop token. Ending this session
/// wakes only its producer, while shutdown and eviction still stop every block
/// on the stream.
struct SessionAttach {
    state: AttachState,
    materialization: Option<u64>,
    block_stop: CancellationToken,
}

/// Generation-bound authority to produce and complete one attach block.
pub(crate) struct AttachBlock {
    subscriber: SubscriberId,
    session: String,
    materialization: u64,
    block_stop: CancellationToken,
}

impl AttachBlock {
    pub(crate) fn stopped(&self) -> &CancellationToken {
        &self.block_stop
    }
}

struct Subscriber {
    live: LiveSender,
    attached: HashMap<String, SessionAttach>,
    /// The latest directory this subscriber's queue accepted.
    ///
    /// Per subscriber, because queue admission is the comparison point. A
    /// subscriber that just registered has accepted nothing, and a snapshot its
    /// full queue dropped was not accepted. A queued snapshot stays the
    /// comparison point when coalescing replaces it, so a later restore of an
    /// older delivered value is still recognized as a change (spec 6.8).
    accepted_list: Option<Vec<SessionSummary>>,
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
        if self.accepted_list.as_deref() == Some(sessions) {
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
                self.accepted_list = Some(sessions.to_vec());
                true
            }
            // Not accepted, so not remembered: the next refresh offers this
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
        match self
            .attached
            .get_mut(session)
            .map(|attached| &mut attached.state)
        {
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
            Some(AttachState::Stopped) => Offered::Dropped,
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
    terminal_sessions: HashSet<String>,
}

struct LiveQueue {
    capacity: NonZeroUsize,
    state: StdMutex<LiveQueueState>,
    ready: Notify,
    /// Stops attach-block production without discarding a completed block's
    /// queued live frames.
    block_stop: CancellationToken,
    cancelled: CancellationToken,
}

#[derive(Clone)]
struct LiveSender(Arc<LiveQueue>);

pub(crate) struct LiveReceiver(Arc<LiveQueue>);

fn live_channel(capacity: NonZeroUsize) -> (LiveSender, LiveReceiver, CancellationToken) {
    let block_stop = CancellationToken::new();
    let cancelled = CancellationToken::new();
    let queue = Arc::new(LiveQueue {
        capacity,
        state: StdMutex::new(LiveQueueState {
            frames: VecDeque::with_capacity(capacity.get()),
            closed: false,
            terminal_sessions: HashSet::new(),
        }),
        ready: Notify::new(),
        block_stop,
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
    fn block_stop_child(&self) -> CancellationToken {
        self.0.block_stop.child_token()
    }

    /// Queues a frame without blocking. Reliable overflow closes the stream.
    fn offer(&self, frame: Frame) -> Offered {
        let key = lossy_key(&frame);
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        if state.closed {
            return Offered::Evicted;
        }
        if frame
            .session()
            .is_some_and(|session| state.terminal_sessions.contains(session))
        {
            return Offered::Dropped;
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
            // Cancellation is the hard-eviction signal and Attachment checks it
            // before reading this queue. Once a terminal frame is queued, using
            // that signal would let EOF overtake the actionable reason for the
            // close. Keep the complete accepted queue and close it gracefully
            // instead. The receiver drains that bounded FIFO before EOF, while
            // ordinary reliable overflow remains an immediate eviction.
            let terminal_sessions = state.terminal_sessions.clone();
            let terminal_pending = state.frames.iter().any(|queued| {
                queued
                    .session()
                    .is_some_and(|session| terminal_sessions.contains(session))
            });
            if !terminal_pending {
                state.frames.clear();
            }
            state.closed = true;
            drop(state);
            self.0.block_stop.cancel();
            if !terminal_pending {
                self.0.cancelled.cancel();
            }
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

    /// Queue a session's terminal frame even when ordinary capacity is spent.
    ///
    /// Each attached session can contribute at most one. Temporarily exceeding
    /// the ordinary bound preserves every earlier reliable frame and every
    /// actionable reason a shared stream is about to close.
    fn offer_terminal(&self, frame: Frame) -> bool {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        if state.closed {
            return false;
        }
        let session = frame
            .session()
            .expect("a terminal frame names its session")
            .to_string();
        if !state.terminal_sessions.insert(session) {
            return false;
        }
        state.frames.push_back(frame);
        drop(state);
        self.0.ready.notify_one();
        true
    }

    fn close(&self) {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        state.closed = true;
        drop(state);
        self.0.block_stop.cancel();
        self.0.ready.notify_waiters();
    }

    /// Close after any attach block already being produced reaches its normal
    /// boundary. Queued live frames remain readable after that block.
    fn close_after_block(&self) {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        state.closed = true;
        drop(state);
        self.0.ready.notify_waiters();
    }

    fn stop_block(&self) {
        self.0.block_stop.cancel();
    }

    fn evict(&self) {
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        state.frames.clear();
        state.closed = true;
        drop(state);
        self.0.block_stop.cancel();
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
    pub(crate) fn block_stop_token(&self) -> CancellationToken {
        self.0.block_stop.clone()
    }

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
    state: StdMutex<FanoutState>,
    next_id: AtomicU64,
    /// Pinged whenever the session directory changed. The list publisher
    /// coalesces on it (spec 6.8).
    list_dirty: Notify,
    live_capacity: NonZeroUsize,
}

/// The exact client streams that received one session's terminal frame.
pub(crate) struct SessionStreams {
    fanout: Arc<Fanout>,
    session: String,
    materialization: u64,
    recipients: Vec<SubscriberId>,
    completed: bool,
}

impl SessionStreams {
    pub(crate) fn close(mut self) {
        self.close_all();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn accepted_for_test(&self) -> bool {
        !self.recipients.is_empty()
    }

    fn close_all(&mut self) {
        if !self.completed {
            self.fanout
                .complete_terminal(&self.session, self.materialization, &self.recipients);
            self.completed = true;
        }
    }
}

impl Drop for SessionStreams {
    fn drop(&mut self) {
        self.close_all();
    }
}

struct FanoutState {
    /// Terminal once set. Keeping this under the registry lock makes a close
    /// atomic with every registration that could otherwise follow it.
    closed: bool,
    subscribers: HashMap<SubscriberId, Subscriber>,
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
            state: StdMutex::new(FanoutState {
                closed: false,
                subscribers: HashMap::new(),
            }),
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
            .map(|session| {
                (
                    session.clone(),
                    SessionAttach {
                        state: AttachState::Attaching,
                        materialization: None,
                        block_stop: live.block_stop_child(),
                    },
                )
            })
            .collect();
        let mut state = self.lock();
        if state.closed {
            live.close();
        } else {
            state.subscribers.insert(
                id,
                Subscriber {
                    live,
                    attached,
                    accepted_list: None,
                },
            );
        }
        (id, receiver, cancelled)
    }

    pub(crate) fn deregister(&self, id: SubscriberId) {
        if let Some(subscriber) = self.lock().subscribers.remove(&id) {
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
        let (stopped, close) = if let Some(subscriber) = self.lock().subscribers.get_mut(&id) {
            let stopped = subscriber
                .attached
                .remove(session)
                .map(|attached| attached.block_stop);
            // Anything already queued for it goes too. Resolving a session
            // takes a moment (a materialization reads its log), and another
            // client can make it live in that window, so the registration this
            // is undoing may have caught frames of its own.
            subscriber
                .live
                .retain(|frame| frame.session() != Some(session));
            let close = subscriber
                .attached
                .is_empty()
                .then(|| subscriber.live.clone());
            (stopped, close)
        } else {
            (None, None)
        };
        if let Some(stopped) = stopped {
            stopped.cancel();
        }
        if let Some(close) = close {
            // Refusal frames still travel on the attach block. Closing only the
            // live tail lets those drain first, followed by any queued terminal
            // reason and then EOF.
            close.close_after_block();
        }
    }

    /// Bind one registered attach to the materialization it resolved.
    ///
    /// Both draining checks happen under the same fanout lock terminal
    /// publication needs. Since the driver sets `draining` before taking that
    /// lock, either this binding belongs to the outgoing generation and receives
    /// its error, or it is removed before that error can be published.
    pub(crate) fn bind_if_live(
        &self,
        id: SubscriberId,
        session: &str,
        materialization: u64,
        is_draining: impl Fn() -> bool,
    ) -> Option<AttachBlock> {
        let mut state = self.lock();
        let Some(subscriber) = state.subscribers.get_mut(&id) else {
            return None;
        };
        // `stop_blocks` takes this same fanout lock before cancelling the root,
        // so this check orders a bind against host-wide stream shutdown. A root
        // stopped first can never acquire a new per-session block promise.
        if subscriber.live.0.block_stop.is_cancelled() || is_draining() {
            return None;
        }
        let attached = subscriber.attached.get_mut(session)?;
        attached.materialization = Some(materialization);
        if is_draining() {
            attached.materialization = None;
            return None;
        }
        Some(AttachBlock {
            subscriber: id,
            session: session.to_string(),
            materialization,
            block_stop: attached.block_stop.clone(),
        })
    }

    /// Publish a terminal frame and capture exactly the streams that accepted
    /// it under the same registry lock. Registrations after this point neither
    /// miss a promised frame nor get closed as if they had received it.
    pub(crate) fn publish_terminal(
        self: &Arc<Self>,
        frame: Frame,
        materialization: u64,
    ) -> SessionStreams {
        let session = frame
            .session()
            .expect("a terminal persistence frame names its session")
            .to_string();
        let mut recipients = Vec::new();
        let mut stopped = Vec::new();
        {
            let mut state = self.lock();
            state.subscribers.retain(|id, subscriber| {
                let bound = subscriber
                    .attached
                    .get(&session)
                    .is_some_and(|attached| attached.materialization == Some(materialization));
                if !bound {
                    return true;
                }
                let retained = subscriber.live.offer_terminal(frame.clone());
                if retained {
                    recipients.push(*id);
                    let attached = subscriber
                        .attached
                        .get_mut(&session)
                        .expect("the matching attachment remains registered");
                    attached.state = AttachState::Stopped;
                    stopped.push(attached.block_stop.clone());
                }
                retained
            });
        }
        for stopped in stopped {
            stopped.cancel();
        }
        SessionStreams {
            fanout: Arc::clone(self),
            session,
            materialization,
            recipients,
            completed: false,
        }
    }

    fn complete_terminal(&self, session: &str, materialization: u64, recipients: &[SubscriberId]) {
        let mut close = Vec::new();
        let mut stopped = Vec::new();
        {
            let mut state = self.lock();
            for id in recipients {
                let Some(subscriber) = state.subscribers.get_mut(id) else {
                    continue;
                };
                if !subscriber
                    .attached
                    .get(session)
                    .is_some_and(|attached| attached.materialization == Some(materialization))
                {
                    continue;
                }
                let attached = subscriber
                    .attached
                    .remove(session)
                    .expect("the matching attachment remains registered");
                stopped.push(attached.block_stop);
                if subscriber.attached.is_empty() {
                    close.push(subscriber.live.clone());
                }
            }
        }
        for stopped in stopped {
            stopped.cancel();
        }
        for stream in close {
            stream.close_after_block();
        }
    }

    /// Fan `frame` out to every subscriber.
    pub(crate) fn publish(&self, frame: Frame) {
        self.lock()
            .subscribers
            .retain(|_, subscriber| subscriber.offer(&frame));
    }

    /// Fan a directory out to every subscriber that does not already have it
    /// (see [`Subscriber::offer_list`]).
    pub(crate) fn publish_list(&self, sessions: Vec<SessionSummary>) {
        self.lock()
            .subscribers
            .retain(|_, subscriber| subscriber.offer_list(&sessions));
    }

    /// Switch a generation-bound block to live delivery and filter duplicates.
    ///
    /// Completion never inserts state. Terminal cleanup and completion
    /// linearize under the registry lock, so a producer that resumes after its
    /// generation was removed cannot recreate the attachment.
    pub(crate) fn finish_block(&self, block: &AttachBlock, boundary: u64) -> bool {
        let mut state = self.lock();
        let Some(subscriber) = state.subscribers.get_mut(&block.subscriber) else {
            return false;
        };
        let valid = subscriber
            .attached
            .get(&block.session)
            .is_some_and(|attached| {
                attached.materialization == Some(block.materialization)
                    && matches!(attached.state, AttachState::Attaching)
                    && !attached.block_stop.is_cancelled()
            });
        if !valid {
            return false;
        }
        subscriber.live.retain(|frame| {
            frame.session() != Some(block.session.as_str())
                || !frame.durable_seq().is_some_and(|seq| seq <= boundary)
        });
        let attached = subscriber
            .attached
            .get_mut(&block.session)
            .expect("the generation was validated under the registry lock");
        attached.state = AttachState::Live { boundary };
        true
    }

    /// Ask every attach-block producer to stop before host-wide teardown. An
    /// aborted attachment continues into its live queue, where it receives a
    /// later terminal persistence error or the queue's final close.
    pub(crate) fn stop_blocks(&self) {
        for subscriber in self.lock().subscribers.values() {
            subscriber.live.stop_block();
        }
    }

    /// Stop block producers for one materialization only after its driver has
    /// had the chance to queue a terminal persistence error.
    pub(crate) fn stop_session_blocks(&self, session: &str, materialization: u64) {
        let mut stopped = Vec::new();
        {
            for subscriber in self.lock().subscribers.values_mut() {
                let Some(attached) = subscriber.attached.get_mut(session) else {
                    continue;
                };
                if attached.materialization == Some(materialization) {
                    attached.state = AttachState::Stopped;
                    stopped.push(attached.block_stop.clone());
                }
            }
        }
        for stopped in stopped {
            stopped.cancel();
        }
    }

    /// Drop every subscriber, closing its stream.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        for (_, subscriber) in state.subscribers.drain() {
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
            .subscribers
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

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn stream_state_for_test(&self) -> Vec<(usize, bool, usize)> {
        self.lock()
            .subscribers
            .values()
            .map(|subscriber| {
                let queue = subscriber
                    .live
                    .0
                    .state
                    .lock()
                    .expect("live queue mutex poisoned");
                (
                    queue.frames.len(),
                    queue.closed,
                    queue.terminal_sessions.len(),
                )
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, FanoutState> {
        self.state.lock().expect("fanout state mutex poisoned")
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
            // An aborted block still transitions to live delivery. Its owner
            // either queued a terminal persistence error or closes the queue
            // during teardown, so EOF cannot overtake that error.
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
                Err(TryRecvError::Disconnected) => {
                    self.block_done = true;
                }
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
    const MATERIALIZATION: u64 = 1;

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
            lock_generation: None,
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
        fanout.lock().subscribers[&id]
            .live
            .0
            .state
            .lock()
            .expect("live queue mutex poisoned")
            .frames
            .len()
    }

    /// Bind and complete one ordinary test block. Tests about generation races
    /// keep their permits explicitly instead.
    fn finish(fanout: &Fanout, id: SubscriberId, session: &str, boundary: u64) {
        let materialization = if session == SESSION {
            MATERIALIZATION
        } else {
            MATERIALIZATION + 1
        };
        let block = fanout
            .bind_if_live(id, session, materialization, || false)
            .expect("the test block binds");
        assert!(fanout.finish_block(&block, boundary));
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

        finish(&fanout, id, SESSION, 5);

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
        finish(&fanout, id, SESSION, 5);

        fanout.publish(durable(4));
        fanout.publish(durable(5));
        fanout.publish(reliable("after"));
        fanout.publish(durable(6));

        assert_eq!(drained(&mut rx), vec!["warning after", "durable 6"]);
    }

    #[tokio::test]
    async fn a_terminal_frame_survives_spent_capacity_before_eof() {
        let fanout = Arc::new(Fanout::new(NonZeroUsize::new(1)));
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        finish(&fanout, id, SESSION, 0);
        fanout.publish(reliable("already queued"));

        let streams = fanout.publish_terminal(reliable("storage failed"), MATERIALIZATION);
        fanout.publish(reliable("too late"));
        streams.close();

        assert_eq!(
            drained(&mut rx),
            vec!["warning already queued", "warning storage failed"],
            "the terminal reason is admitted once beyond the ordinary bound"
        );
        assert!(
            rx.recv().await.is_none(),
            "terminal queue closes after its frames"
        );
    }

    #[tokio::test]
    async fn a_healthy_siblings_reliable_frame_cannot_erase_a_terminal_error() {
        let fanout = Arc::new(Fanout::new(NonZeroUsize::new(1)));
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        let stopped = live.block_stop_token();
        finish(&fanout, id, SESSION, 0);
        finish(&fanout, id, OTHER, 0);
        let (mut attachment, block) = Attachment::new(
            id,
            live,
            cancelled.clone(),
            vec![SESSION.to_string(), OTHER.to_string()],
            Arc::clone(&fanout),
        );
        drop(block);
        fanout.publish(other(reliable("healthy already queued")));

        let streams = fanout.publish_terminal(reliable("storage failed"), MATERIALIZATION);
        fanout.publish(other(reliable("healthy after the failure")));

        assert!(
            !cancelled.is_cancelled(),
            "healthy sibling traffic evicted the shared stream and erased its terminal error",
        );
        assert!(
            stopped.is_cancelled(),
            "graceful eviction left block work live"
        );
        assert!(
            !fanout.attached(OTHER),
            "overflow left the evicted subscriber registered",
        );
        let first = attachment.recv().await.expect("accepted healthy frame");
        let second = attachment.recv().await.expect("terminal storage frame");
        assert!(
            matches!(first, Frame::Event { ref event, .. } if matches!(event.known(), Some(AgentEvent::Warning { text, .. }) if text == "healthy already queued")),
            "the accepted healthy prefix changed: {first:?}",
        );
        assert!(
            matches!(second, Frame::Event { ref event, .. } if matches!(event.known(), Some(AgentEvent::Warning { text, .. }) if text == "storage failed")),
            "the terminal frame did not follow the accepted prefix: {second:?}",
        );
        assert!(
            attachment.recv().await.is_none(),
            "the retained terminal tail must end in EOF",
        );
        streams.close();
    }

    #[tokio::test]
    async fn a_terminal_tail_closes_when_its_last_unresolved_sibling_detaches() {
        let fanout = Arc::new(Fanout::default());
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        finish(&fanout, id, SESSION, 0);

        fanout
            .publish_terminal(reliable("storage failed"), MATERIALIZATION)
            .close();
        fanout.detach(id, OTHER);

        assert_eq!(drained(&mut rx), vec!["warning storage failed"]);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("the terminal tail reaches EOF")
                .is_none(),
        );
    }

    #[test]
    fn an_unbound_attach_is_not_terminalized_by_the_generation_it_never_resolved() {
        let fanout = Arc::new(Fanout::default());
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);

        fanout
            .publish_terminal(reliable("old generation failed"), MATERIALIZATION)
            .close();
        let replacement = fanout
            .bind_if_live(id, SESSION, MATERIALIZATION + 1, || false)
            .expect("the registration remains available for replacement binding");
        assert!(fanout.finish_block(&replacement, 0));
        fanout.publish(Frame::Event {
            session: SESSION.to_string(),
            epoch: "replacement-epoch".to_string(),
            durability: None,
            event: AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "replacement is live".to_string(),
            }
            .into(),
        });

        assert_eq!(drained(&mut rx), vec!["warning replacement is live"]);
        let state = rx.0.state.lock().expect("live queue mutex poisoned");
        assert!(!state.closed && state.terminal_sessions.is_empty());
    }

    #[test]
    fn outgoing_block_stop_does_not_cancel_an_unbound_replacement_attach() {
        let fanout = Fanout::default();
        let (id, rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        let block_stop = rx.block_stop_token();

        fanout.stop_session_blocks(SESSION, MATERIALIZATION);
        assert!(
            !block_stop.is_cancelled(),
            "the outgoing owner canceled an unbound replacement"
        );
        let replacement = fanout
            .bind_if_live(id, SESSION, MATERIALIZATION + 1, || false)
            .expect("the replacement binds");
        fanout.stop_session_blocks(SESSION, MATERIALIZATION);
        assert!(
            !block_stop.is_cancelled(),
            "the outgoing owner canceled a bound replacement"
        );
        fanout.stop_session_blocks(SESSION, MATERIALIZATION + 1);
        assert!(
            replacement.stopped().is_cancelled(),
            "the bound owner can stop its own block"
        );
        assert!(
            !block_stop.is_cancelled(),
            "a session stop must not become stream-wide"
        );
    }

    #[test]
    fn one_sessions_block_stop_does_not_cancel_another_sessions_block() {
        let fanout = Fanout::default();
        let (id, rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        let block_stop = rx.block_stop_token();
        let failed = fanout
            .bind_if_live(id, SESSION, MATERIALIZATION, || false)
            .expect("the failed session binds");
        let healthy = fanout
            .bind_if_live(id, OTHER, MATERIALIZATION + 1, || false)
            .expect("the healthy session binds");

        fanout.stop_session_blocks(SESSION, MATERIALIZATION);

        assert!(
            !block_stop.is_cancelled(),
            "stopping one failed session also canceled the healthy session's promised attach block",
        );
        assert!(failed.stopped().is_cancelled());
        assert!(!healthy.stopped().is_cancelled());
        assert!(
            !fanout.finish_block(&failed, 0),
            "a canceled failed-generation block completed anyway",
        );
        assert!(
            fanout.finish_block(&healthy, 0),
            "the healthy sibling could not complete its promised block",
        );
    }

    #[test]
    fn a_late_block_completion_cannot_restore_a_terminalized_generation() {
        let fanout = Arc::new(Fanout::default());
        let (id, _rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        let failed = fanout
            .bind_if_live(id, SESSION, MATERIALIZATION, || false)
            .expect("the failed session binds");
        let healthy = fanout
            .bind_if_live(id, OTHER, MATERIALIZATION + 1, || false)
            .expect("the healthy session binds");
        assert!(fanout.finish_block(&healthy, 0));

        fanout
            .publish_terminal(reliable("failed"), MATERIALIZATION)
            .close();
        assert!(
            !fanout.finish_block(&failed, 0),
            "the stale generation completed after terminal cleanup",
        );

        assert!(
            !fanout.attached(SESSION),
            "the old producer restored attachment state after terminal cleanup removed its generation",
        );
        assert!(
            fanout.attached(OTHER),
            "terminal cleanup removed the sibling"
        );
    }

    #[test]
    fn a_shared_stream_keeps_each_sessions_terminal_error() {
        let fanout = Arc::new(Fanout::default());
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        finish(&fanout, id, SESSION, 0);
        finish(&fanout, id, OTHER, 0);

        let first = fanout.publish_terminal(reliable("first failed"), MATERIALIZATION);
        first.close();
        let second = fanout.publish_terminal(
            Frame::Event {
                session: OTHER.to_string(),
                epoch: EPOCH.to_string(),
                durability: None,
                event: AgentEvent::Warning {
                    agent_id: AgentId::Main,
                    text: "second failed".to_string(),
                }
                .into(),
            },
            MATERIALIZATION + 1,
        );
        second.close();

        assert_eq!(
            drained(&mut rx),
            vec!["warning first failed", "warning second failed"]
        );
    }

    #[test]
    fn binding_that_observes_drain_on_its_second_check_stays_unbound() {
        let fanout = Arc::new(Fanout::default());
        let (id, rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        let checks = AtomicU64::new(0);

        assert!(
            fanout
                .bind_if_live(id, SESSION, MATERIALIZATION, || {
                    checks.fetch_add(1, Ordering::Relaxed) > 0
                })
                .is_none(),
            "a drain beginning during binding withdraws the old generation"
        );
        fanout
            .publish_terminal(reliable("old generation failed"), MATERIALIZATION)
            .close();

        let state = rx.0.state.lock().expect("live queue mutex poisoned");
        assert!(state.frames.is_empty() && !state.closed && state.terminal_sessions.is_empty());
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
            lock_generation: None,
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
    /// or not. Suppression records what the queue accepted, so a lossy frame the
    /// bound turned away does not count as sent.
    #[test]
    fn a_dropped_directory_is_offered_again() {
        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        finish(&fanout, id, SESSION, 0);

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

    /// A snapshot that restores the last delivered value is still offered when
    /// a different one was accepted in between (spec 6.8).
    ///
    /// `list` coalescing can replace a queued change with the restore before
    /// either is drained. Comparing the restore against what was delivered says
    /// "unchanged" and skips it, leaving the queued value as the newest answer.
    /// Comparing against the latest accepted row says it changed back and
    /// replaces the queued change with the restore, so the newest cumulative
    /// answer still reaches the client.
    #[test]
    fn a_restore_is_compared_against_the_latest_accepted_directory() {
        let fanout = Fanout::default();
        let (_id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);

        let free = directory(1);
        let held = directory(2);
        fanout.publish_list(free.clone());
        let Some(Frame::List { sessions, .. }) = rx.try_recv() else {
            panic!("the baseline directory was not queued");
        };
        assert_eq!(sessions, free, "the client did not receive the baseline");

        // The subscriber stops draining. The rise is accepted, then the fall
        // restores the baseline before the rise can be delivered.
        fanout.publish_list(held);
        fanout.publish_list(free.clone());

        let Some(Frame::List { sessions, .. }) = rx.try_recv() else {
            panic!("the restored directory was suppressed as already delivered");
        };
        assert_eq!(
            sessions, free,
            "coalescing did not leave the latest cumulative snapshot",
        );
        assert!(
            rx.try_recv().is_none(),
            "the superseded intermediate snapshot was delivered too",
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
        finish(&fanout, id, SESSION, 5);
        finish(&fanout, id, OTHER, 1);

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
        finish(&fanout, id, SESSION, 0);
        finish(&fanout, id, OTHER, 0);
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

        finish(&fanout, id, SESSION, 0);

        assert_eq!(
            drained(&mut rx),
            vec!["error unknown_session"],
            "a lossy frame published during an attach would have been dropped",
        );

        let fanout = Fanout::new(NonZeroUsize::new(2));
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        finish(&fanout, id, SESSION, 0);
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

        finish(&fanout, id, SESSION, 0);

        assert_eq!(drained(&mut rx), vec!["warning held", "reset"]);
    }

    /// Replacing a lossy frame removes the old one and appends the new one at
    /// the tail, so it cannot jump a reliable boundary.
    #[test]
    fn lossy_replacement_moves_to_the_queue_tail() {
        let fanout = Fanout::new(NonZeroUsize::new(3));
        let (id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);
        finish(&fanout, id, SESSION, 0);

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
        finish(&fanout, id, SESSION, 0);
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
        assert!(
            fanout.lock().subscribers.is_empty(),
            "the subscriber was evicted"
        );
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
        finish(&fanout, id, SESSION, 0);

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
        finish(&fanout, id, SESSION, 0);
        finish(&fanout, id, OTHER, 0);

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
        finish(&fanout, id, SESSION, 0);

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

    /// Closing the registry is terminal. A registration that begins after it
    /// returns receives a live channel already at EOF rather than recreating a
    /// subscriber that no later close can reach.
    #[tokio::test]
    async fn a_registration_after_close_is_already_closed() {
        let fanout = Fanout::default();
        fanout.close();

        let (_id, mut rx, _cancelled) = fanout.register(&[SESSION.to_string()]);

        assert!(
            fanout.lock().subscribers.is_empty(),
            "a closed fanout retains no late subscriber"
        );
        let ended = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("the late receiver is closed synchronously");
        assert!(ended.is_none(), "a late receiver starts at EOF");
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
        assert_eq!(fanout.lock().subscribers.len(), 1);

        drop(attachment);

        assert!(fanout.lock().subscribers.is_empty());
    }
}
