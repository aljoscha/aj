//! The subscriber registry and the fan-out into it.
//!
//! One subscriber is one stream: a producer-paced attach channel, a bounded
//! live queue, and the backfill boundary for each attached session.
//!
//! Publishers only touch the bounded live queue and never await. The attach
//! producer awaits its separate capacity-one channel, so HTTP backpressure
//! paces a large backfill without ever stalling a session driver.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_wire::Frame;
use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_util::sync::CancellationToken;

/// Enough burst room for normal clients while bounding a stalled stream.
const DEFAULT_LIVE_CAPACITY: usize = 256;

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
}

impl Subscriber {
    /// Queue `frame` for this subscriber, applying the attach rules of the
    /// session it belongs to.
    fn offer(&mut self, frame: &Frame) -> bool {
        let Some(session) = frame.session() else {
            // Host-level frames (`list`, `heartbeat`) belong to the
            // connection, not to a session, so no attach state gates them.
            return self.live.offer(frame.clone());
        };
        match self.attached.get_mut(session) {
            None => {
                // Spec 6.5: an unattached session still produces durable
                // and reliable-transient frames, but its lossy frames may
                // be suppressed, which keeps host-wide streaming churn off
                // a client that is not watching it.
                if !frame.is_lossy() {
                    return self.live.offer(frame.clone());
                }
                true
            }
            Some(AttachState::Attaching) => {
                if !frame.is_lossy() {
                    return self.live.offer(frame.clone());
                }
                true
            }
            Some(AttachState::Live { boundary }) => {
                if frame.durable_seq().is_some_and(|seq| seq <= *boundary) {
                    return true;
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
        Frame::CaughtUp { .. } | Frame::Reset { .. } | Frame::Heartbeat => None,
    }
}

struct LiveQueueState {
    frames: VecDeque<Frame>,
    closed: bool,
}

struct LiveQueue {
    capacity: usize,
    state: StdMutex<LiveQueueState>,
    ready: Notify,
    cancelled: CancellationToken,
}

#[derive(Clone)]
struct LiveSender(Arc<LiveQueue>);

pub(crate) struct LiveReceiver(Arc<LiveQueue>);

fn live_channel(capacity: usize) -> (LiveSender, LiveReceiver, CancellationToken) {
    assert!(capacity > 0, "live queue capacity must be non-zero");
    let cancelled = CancellationToken::new();
    let queue = Arc::new(LiveQueue {
        capacity,
        state: StdMutex::new(LiveQueueState {
            frames: VecDeque::with_capacity(capacity),
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

impl LiveSender {
    /// Queues a frame without blocking. Reliable overflow closes the stream.
    fn offer(&self, frame: Frame) -> bool {
        let key = lossy_key(&frame);
        let mut state = self.0.state.lock().expect("live queue mutex poisoned");
        if state.closed {
            return false;
        }
        if let Some(key) = key {
            if let Some(index) = state
                .frames
                .iter()
                .position(|queued| lossy_key(queued).as_ref() == Some(&key))
            {
                state.frames.remove(index);
            } else if state.frames.len() >= self.0.capacity {
                return true;
            }
        } else if state.frames.len() >= self.0.capacity {
            state.frames.clear();
            state.closed = true;
            drop(state);
            self.0.cancelled.cancel();
            self.0.ready.notify_waiters();
            return false;
        }
        state.frames.push_back(frame);
        drop(state);
        self.0.ready.notify_one();
        true
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
    live_capacity: usize,
}

impl Default for Fanout {
    fn default() -> Self {
        Self {
            subscribers: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            list_dirty: Notify::new(),
            live_capacity: DEFAULT_LIVE_CAPACITY,
        }
    }
}

impl Fanout {
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
        self.lock().insert(id, Subscriber { live, attached });
        (id, receiver, cancelled)
    }

    pub(crate) fn deregister(&self, id: SubscriberId) {
        if let Some(subscriber) = self.lock().remove(&id) {
            subscriber.live.evict();
        }
    }

    /// Fan `frame` out to every subscriber.
    pub(crate) fn publish(&self, frame: Frame) {
        self.lock().retain(|_, subscriber| subscriber.offer(&frame));
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
    pub(crate) fn new(
        id: SubscriberId,
        block: Receiver<Frame>,
        live: LiveReceiver,
        cancelled: CancellationToken,
        attached: Vec<String>,
        fanout: Arc<Fanout>,
    ) -> Self {
        Self {
            id,
            block,
            block_done: false,
            live,
            cancelled,
            attached,
            fanout,
        }
    }

    /// The sessions this stream was served an attach block for.
    ///
    /// A client arms its fold from this rather than from what it asked for:
    /// an attach that was refused (a lock conflict, an unknown session)
    /// returns no `Attachment` at all, so nothing here can name a session
    /// whose block will not arrive.
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
    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};

    use super::*;

    const SESSION: &str = "session-1";
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
                    other => format!("event {other:?}"),
                },
                Frame::State { last_seq, .. } => format!("state {last_seq}"),
                Frame::CaughtUp { last_seq, .. } => format!("caught_up {last_seq}"),
                Frame::Reset { .. } => "reset".to_string(),
                Frame::List { .. } => "list".to_string(),
                other => format!("{other:?}"),
            });
        }
        out
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

    /// A session this stream did not attach still produces its durable and
    /// reliable-transient frames, but not its lossy ones (spec 6.5).
    #[test]
    fn an_unattached_session_delivers_everything_but_its_lossy_frames() {
        let fanout = Fanout::default();
        let (_id, mut rx, _cancelled) = fanout.register(&[]);

        fanout.publish(durable(1));
        fanout.publish(reliable("kept"));
        fanout.publish(lossy(0));
        fanout.publish(Frame::List {
            sessions: Vec::new(),
        });

        assert_eq!(
            drained(&mut rx),
            vec!["durable 1", "warning kept", "list"],
            "the host-level list frame belongs to the connection, not a session",
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
        let fanout = Fanout {
            live_capacity: 3,
            ..Fanout::default()
        };
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
        let fanout = Fanout {
            live_capacity: 2,
            ..Fanout::default()
        };
        let (id, mut rx, cancelled) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(id, SESSION, 0);
        fanout.publish(reliable("one"));
        fanout.publish(reliable("two"));
        fanout.publish(Frame::List {
            sessions: Vec::new(),
        });
        assert!(!cancelled.is_cancelled(), "lossy overflow is only dropped");

        fanout.publish(reliable("three"));
        assert!(cancelled.is_cancelled());
        assert!(fanout.lock().is_empty(), "the subscriber was evicted");
        assert!(drained(&mut rx).is_empty(), "eviction closes and clears");
    }

    /// The attach channel has capacity one and live frames remain hidden until
    /// its sender closes.
    #[test]
    fn attachment_is_producer_paced_and_reads_the_block_before_live() {
        let fanout = Arc::new(Fanout::default());
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string()]);
        let (block_tx, block_rx) = tokio::sync::mpsc::channel(1);
        let mut attachment = Attachment::new(
            id,
            block_rx,
            live,
            cancelled,
            vec![SESSION.to_string()],
            Arc::clone(&fanout),
        );
        block_tx.try_send(lossy(1)).expect("first block frame");
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

    /// Dropping the stream deregisters it, so the host stops paying for a
    /// client that went away.
    #[test]
    fn dropping_an_attachment_deregisters_it() {
        let fanout = Arc::new(Fanout::default());
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string()]);
        let (_block_tx, block_rx) = tokio::sync::mpsc::channel(1);
        let attachment = Attachment::new(
            id,
            block_rx,
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
