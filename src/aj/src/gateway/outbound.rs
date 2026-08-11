//! One client stream's bounded outbound queue (spec 6.9).
//!
//! The policy is the host's own fan-out, and deliberately not a second one: a
//! bounded queue per attached client, lossy frames coalesced by their key and
//! dropped when the bound is reached, durable and reliable-transient frames
//! never dropped, and a client whose queue overflows with them evicted rather
//! than buffered without bound. Recovery is the ordinary re-attach with a
//! cursor, which is what makes eviction safe.
//!
//! One exception, for the same reason the host makes it: the frames of an attach
//! block are **paced** ([`Sender::send_paced`]) rather than measured against the
//! bound. A backfill bigger than the queue would otherwise evict the very client
//! that asked for it, and the re-attach that follows would do the same again.
//! Pacing a block means the upstream connection stalls until the client reads,
//! which is ordinary HTTP backpressure with the host's own producer at the far
//! end of it.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex as StdMutex};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::TaskId;
use aj_wire::{DecodedFrame, Frame};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The queue of one client stream, with `capacity` frames of slack.
///
/// `cancelled` is the stream's own token: an eviction cancels it, which is what
/// ends the tasks feeding this queue as well as the stream reading it.
pub(crate) fn channel(capacity: NonZeroUsize, cancelled: CancellationToken) -> (Sender, Receiver) {
    let queue = Arc::new(Queue {
        capacity,
        state: StdMutex::new(State {
            frames: VecDeque::new(),
            closed: false,
        }),
        ready: Notify::new(),
        room: Notify::new(),
        cancelled,
    });
    (Sender(Arc::clone(&queue)), Receiver(queue))
}

struct Queue {
    capacity: NonZeroUsize,
    state: StdMutex<State>,
    /// Woken when a frame is queued: the stream writer waits on this.
    ready: Notify,
    /// Woken when a frame is taken: a paced producer waits on this.
    room: Notify,
    cancelled: CancellationToken,
}

struct State {
    frames: VecDeque<Queued>,
    /// Set by an eviction, which also clears the frames: what a client did not
    /// get, it will get again from the backfill of its re-attach.
    closed: bool,
}

/// One frame waiting for the client.
struct Queued {
    frame: DecodedFrame,
    /// Whether a newer snapshot of the same lossy key may take this one's place.
    ///
    /// False for the frames of an attach block. They have to reach the client in
    /// the order the host wrote them: the block opens with the `state` frame the
    /// client adopts the session's epoch from (spec 6.5), so a live snapshot that
    /// coalesced it away would leave the client applying a block under no epoch
    /// at all. A newer snapshot queues behind the block instead.
    coalescible: bool,
}

/// The producing half, held by every task feeding one client stream.
#[derive(Clone)]
pub(crate) struct Sender(Arc<Queue>);

/// The consuming half, held by the client's stream and by nothing else.
pub(crate) struct Receiver(Arc<Queue>);

/// What became of an offered frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offered {
    /// The client has it: queued, or coalesced onto a queued frame of the same
    /// lossy key.
    Queued,
    /// A cumulative snapshot met the bound. The client stays: a newer snapshot
    /// supersedes this one anyway (spec 6.4).
    Dropped,
    /// The client is gone: its queue overflowed with frames that may not be
    /// dropped, or it had already left.
    Evicted,
}

impl Sender {
    /// Queue `frame` without waiting.
    ///
    /// For live frames, which is everything after a session's `caught_up`.
    pub(crate) fn offer(&self, frame: DecodedFrame) -> Offered {
        let key = lossy_key(&frame);
        let mut state = self.0.lock();
        if state.closed {
            return Offered::Evicted;
        }
        if let Some(key) = key {
            let superseded = state.frames.iter().position(|queued| {
                queued.coalescible && lossy_key(&queued.frame).as_ref() == Some(&key)
            });
            if let Some(index) = superseded {
                // Dropped and re-enqueued at the tail rather than substituted in
                // place: in-place substitution would reorder content across a
                // queued durable boundary (spec 6.9).
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
        state.frames.push_back(Queued {
            frame,
            coalescible: true,
        });
        drop(state);
        self.0.ready.notify_one();
        Offered::Queued
    }

    /// Queue `frame`, waiting for room rather than evicting.
    ///
    /// For the frames of an attach block (see the module docs). They are queued
    /// in the order they arrived and never coalesced, in either direction: the
    /// block is a sequence the client applies as one.
    ///
    /// Answers whether the client is still there: `false` once it left or its
    /// stream was cancelled, with the frame undelivered.
    pub(crate) async fn send_paced(&self, frame: DecodedFrame) -> bool {
        loop {
            // Interest is registered before the queue is inspected, so a frame
            // taken in between wakes this rather than being missed.
            let room = self.0.room.notified();
            {
                let mut state = self.0.lock();
                if state.closed {
                    return false;
                }
                if state.frames.len() < self.0.capacity.get() {
                    state.frames.push_back(Queued {
                        frame,
                        coalescible: false,
                    });
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
}

// The queue is behind a mutex, so receiving needs no `&mut` to be sound. It
// takes it to say there is one reader: two tasks receiving concurrently would
// interleave a session's frames, and the stream's whole contract is that they
// arrive in order.
#[allow(clippy::needless_pass_by_ref_mut)]
impl Receiver {
    /// The next frame, `None` once this client was evicted.
    ///
    /// A queue with no producers left simply stays quiet: an upstream that ended
    /// says so with `reset`, and the stream carrying it still has the merged
    /// directory to write.
    pub(crate) async fn recv(&mut self) -> Option<DecodedFrame> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut state = self.0.lock();
                if let Some(queued) = state.frames.pop_front() {
                    drop(state);
                    self.0.room.notify_waiters();
                    return Some(queued.frame);
                }
                if state.closed {
                    return None;
                }
            }
            ready.await;
        }
    }
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the outbound queue mutex is poisoned")
    }
}

/// Identity of one cumulative snapshot in the queue (spec 6.4).
///
/// The classes are the protocol's, so this mirrors the host's own fan-out. The
/// session ids in it are the namespaced ones, because a frame is rewritten
/// before it is queued.
#[derive(Debug, PartialEq, Eq)]
enum LossyKey {
    Message(String, AgentId),
    Tool(String, String),
    Task(String, TaskId),
    State(String),
    List,
    Vms,
}

/// The lossy key of `frame`, `None` for a frame that may not be dropped.
///
/// A frame kind or event type this build does not know classifies as
/// **reliable**, which is the safe side of the decision and the same side
/// [`Frame::is_lossy`] takes: a newer peer's lossy frame costs a needless
/// delivery, whereas the opposite default would drop a one-shot frame whose loss
/// wedges the client.
fn lossy_key(frame: &DecodedFrame) -> Option<LossyKey> {
    let DecodedFrame::Known(frame) = frame else {
        return None;
    };
    match frame.value() {
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
        // A merged `list` reaches a client from the gateway's own control links
        // rather than through this queue (spec 7.1). Classified anyway, so that
        // what this says about a frame is the protocol's answer rather than a
        // claim about who happens to enqueue one.
        Frame::List { .. } => Some(LossyKey::List),
        Frame::Vms { .. } => Some(LossyKey::Vms),
        Frame::CaughtUp { .. } | Frame::Reset { .. } | Frame::Heartbeat => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aj_agent::events::AgentSettings;
    use futures::FutureExt;

    use super::*;
    use crate::remote::tests::bounded;

    const SESSION: &str = "left:s-1";

    fn queue(capacity: usize) -> (Sender, Receiver, CancellationToken) {
        let cancelled = CancellationToken::new();
        let (sender, receiver) = channel(
            NonZeroUsize::new(capacity).expect("non-zero"),
            cancelled.clone(),
        );
        (sender, receiver, cancelled)
    }

    fn decoded(frame: Frame) -> DecodedFrame {
        DecodedFrame::try_from(frame).expect("a valid frame")
    }

    /// A reliable-transient frame: one-shot, never droppable.
    fn reliable(text: &str) -> DecodedFrame {
        decoded(Frame::Event {
            session: SESSION.to_string(),
            epoch: "epoch-1".to_string(),
            durability: None,
            event: AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: text.to_string(),
            }
            .into(),
        })
    }

    /// A lossy frame: a cumulative snapshot a later one supersedes.
    fn lossy(last_seq: u64) -> DecodedFrame {
        decoded(Frame::State {
            session: SESSION.to_string(),
            epoch: "epoch-1".to_string(),
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
        })
    }

    /// A frame kind this build does not know, as it arrives from the wire.
    fn unknown() -> DecodedFrame {
        serde_json::from_str::<DecodedFrame>(r#"{"kind":"something_newer","session":"left:s-1"}"#)
            .expect("an unknown frame decodes")
    }

    /// Everything already queued, rendered as a comparable summary.
    fn drained(receiver: &mut Receiver) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(Some(frame)) = receiver.recv().now_or_never() {
            out.push(match &frame {
                DecodedFrame::Unknown { kind, .. } => format!("unknown {kind}"),
                DecodedFrame::Known(known) => match known.value() {
                    Frame::Event { event, .. } => match event.known() {
                        Some(AgentEvent::Warning { text, .. }) => format!("warning {text}"),
                        other => format!("event {other:?}"),
                    },
                    Frame::State { last_seq, .. } => format!("state {last_seq}"),
                    Frame::Reset { session } => format!("reset {session}"),
                    other => format!("{other:?}"),
                },
            });
        }
        out
    }

    /// Lossy overflow drops the snapshot and the client stays. Reliable overflow
    /// evicts it instead of losing a frame it cannot regenerate (spec 6.9).
    #[tokio::test]
    async fn overflow_drops_a_snapshot_and_evicts_on_a_reliable_frame() {
        let (sender, mut receiver, cancelled) = queue(2);

        assert_eq!(sender.offer(reliable("one")), Offered::Queued);
        assert_eq!(sender.offer(reliable("two")), Offered::Queued);
        assert_eq!(
            sender.offer(lossy(1)),
            Offered::Dropped,
            "a snapshot meeting the bound is dropped",
        );
        assert!(!cancelled.is_cancelled(), "and the client stays");

        assert_eq!(sender.offer(reliable("three")), Offered::Evicted);
        assert!(
            cancelled.is_cancelled(),
            "an eviction ends the stream and the tasks feeding it",
        );
        assert!(
            drained(&mut receiver).is_empty(),
            "eviction closes and clears: what the client missed comes back from a backfill",
        );
        assert_eq!(
            sender.offer(reliable("four")),
            Offered::Evicted,
            "and nothing is queued for a client that is gone",
        );
    }

    /// A frame kind this build does not know may not be dropped: its class is
    /// unknown, so it counts as reliable.
    #[tokio::test]
    async fn an_unknown_kind_is_never_dropped() {
        let (sender, _receiver, cancelled) = queue(1);
        assert_eq!(sender.offer(reliable("one")), Offered::Queued);

        assert_eq!(sender.offer(unknown()), Offered::Evicted);

        assert!(cancelled.is_cancelled());
    }

    /// A newer snapshot replaces the queued older one *at the tail*, so it
    /// cannot jump a reliable frame that was queued in between.
    #[tokio::test]
    async fn a_snapshot_is_replaced_at_the_tail() {
        let (sender, mut receiver, _cancelled) = queue(3);

        sender.offer(lossy(1));
        sender.offer(reliable("after"));
        sender.offer(lossy(2));

        assert_eq!(drained(&mut receiver), vec!["warning after", "state 2"]);
    }

    /// A queued attach block is never coalesced into: its opening `state` frame
    /// is what the client adopts the session's epoch from (spec 6.5), so a live
    /// snapshot that took its place would leave the client applying a block under
    /// no epoch at all. The newer snapshot queues behind the block.
    #[tokio::test]
    async fn a_live_snapshot_does_not_take_a_queued_blocks_place() {
        let (sender, mut receiver, _cancelled) = queue(4);

        assert!(sender.send_paced(lossy(1)).await, "the block's state frame");
        assert!(sender.send_paced(reliable("backfill")).await);
        assert_eq!(sender.offer(lossy(2)), Offered::Queued);

        assert_eq!(
            drained(&mut receiver),
            vec!["state 1", "warning backfill", "state 2"],
            "the block arrived in the order the host wrote it",
        );
    }

    /// A paced frame waits for room instead of evicting, which is what keeps an
    /// attach block bigger than the bound from evicting its own client.
    #[tokio::test]
    async fn a_paced_frame_waits_for_room() {
        let (sender, mut receiver, cancelled) = queue(1);
        assert_eq!(sender.offer(reliable("queued")), Offered::Queued);

        let paced = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send_paced(reliable("paced")).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!paced.is_finished(), "the bound is reached, so it waits");
        assert!(!cancelled.is_cancelled(), "waiting is not eviction");

        assert!(receiver.recv().await.is_some(), "the client reads");

        assert!(
            bounded("the paced frame to land", paced)
                .await
                .expect("the task")
        );
        assert_eq!(drained(&mut receiver), vec!["warning paced"]);
    }

    /// A paced frame does not wait forever: a stream that ends while a block is
    /// in flight releases the task pumping it.
    #[tokio::test]
    async fn a_paced_frame_gives_up_when_the_stream_is_cancelled() {
        let (sender, _receiver, cancelled) = queue(1);
        sender.offer(reliable("queued"));

        let paced = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send_paced(reliable("paced")).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancelled.cancel();

        assert!(
            !bounded("the paced frame to give up", paced)
                .await
                .expect("the task"),
            "the frame was not delivered, and the task is not stuck on it",
        );
    }

    /// A reader waiting on an empty queue is woken by the next frame.
    #[tokio::test]
    async fn a_waiting_reader_is_woken() {
        let (sender, mut receiver, _cancelled) = queue(4);

        let reading = tokio::spawn(async move { receiver.recv().await.is_some() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        sender.offer(reliable("late"));

        assert!(
            bounded("the reader to wake", reading)
                .await
                .expect("the task")
        );
    }
}
