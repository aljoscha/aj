//! The subscriber registry and the fan-out into it.
//!
//! One subscriber is one stream: a producer-paced attach channel, a bounded
//! live queue, and the backfill boundary for each attached session.
//!
//! Publishers only touch the bounded live queue and never await. The attach
//! producer awaits its separate capacity-one channel, so HTTP backpressure
//! paces a large backfill without ever stalling a session driver.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use aj_wire::{Frame, SessionSummary};
use tokio::sync::Notify;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio_util::sync::CancellationToken;

use crate::outbound::{self, Offered};

/// Enough burst room for normal clients while bounding a stalled stream.
const DEFAULT_LIVE_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).expect("non-zero");

/// Identity of one attached subscriber within a host.
pub(crate) type SubscriberId = u64;

/// A subscriber's view of one session it attached.
enum AttachState {
    /// The attach block has not been written yet, so live frames are held
    /// to keep the block contiguous and ordered on this stream.
    ///
    /// Lossy frames are dropped rather than queued: a
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
    live: outbound::Sender<Frame>,
    /// A child of eviction cancellation, also stopped independently during
    /// graceful shutdown so a completed block can still drain live frames.
    block_stop: CancellationToken,
    attached: HashMap<String, AttachState>,
    /// Whether this subscriber's queue accepted the fan-out's latest directory.
    /// A fresh subscriber and one whose full queue dropped the frame stay false,
    /// so the next refresh offers the current snapshot to them.
    list_current: bool,
}

impl Subscriber {
    /// Queue `frame` for this subscriber, applying the attach rules of the
    /// session it belongs to.
    fn offer(&mut self, frame: &Frame) -> bool {
        self.deliver(frame) != Offered::Evicted
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
                // row in `list` frames. Its events would apply to
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

/// The live queue and its host-owned attach producer cancellation.
pub(crate) struct LiveReceiver {
    frames: outbound::Receiver<Frame>,
    block_stop: CancellationToken,
}

impl LiveReceiver {
    pub(crate) fn block_stop_token(&self) -> CancellationToken {
        self.block_stop.clone()
    }

    async fn recv(&mut self) -> Option<Frame> {
        self.frames.recv().await
    }

    fn try_recv(&mut self) -> Option<Frame> {
        self.frames.try_recv()
    }
}

/// The host's subscriber registry.
pub(crate) struct Fanout {
    state: StdMutex<FanoutState>,
    next_id: AtomicU64,
    /// Pinged whenever the session directory changed. The list publisher
    /// coalesces on it.
    list_dirty: Notify,
    live_capacity: NonZeroUsize,
}

struct FanoutState {
    /// Terminal once set. Keeping this under the registry lock makes a close
    /// atomic with every registration that could otherwise follow it.
    closed: bool,
    subscribers: HashMap<SubscriberId, Subscriber>,
    /// The latest directory payload, compared once per publisher tick.
    /// Subscribers retain only whether their own queue accepted it.
    current_list: Option<Vec<SessionSummary>>,
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
                current_list: None,
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
        let cancelled = CancellationToken::new();
        let block_stop = cancelled.child_token();
        let (live, frames) = outbound::channel(self.live_capacity, cancelled.clone());
        let receiver = LiveReceiver {
            frames,
            block_stop: block_stop.clone(),
        };
        let attached = sessions
            .iter()
            .map(|session| (session.clone(), AttachState::Attaching))
            .collect();
        let mut state = self.lock();
        if state.closed {
            block_stop.cancel();
            live.close();
        } else {
            state.subscribers.insert(
                id,
                Subscriber {
                    live,
                    block_stop,
                    attached,
                    list_current: false,
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
    /// could not resolve.
    ///
    /// A subscriber is registered for every session its request names before
    /// any of them is resolved, which is what makes an attach in flight count
    /// as use. A session that then turns out to be unservable has to come back
    /// out: this host may hold it later, for another client, and a
    /// session-scoped frame is undroppable by class, so it would count against
    /// a bound this client never asked to spend and could evict it over traffic
    /// it never asked for.
    pub(crate) fn detach(&self, id: SubscriberId, session: &str) {
        if let Some(subscriber) = self.lock().subscribers.get_mut(&id) {
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

    /// Take `session` off every subscriber's attach set, keeping what is
    /// already queued for it.
    ///
    /// For a session whose materialization is ending under its clients: the
    /// frame that tells them so is already queued and must still reach them,
    /// while nothing published after this call may. Clearing the attach set is
    /// also what lets the release path see the session as unused.
    pub(crate) fn detach_all(&self, session: &str) {
        for subscriber in self.lock().subscribers.values_mut() {
            subscriber.attached.remove(session);
        }
    }

    /// Fan `frame` out to every subscriber.
    pub(crate) fn publish(&self, frame: Frame) {
        self.lock()
            .subscribers
            .retain(|_, subscriber| subscriber.offer(&frame));
    }

    /// Fan a changed directory out, and retry it for subscribers that missed it.
    ///
    /// The payload comparison belongs here because there is one current
    /// directory. Queue admission remains per subscriber: a fresh subscriber or
    /// one whose full queue dropped the frame still needs the next refresh.
    pub(crate) fn publish_list(&self, sessions: Vec<SessionSummary>) {
        let mut state = self.lock();
        if state.current_list.as_deref() != Some(&sessions) {
            state.current_list = Some(sessions);
            for subscriber in state.subscribers.values_mut() {
                subscriber.list_current = false;
            }
        }
        if state
            .subscribers
            .values()
            .all(|subscriber| subscriber.list_current)
        {
            return;
        }
        let frame = Frame::List {
            sessions: state
                .current_list
                .clone()
                .expect("the current directory was set above"),
            // A plain host's rows are all its own, so it names no hosts:
            // that field is a gateway's.
            hosts: Vec::new(),
        };
        state.subscribers.retain(|_, subscriber| {
            if subscriber.list_current {
                return true;
            }
            match subscriber.deliver(&frame) {
                Offered::Queued => {
                    subscriber.list_current = true;
                    true
                }
                // The next refresh retries the current directory only for this
                // subscriber.
                Offered::Dropped => true,
                Offered::Evicted => false,
            }
        });
    }

    /// Switches a session to live delivery and filters duplicate durables.
    ///
    /// Only for a session the subscriber is still attached to: a block whose
    /// session was detached while it was being served ([`Self::detach_all`])
    /// must not reattach it.
    pub(crate) fn finish_block(&self, id: SubscriberId, session: &str, boundary: u64) {
        let mut state = self.lock();
        let Some(subscriber) = state.subscribers.get_mut(&id) else {
            return;
        };
        let Some(attach) = subscriber.attached.get_mut(session) else {
            return;
        };
        *attach = AttachState::Live { boundary };
        subscriber.live.retain(|frame| {
            frame.session() != Some(session)
                || !frame.durable_seq().is_some_and(|seq| seq <= boundary)
        });
    }

    /// Ask every attach-block producer to stop before session teardown begins.
    /// A fully completed block sequence remains readable and continues into
    /// its live queue; an aborted partial sequence ends at its channel close.
    pub(crate) fn stop_blocks(&self) {
        for subscriber in self.lock().subscribers.values() {
            subscriber.block_stop.cancel();
        }
    }

    /// Drop every subscriber, closing its stream.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        for (_, subscriber) in state.subscribers.drain() {
            subscriber.block_stop.cancel();
            subscriber.live.close();
        }
    }

    /// Whether any subscriber is attached to `session`.
    ///
    /// True from the moment a subscriber registers, not from when its attach
    /// block completes, which is what lets the release path treat an attach in
    /// flight as use: attachment is the retention signal.
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
    block_complete: AttachBlockCompletion,
    cancelled: CancellationToken,
    attached: Vec<String>,
    fanout: Arc<Fanout>,
}

/// Shared producer result for deciding whether a closed block channel is a
/// complete prefix or an aborted partial block.
#[derive(Clone)]
pub(crate) struct AttachBlockCompletion(Arc<AtomicBool>);

impl AttachBlockCompletion {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn finish(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_finished(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
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
    ) -> (Self, Sender<Frame>, AttachBlockCompletion) {
        let (block_tx, block) = channel(1);
        let block_complete = AttachBlockCompletion::new();
        let attachment = Self {
            id,
            block,
            block_done: false,
            live,
            block_complete: block_complete.clone(),
            cancelled,
            attached,
            fanout,
        };
        (attachment, block_tx, block_complete)
    }

    /// The sessions this stream was served an attach block for.
    ///
    /// A client arms its fold from this rather than from what it asked for:
    /// a session the attach could not resolve is answered with an `error`
    /// frame instead of a block, so it is not here, and arming for
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
            if !self.block_complete.is_finished() {
                return None;
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
                Err(TryRecvError::Disconnected) => {
                    if !self.block_complete.is_finished() {
                        return None;
                    }
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
            credential_warning: None,
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

    /// A session-scoped refusal, reliable-transient like every `error` frame.
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
    /// or not. Suppression records what the queue accepted, so a lossy frame the
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

    /// A snapshot that restores the last delivered value is still offered when
    /// a different one was accepted in between.
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
    /// its durable and reliable-transient frames. The host-level
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
    /// against a bound this client never asked to spend.
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

    /// A session whose materialization ends under its clients is taken off
    /// every stream, but what was already queued for it stays: the error that
    /// says why is published just before, and must still arrive. Nothing
    /// published after reaches the stream, and a block still being served for
    /// the session cannot reattach it when it completes.
    #[test]
    fn detaching_a_session_everywhere_keeps_what_was_queued_and_admits_nothing_later() {
        let fanout = Fanout::default();
        let (live, mut live_rx, _c1) = fanout.register(&[SESSION.to_string()]);
        fanout.finish_block(live, SESSION, 0);
        let (attaching, mut attaching_rx, _c2) =
            fanout.register(&[SESSION.to_string(), OTHER.to_string()]);
        fanout.finish_block(attaching, OTHER, 0);
        fanout.publish(refusal("persistence_failed"));

        fanout.detach_all(SESSION);

        fanout.publish(reliable("after"));
        fanout.publish(other(reliable("someone else's session")));
        // The block for `SESSION` completes late on the attaching stream.
        fanout.finish_block(attaching, SESSION, 0);
        fanout.publish(reliable("after the late block"));
        assert_eq!(drained(&mut live_rx), vec!["error persistence_failed"]);
        assert_eq!(
            drained(&mut attaching_rx),
            vec!["error persistence_failed", "warning someone else's session"],
            "the other session's traffic is untouched",
        );
        assert!(!fanout.attached(SESSION));
        assert!(fanout.attached(OTHER));
    }

    /// A refusal is reliable-transient, so neither queue rule that
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
        let block_stop = rx.block_stop_token();
        fanout.finish_block(id, SESSION, 0);
        fanout.publish(reliable("one"));
        fanout.publish(reliable("two"));
        fanout.publish(Frame::List {
            sessions: Vec::new(),
            hosts: Vec::new(),
        });
        assert!(!cancelled.is_cancelled(), "lossy overflow is only dropped");

        fanout.publish(reliable("three"));
        assert!(cancelled.is_cancelled());
        assert!(
            block_stop.is_cancelled(),
            "eviction stops attach production"
        );
        assert!(
            fanout.lock().subscribers.is_empty(),
            "the subscriber was evicted"
        );
        assert!(drained(&mut rx).is_empty(), "eviction closes and clears");
    }

    /// The attach channel has capacity one and live frames remain hidden until
    /// its sender closes.
    #[test]
    fn attachment_is_producer_paced_and_reads_the_block_before_live() {
        let fanout = Arc::new(Fanout::default());
        let (id, live, cancelled) = fanout.register(&[SESSION.to_string()]);
        let (mut attachment, block_tx, block_complete) = Attachment::new(
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
        block_complete.finish();
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
        let block_stop = live.block_stop_token();
        let (attachment, _block_tx, _block_complete) = Attachment::new(
            id,
            live,
            cancelled,
            vec![SESSION.to_string()],
            Arc::clone(&fanout),
        );
        assert_eq!(fanout.lock().subscribers.len(), 1);

        drop(attachment);

        assert!(fanout.lock().subscribers.is_empty());
        assert!(
            block_stop.is_cancelled(),
            "departure stops attach production"
        );
    }
}
