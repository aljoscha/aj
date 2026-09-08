//! Bounded outbound delivery for hosts and gateways.
//!
//! Live offers never wait: cumulative snapshots coalesce at the tail or drop
//! at capacity, while reliable overflow evicts the client. Paced attach frames
//! share the FIFO without coalescing or consuming live capacity. They wait for
//! room instead, so a large backfill cannot evict the client that requested it.
//!
//! Stream ownership stays with the caller. Dropping a queue handle does not
//! close it, and the supplied cancellation token stops paced waits but does not
//! discard queued frames. Owners handle cancellation of their readers.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_wire::{DecodedFrame, Frame};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A typed or retained-wire frame, inspected without changing its payload.
pub trait OutboundFrame {
    /// The known frame used for classification, or `None` for an unknown kind.
    /// Unknown kinds are reliable and cannot be coalesced or dropped.
    fn known_frame(&self) -> Option<&Frame>;
}

impl OutboundFrame for Frame {
    fn known_frame(&self) -> Option<&Frame> {
        Some(self)
    }
}

impl OutboundFrame for DecodedFrame {
    fn known_frame(&self) -> Option<&Frame> {
        match self {
            Self::Known(frame) => Some(frame.value()),
            Self::Unknown { .. } => None,
        }
    }
}

/// Make one client's queue with `capacity` live slots. Eviction cancels
/// `cancelled`, allowing the owner to stop the producers feeding this client.
pub fn channel<T: OutboundFrame>(
    capacity: NonZeroUsize,
    cancelled: CancellationToken,
) -> (Sender<T>, Receiver<T>) {
    let queue = Arc::new(Queue {
        capacity,
        state: Mutex::new(State {
            frames: VecDeque::new(),
            live: 0,
            closed: false,
        }),
        ready: Notify::new(),
        room: Notify::new(),
        cancelled,
    });
    (Sender(Arc::clone(&queue)), Receiver(queue))
}

struct Queue<T> {
    capacity: NonZeroUsize,
    state: Mutex<State<T>>,
    ready: Notify,
    room: Notify,
    cancelled: CancellationToken,
}

struct State<T> {
    frames: VecDeque<Queued<T>>,
    /// Counts only live entries, not paced frames. Every insertion, removal and
    /// clear maintains this count so live admission does not scan to find room.
    live: usize,
    closed: bool,
}

struct Queued<T> {
    frame: T,
    /// Only live frames consume the bound and may be superseded. A paced
    /// opening state must survive even if a live snapshot of its key arrives.
    live: bool,
}

/// A producing handle. Clones feed the same client's queue.
pub struct Sender<T>(Arc<Queue<T>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// The sole receiving handle for a client's queue.
pub struct Receiver<T>(Arc<Queue<T>>);

/// What became of a live offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    /// Queued, replacing any older live snapshot of the same key.
    Queued,
    /// A snapshot met the live bound. The client stays connected.
    Dropped,
    /// Reliable overflow evicted the client, or the queue was already closed.
    Evicted,
}

impl<T: OutboundFrame> Sender<T> {
    /// Offer a live frame without waiting. Replacement moves to the tail so a
    /// newer snapshot cannot overtake an intervening reliable frame.
    pub fn offer(&self, frame: T) -> Offered {
        let key = lossy_key(&frame);
        let mut state = self.0.lock();
        if state.closed {
            return Offered::Evicted;
        }
        if let Some(key) = key {
            if let Some(index) = state
                .frames
                .iter()
                .position(|queued| queued.live && lossy_key(&queued.frame).as_ref() == Some(&key))
            {
                state.frames.remove(index);
                state.live -= 1;
            } else if state.live >= self.0.capacity.get() {
                return Offered::Dropped;
            }
        } else if state.live >= self.0.capacity.get() {
            self.0.evict(state);
            return Offered::Evicted;
        }
        state.frames.push_back(Queued { frame, live: true });
        state.live += 1;
        drop(state);
        self.0.ready.notify_one();
        Offered::Queued
    }

    /// Queue an attach frame in order, without coalescing. Wait for total queue
    /// length to fall below capacity rather than spending live slots. Together
    /// the two classes hold at most twice the capacity, with full live headroom
    /// even while a backfill is stalled.
    ///
    /// Returns `false` if the queue closes or a wait is cancelled. Cancellation
    /// does not prevent immediate admission when room is already available.
    pub async fn send_paced(&self, frame: T) -> bool {
        loop {
            // Construct before inspecting the queue: notify_waiters observes
            // even a notification between construction and the first poll.
            let room = self.0.room.notified();
            {
                let mut state = self.0.lock();
                if state.closed {
                    return false;
                }
                if state.frames.len() < self.0.capacity.get() {
                    state.frames.push_back(Queued { frame, live: false });
                    drop(state);
                    self.0.ready.notify_one();
                    return true;
                }
            }
            tokio::select! {
                _ = self.0.cancelled.cancelled() => return false,
                _ = room => {}
            }
        }
    }

    /// Remove queued frames that no longer belong to this attachment, preserving
    /// the order of the rest and returning their room to producers.
    pub fn retain(&self, mut keep: impl FnMut(&T) -> bool) {
        let mut state = self.0.lock();
        let State { frames, live, .. } = &mut *state;
        frames.retain(|queued| {
            let kept = keep(&queued.frame);
            if !kept && queued.live {
                *live -= 1;
            }
            kept
        });
        drop(state);
        self.0.room.notify_waiters();
    }

    /// Stop admission and wake waiters, leaving queued frames to drain. Does not
    /// cancel the owner's token. The receiver ends once the queue is empty.
    pub fn close(&self) {
        self.0.lock().closed = true;
        self.0.ready.notify_waiters();
        self.0.room.notify_waiters();
    }

    /// Discard the queued prefix, stop admission, and cancel the client. Recovery
    /// belongs to the client's ordinary reattachment with its own cursor.
    pub fn evict(&self) {
        self.0.evict(self.0.lock());
    }
}

// A mutable receiver enforces one reader even though the mutex makes concurrent
// reads memory-safe. Competing readers could apply a session's frames in reverse.
#[allow(clippy::needless_pass_by_ref_mut)]
impl<T> Receiver<T> {
    /// Read the next frame, or `None` after closure and draining. Dropping a
    /// pending receive does not consume a frame.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut state = self.0.lock();
                if let Some(frame) = state.pop() {
                    drop(state);
                    self.0.room.notify_waiters();
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            ready.await;
        }
    }

    /// Take an already-queued frame. `None` means empty, whether open or closed.
    pub fn try_recv(&mut self) -> Option<T> {
        let frame = self.0.lock().pop();
        if frame.is_some() {
            self.0.room.notify_waiters();
        }
        frame
    }
}

impl<T> State<T> {
    fn pop(&mut self) -> Option<T> {
        let queued = self.frames.pop_front()?;
        if queued.live {
            self.live -= 1;
        }
        Some(queued.frame)
    }
}

impl<T> Queue<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().expect("outbound queue mutex poisoned")
    }

    fn evict(&self, mut state: MutexGuard<'_, State<T>>) {
        state.frames.clear();
        state.live = 0;
        state.closed = true;
        drop(state);
        self.cancelled.cancel();
        self.ready.notify_waiters();
    }
}

/// Identity of a cumulative snapshot. Unknown frame and event kinds have no
/// key, so newer peers' one-shot frames cannot be silently lost.
#[derive(PartialEq, Eq)]
enum LossyKey {
    Message(String, AgentId),
    Tool(String, String),
    Task(String, TaskId),
    State(String),
    List,
    Vms,
}

fn lossy_key(frame: &impl OutboundFrame) -> Option<LossyKey> {
    match frame.known_frame()? {
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
