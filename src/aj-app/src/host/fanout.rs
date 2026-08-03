//! The subscriber registry and the fan-out into it.
//!
//! One subscriber is one stream: a queue of frames plus the set of
//! sessions it attached, each with the backfill boundary that tells the
//! fan-out which live durable frames the attach block already covered.
//!
//! Every send happens under the registry lock. That is what makes an
//! attach block contiguous: the block's frames and the switch out of
//! [`AttachState::Attaching`] land in one critical section, so no live
//! frame can be spliced into the middle of it. The sends themselves are
//! pushes onto unbounded queues and never block, so holding the lock
//! across them is safe. Bounded queues with coalescing and eviction
//! (spec 6.9) replace the unbounded ones later; this is the seam they
//! slot into.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use aj_wire::Frame;
use tokio::sync::Notify;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Identity of one attached subscriber within a host.
pub(crate) type SubscriberId = u64;

/// A subscriber's view of one session it attached.
enum AttachState {
    /// The attach block has not been written yet, so live frames are held
    /// to keep the block contiguous and ordered on this stream.
    ///
    /// Held lossy frames are dropped rather than flushed (spec 6.5): a
    /// cumulative snapshot delivered after the durable frame that
    /// superseded it resurrects stale transient state, and a
    /// `MessageUpdate` for a message the backfill already finalized would
    /// paint a second, unfinalized copy of it. The cost is at most one
    /// coalescing tick of streaming text, which the next live snapshot
    /// restores.
    Attaching { held: Vec<Frame> },
    /// Live delivery. A durable frame at or below `boundary` is already in
    /// the backfill this stream was served, so it is dropped rather than
    /// re-delivered.
    Live { boundary: u64 },
}

struct Subscriber {
    frames: UnboundedSender<Frame>,
    attached: HashMap<String, AttachState>,
}

impl Subscriber {
    /// Queue `frame` for this subscriber, applying the attach rules of the
    /// session it belongs to.
    fn offer(&mut self, frame: &Frame) {
        let Some(session) = frame.session() else {
            // Host-level frames (`list`, `heartbeat`) belong to the
            // connection, not to a session, so no attach state gates them.
            let _ = self.frames.send(frame.clone());
            return;
        };
        match self.attached.get_mut(session) {
            None => {
                // Spec 6.5: an unattached session still produces durable
                // and reliable-transient frames, but its lossy frames may
                // be suppressed, which keeps host-wide streaming churn off
                // a client that is not watching it.
                if !frame.is_lossy() {
                    let _ = self.frames.send(frame.clone());
                }
            }
            Some(AttachState::Attaching { held }) => {
                if !frame.is_lossy() {
                    held.push(frame.clone());
                }
            }
            Some(AttachState::Live { boundary }) => {
                if frame.durable_seq().is_some_and(|seq| seq <= *boundary) {
                    return;
                }
                let _ = self.frames.send(frame.clone());
            }
        }
    }
}

/// The host's subscriber registry.
pub(crate) struct Fanout {
    subscribers: StdMutex<HashMap<SubscriberId, Subscriber>>,
    next_id: AtomicU64,
    /// Pinged whenever the session directory changed. The list publisher
    /// coalesces on it (spec 6.8).
    list_dirty: Notify,
}

impl Default for Fanout {
    fn default() -> Self {
        Self {
            subscribers: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            list_dirty: Notify::new(),
        }
    }
}

impl Fanout {
    /// Register a subscriber that is about to be served attach blocks for
    /// `sessions`.
    ///
    /// Registration happens before the blocks are projected, which is what
    /// makes an attach atomic with respect to the session's event flow:
    /// every frame published from here on is either held for the block or
    /// filtered against its boundary, so none can be missed.
    pub(crate) fn register(&self, sessions: &[String]) -> (SubscriberId, UnboundedReceiver<Frame>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = unbounded_channel();
        let attached = sessions
            .iter()
            .map(|session| (session.clone(), AttachState::Attaching { held: Vec::new() }))
            .collect();
        self.lock().insert(
            id,
            Subscriber {
                frames: tx,
                attached,
            },
        );
        (id, rx)
    }

    pub(crate) fn deregister(&self, id: SubscriberId) {
        self.lock().remove(&id);
    }

    /// Fan `frame` out to every subscriber.
    pub(crate) fn publish(&self, frame: Frame) {
        for subscriber in self.lock().values_mut() {
            subscriber.offer(&frame);
        }
    }

    /// Write `block` to `id` and switch that session to live delivery at
    /// `boundary`.
    ///
    /// One critical section, so the block stays contiguous: the held
    /// frames are flushed behind it, and a concurrently published frame
    /// waits for this lock and lands after.
    pub(crate) fn deliver_block(
        &self,
        id: SubscriberId,
        session: &str,
        block: Vec<Frame>,
        boundary: u64,
    ) {
        let mut subscribers = self.lock();
        let Some(subscriber) = subscribers.get_mut(&id) else {
            // The stream was dropped while its block was being projected.
            return;
        };
        for frame in block {
            let _ = subscriber.frames.send(frame);
        }
        let held = match subscriber.attached.remove(session) {
            Some(AttachState::Attaching { held }) => held,
            // The caller registers a session before serving it, so this is
            // unreachable unless the same session were served twice.
            _ => Vec::new(),
        };
        for frame in held {
            if frame.durable_seq().is_some_and(|seq| seq <= boundary) {
                continue;
            }
            let _ = subscriber.frames.send(frame);
        }
        subscriber
            .attached
            .insert(session.to_string(), AttachState::Live { boundary });
    }

    /// Forget every stream's backfill boundary for `session`, because its
    /// seqs no longer describe the history the host will serve.
    ///
    /// A head switch keeps the session id but mints a new epoch, and a
    /// boundary from the old epoch would silently drop the new epoch's
    /// low-numbered frames. Clients re-attach on the `reset` that follows;
    /// until they do, their own epoch filter is what keeps the new history
    /// out of the old one's transcript.
    pub(crate) fn reset_boundaries(&self, session: &str) {
        for subscriber in self.lock().values_mut() {
            if let Some(state) = subscriber.attached.get_mut(session) {
                *state = AttachState::Live { boundary: 0 };
            }
        }
    }

    /// Drop every subscriber, closing its stream.
    pub(crate) fn close(&self) {
        self.lock().clear();
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
    frames: UnboundedReceiver<Frame>,
    fanout: Arc<Fanout>,
}

impl Attachment {
    pub(crate) fn new(
        id: SubscriberId,
        frames: UnboundedReceiver<Frame>,
        fanout: Arc<Fanout>,
    ) -> Self {
        Self { id, frames, fanout }
    }

    /// The next frame, or `None` once the host closed the stream.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.frames.recv().await
    }

    /// The next already-queued frame, without waiting. `None` both when
    /// the queue is empty and when the stream closed, which a caller
    /// draining what it has does not need to distinguish.
    pub fn try_recv(&mut self) -> Option<Frame> {
        match self.frames.try_recv() {
            Ok(frame) => Some(frame),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
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
