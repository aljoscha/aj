//! Client-side fold of one session's frame stream (spec 6.5).
//!
//! [`SessionClient`] is the consumer contract a session host has to
//! satisfy. It turns the frames of one session into reducer calls and
//! keeps the state no transcript carries: the session's epoch, the two
//! cursor positions, the lifecycle sets, and the settings. The queue and
//! background-task snapshots a remote frontend cannot read off live handles
//! go into [`ChatState`], which is what every frontend renders from.
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

/// Why a session is withheld, which names the edge in the peer's directory
/// that re-asks for it (spec 6.5).
///
/// Read off the refusal's code once, where the frame is folded, so the wire's
/// vocabulary stays in this module and whatever watches the directory reads a
/// decision rather than a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The `locked` code: a rival writer holds the session (spec section 5).
    /// Its row stays listed for as long as the hold lasts, so the edge that
    /// says the answer can have changed is the row's `locked` bit going true
    /// then false, the rival letting go (spec 6.8). The absence edge is kept
    /// besides, because a row can leave and return anyway and comes back
    /// rebuilt with the bit already false.
    Locked {
        /// The generation of the acquire this refusal answered, as the peer
        /// named it. `None` from a peer that publishes no generations.
        ///
        /// The transitions above are read out of `list`, which is
        /// lossy-coalescible: the rise and the fall are seconds apart by
        /// design, so a client that does not drain in between sees only the
        /// fall's snapshot and has nothing to compare it against. This is what
        /// makes the recovery derivable from that one snapshot instead (spec
        /// 6.5): a row reporting the lock free at this generation or beyond says
        /// this conflict is over.
        generation: Option<u64>,
    },
    /// Every other code, the ones this build has never heard of included: the
    /// row's return to the list is the edge, and nothing else. An unknown
    /// refusal behaves like the refusals this build knows rather than like the
    /// most specific one, which is spec 6.6's additive codes applied to
    /// rejoining.
    Other,
}

impl Refusal {
    /// Classify a peer refusal from its wire code and optional lock generation.
    ///
    /// Unknown codes deliberately use [`Self::Other`], whose recovery waits for
    /// the session row to leave and return instead of assuming lock semantics.
    pub fn from_code(code: &str, lock_generation: Option<u64>) -> Self {
        if code == "locked" {
            Self::Locked {
                generation: lock_generation,
            }
        } else {
            Self::Other
        }
    }

    /// The acquire generation this refusal names, `None` unless it is a
    /// `locked` one from a peer that publishes generations.
    pub(crate) fn generation(self) -> Option<u64> {
        match self {
            Self::Locked { generation } => generation,
            Self::Other => None,
        }
    }
}

/// Action-local rows that must survive each attempt to project an accepted
/// head switch.
#[derive(Clone, Debug, Default)]
struct RecoveryRows {
    peer_error: Option<String>,
    warning: Option<String>,
}

impl RecoveryRows {
    fn restore(&self, chat: &mut ChatState, lifecycle: &mut AgentLifecycle) {
        if let Some(text) = &self.peer_error {
            let _ = reduce(
                chat,
                lifecycle,
                AgentEvent::Error {
                    agent_id: AgentId::Main,
                    text: text.clone(),
                },
                None,
            );
        }
        if let Some(text) = &self.warning {
            let _ = reduce(
                chat,
                lifecycle,
                AgentEvent::Warning {
                    agent_id: AgentId::Main,
                    text: text.clone(),
                },
                None,
            );
        }
    }
}

/// A head switch whose authoritative attach block still has to replace the
/// current projection.
///
/// The phase prevents an unrelated `caught_up` from discharging the reset. A
/// refusal or interrupted block returns it to `Awaiting`, preserving its rows
/// for the next attempt, and only a caught-up block opened under `Applying`
/// completes it.
#[derive(Debug)]
enum ForwardReset {
    Awaiting(RecoveryRows),
    Applying(RecoveryRows),
}

impl ForwardReset {
    fn rows(&self) -> &RecoveryRows {
        match self {
            Self::Awaiting(rows) | Self::Applying(rows) => rows,
        }
    }

    fn rows_mut(&mut self) -> &mut RecoveryRows {
        match self {
            Self::Awaiting(rows) | Self::Applying(rows) => rows,
        }
    }

    fn start_applying(&mut self) {
        if let Self::Awaiting(rows) = self {
            *self = Self::Applying(std::mem::take(rows));
        }
    }

    fn retry(&mut self) {
        if let Self::Applying(rows) = self {
            *self = Self::Awaiting(std::mem::take(rows));
        }
    }

    fn is_applying(&self) -> bool {
        matches!(self, Self::Applying(_))
    }
}

/// Where the client stands relative to an attach block (spec 6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    /// No attach outstanding. Every frame is a live one.
    ///
    /// Where a block ends, whether its `caught_up` committed it or a refusal
    /// replaced it, and where one that was never armed starts. So this says
    /// nothing on its own about whether the client holds an attachment.
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
    /// assistant or compaction entry's `UsageUpdate`, a tool-result entry's bracket) and
    /// a drop in between would otherwise make the client claim an entry it
    /// only half applied.
    committed: Option<u64>,
    /// The durable high-water mark the cursor invariant compares against.
    applied: Option<u64>,
    attach: Attach,
    settings: Option<AgentSettings>,
    working: bool,
    first_attach_settings: Option<AgentSettings>,
    saw_first_attach: bool,
    needs_task_refetch: bool,
    needs_queue_refetch: bool,
    needs_reattach: bool,
    /// Present after an accepted Head command until its authoritative attach
    /// block is caught up. While present, every block opening resets the chat
    /// even when a refusal cleared `epoch`, and restores action-local recovery
    /// rows before durable backfill starts.
    forward_reset: Option<ForwardReset>,
    /// A refusal cleared the epoch while leaving last-known chat visible. The
    /// next block replaces that cache and restores this exact local error ahead
    /// of the new authoritative backfill.
    refusal_error: Option<String>,
    /// A one-shot frontend signal for each forced projection replacement.
    forced_replacement_opened: bool,
    /// Refused, and nothing is asking again yet, carrying the reason so the
    /// directory knows which edge re-asks.
    ///
    /// Set by [`Self::drop_attachment`], which is the one place that learns a
    /// refusal happened, and cleared by [`Self::owe_reattach`]. Held here rather
    /// than derived from the epoch and the arm because a refusal after an earlier
    /// one moves neither: the client is already following nothing, so there is no
    /// transition left to read, and a session refused twice would look attached
    /// to anything watching for one.
    withheld: Option<Refusal>,
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
            first_attach_settings: None,
            saw_first_attach: false,
            needs_task_refetch: false,
            needs_queue_refetch: false,
            needs_reattach: false,
            forward_reset: None,
            refusal_error: None,
            forced_replacement_opened: false,
            withheld: None,
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
    /// Over a connection there is nothing better than the request to arm from:
    /// the protocol gives a client no per-session answer at attach time, only
    /// frames afterwards. So a remote arm is a guess that the peer will serve
    /// what it was asked for, and whoever folds the block owes it a deadline
    /// (see [`Self::abandon_attach`]).
    ///
    /// Every session the server did attach has to be armed. An unarmed
    /// session's block folds as live frames instead: its `caught_up` is
    /// ignored and its durable frames advance the cursor in projection
    /// order, which is not seq order.
    ///
    /// Arming also satisfies [`Self::needs_reattach`], and ends a withheld
    /// state: an arm is something asking again, which is the one condition
    /// [`Self::withheld`] tracks.
    pub fn expect_attach(&mut self) {
        self.attach = Attach::Requested;
        self.needs_reattach = false;
        // Not only the edges' business. A reconnect arms every session
        // `attach_requests` names, withheld ones included, so an arm that left
        // this set would leave a client attached and still marked refused, and
        // the next row transition would re-ask for a session it already holds.
        self.withheld = None;
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
                    // The reducer treats this event as a pure redraw ping and
                    // drops the payload, so the snapshot is kept here instead.
                    // This is the single writer of the chat's queue model, which
                    // is what every frontend's pending box renders.
                    chat.note_queue(AgentQueue {
                        agent_id: *agent_id,
                        steering: steering.clone(),
                        follow_up: follow_up.clone(),
                    });
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
                let opens_block = self.attach == Attach::Requested;
                if self.attach == Attach::Requested {
                    self.open_attach_block(chat, epoch);
                } else if !self.epoch_matches(&epoch) {
                    return Redraw(false);
                }
                if opens_block && !self.saw_first_attach {
                    self.first_attach_settings = Some(settings.clone());
                    self.saw_first_attach = true;
                }
                // The host is authoritative for all of these, at every
                // emission: neither is derivable from projected events.
                let context_window = chat.resolve_window(&settings);
                chat.footers_mut()
                    .note_settings(AgentId::Main, settings.clone(), context_window);
                self.seed_lifecycle(working);
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
                if self
                    .forward_reset
                    .as_ref()
                    .is_some_and(ForwardReset::is_applying)
                {
                    // Only a block opened as the forced replacement can
                    // discharge it. The restored rows stay in this projection,
                    // but no later epoch reset should resurrect them.
                    self.forward_reset = None;
                }
                Redraw(true)
            }
            Frame::Error {
                session,
                code,
                message,
                lock_generation,
                ..
            } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                // Not filtered by epoch, the way `reset` is not: the frames
                // spec 6.5 filters are the ones that carry state under one, and
                // a refusal for a session the server cannot resolve names none.
                //
                // Every error frame drops the attachment, whatever its code,
                // which is what the two that exist are (a host's unresolvable
                // session, a gateway's withdrawn host) and what a rival's hold
                // is too. The code decides only which edge asks again, never
                // whether to let go: a refused client is following nothing
                // either way, and a code this build has never heard of has to
                // behave like the refusals it knows (spec 6.6).
                //
                // Spec section 5 reserves the kind for a turn's fatal error too,
                // on a session that stays live. Nothing emits that today, and
                // the day something does it arrives on the unknown-code path and
                // tears the attachment down over a turn that failed. Telling
                // that kind from a refusal is the branch this arm still owes.
                self.drop_attachment(Refusal::from_code(&code, lock_generation), message.clone());
                // The message verbatim, which spec 6.6 makes sufficient on its
                // own, so a code this build has never heard of still reads.
                reduce(
                    chat,
                    &mut self.lifecycle,
                    AgentEvent::Error {
                        agent_id: AgentId::Main,
                        text: message,
                    },
                    None,
                )
            }
            Frame::Reset { session } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                // Continuity is broken, but the cursor stays valid to
                // offer: the server decides whether it can resume from it.
                // An armed attach stays armed, so a `reset` that overtakes
                // the block the client already asked for cannot disarm it.
                //
                // Through `owe_reattach` rather than by assignment, so that one
                // function stays the only place the obligation is taken on and
                // the withheld state released. A reset reaches a refused
                // session too (a gateway sends one per session when a host's
                // link returns), and one that left it marked refused would have
                // the client re-ask on a later row for a session this reset is
                // already sending it back to.
                self.owe_reattach();
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

    /// Takes the settings carried by the first attach state exactly once.
    ///
    /// A frontend that resumed an existing session can render its local
    /// restored-settings summary from this without the host publishing a
    /// notice that would repeat on every reconnect.
    pub fn take_first_attach_settings(&mut self) -> Option<AgentSettings> {
        self.first_attach_settings.take()
    }

    /// Whether the host reported a turn in flight, as of the last `state`
    /// frame. The lifecycle sets are the authority for spinners, this is
    /// the host's own flag.
    pub fn working(&self) -> bool {
        self.working
    }

    /// Replace the queue snapshot from the queue read (spec 6.7), which is
    /// how a mid-session joiner learns about messages queued before it
    /// attached. Clears [`Self::needs_queue_refetch`].
    pub fn set_queue(&mut self, chat: &mut ChatState, queue: QueueState) {
        chat.replace_queue(queue);
        self.needs_queue_refetch = false;
    }

    /// Replace the task table from the tasks read, clearing
    /// [`Self::needs_task_refetch`].
    pub fn set_tasks(&mut self, chat: &mut ChatState, tasks: TaskTable) {
        chat.replace_tasks(tasks);
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

    /// Record that a Head command took and the current projection is now
    /// necessarily behind the session.
    ///
    /// The next attach block replaces the chat even if a refusal clears the
    /// client's epoch before that block arrives. The obligation remains across
    /// refused and interrupted attempts until one forced block is caught up.
    pub fn prepare_committed_head(&mut self) {
        self.forward_reset = Some(ForwardReset::Awaiting(RecoveryRows::default()));
        self.owe_reattach();
    }

    /// Retain the local rows explaining why an accepted Head command is not yet
    /// reflected by the projection.
    ///
    /// `peer_error` is the peer's exact text when the failed follow produced an
    /// error frame. `None` preserves any exact error retained by an earlier
    /// attempt. `warning` is the frontend's action-level explanation. The rows
    /// are restored, in that order, whenever the pending replacement resets the
    /// chat and before its durable backfill is folded.
    ///
    /// This is inert without a pending accepted Head command. It only retains
    /// rows, so the caller still folds the warning into the current projection
    /// when reporting the failure.
    pub fn recover_committed_head(&mut self, peer_error: Option<String>, warning: String) {
        let Some(reset) = &mut self.forward_reset else {
            return;
        };
        if peer_error.is_some() {
            // The one-shot action handler is transferring this refusal into the
            // forward-reset rows. A later value in `refusal_error` then really
            // is a newer passive recovery answer.
            self.refusal_error = None;
        }
        let rows = reset.rows_mut();
        if let Some(peer_error) = peer_error {
            rows.peer_error = Some(peer_error);
        }
        rows.warning = Some(warning);
    }

    /// Take the signal that a forced projection replacement opened.
    ///
    /// A frontend uses this to retire terminal image ids from the projection
    /// that [`ChatState::reset`] discarded. Each forced opening raises the
    /// signal again, including a retry after an interrupted block.
    pub fn take_forced_replacement_opened(&mut self) -> bool {
        std::mem::take(&mut self.forced_replacement_opened)
    }

    /// Where this client stands on an attach block.
    ///
    /// This is what a caller folding a block waits on, and it is the client's
    /// own arm rather than a frame kind, which is what makes the wait end on
    /// everything that ends a block: the `caught_up` that commits it, and the
    /// refusal that replaces it for a session the server cannot resolve (spec
    /// 6.5). A peer that answers with neither is covered by nothing here, so a
    /// caller still owes the wait a deadline of its own.
    ///
    /// The two outstanding phases are worth telling apart, because waiting for a
    /// block to begin and waiting for one already arriving are different
    /// questions: nothing has been applied before the opening `state` frame.
    pub fn attach_phase(&self) -> Attach {
        self.attach
    }

    /// Whether this client holds an attachment: an epoch adopted from an attach
    /// block, which is what its session frames are folded under.
    ///
    /// False before the first block, and false again once a refusal drops the
    /// attachment (see [`Self::apply`]).
    pub fn holds_attachment(&self) -> bool {
        self.epoch.is_some()
    }

    /// Why this session was refused, `None` once something is asking again.
    ///
    /// Set from the refusal until something owes the re-attach
    /// ([`Self::owe_reattach`]), so a caller reading it across one folded frame
    /// sees each refusal, including a repeat one. The reason is what names the
    /// edge that re-asks (see [`Refusal`]).
    pub fn withheld(&self) -> Option<Refusal> {
        self.withheld
    }

    /// Owe a re-attach: this client is not following its session, and something
    /// has to ask for it again.
    ///
    /// [`Self::needs_reattach`] is the only record that one is owed, so every
    /// path that leaves the client not following has to end here or nothing
    /// anywhere will ask.
    pub fn owe_reattach(&mut self) {
        self.needs_reattach = true;
        self.withheld = None;
    }

    /// Give up on an attach block that never arrived, re-owing the re-attach.
    ///
    /// [`Self::expect_attach`] discharges [`Self::needs_reattach`] on the
    /// promise that the block it arms for is served. A block that stopped
    /// arriving breaks that promise, and the obligation has to come back: the
    /// arm is the only record that one is outstanding, so a caller that dropped
    /// the wait without this would leave the session armed for a block nobody
    /// is bringing, and nothing anywhere would ask for it again.
    ///
    /// The epoch and the cursor stay. Nothing said this session is gone, only
    /// that this attempt did not finish, so the next attach still offers what
    /// this client has: the opposite of what a refusal does (see
    /// [`Self::apply`]'s `error` arm).
    pub fn abandon_attach(&mut self) {
        self.attach = Attach::Live;
        if let Some(reset) = &mut self.forward_reset {
            reset.retry();
        }
        self.owe_reattach();
    }

    /// Drop the attachment for a session the server refused (spec 6.5).
    ///
    /// Everything the fold holds about the session comes from an attach block
    /// that is not coming: the epoch it applied under and the cursor it would
    /// offer describe a history the server says it cannot resolve, so keeping
    /// either would have the client ask for one nobody has. The arm goes too, or
    /// the next `state` frame to arrive would be taken for the block this
    /// refusal replaced.
    ///
    /// Withdrawing the re-attach obligation is half a rule, and the other half
    /// is not here. Asking again immediately is noise, because nothing has
    /// changed since the refusal, and never asking again strands the client on a
    /// session whose host was only restarting. So the obligation is withdrawn
    /// *until the peer's own directory says the answer can have changed*, which
    /// is `SessionDirectory`'s to notice, and it puts the obligation back
    /// ([`Self::owe_reattach`]). `refusal` is what tells it which edge to watch.
    /// Spec 6.5 permits the later attach that costs a full backfill. What it
    /// does not ask for is a retry loop.
    ///
    /// Deliberately not what a `reset` does. That one says continuity broke on a
    /// session the server still has, so its obligation stands and is discharged
    /// at once.
    fn drop_attachment(&mut self, refusal: Refusal, message: String) {
        self.attach = Attach::Live;
        self.epoch = None;
        self.committed = None;
        self.applied = None;
        if let Some(reset) = &mut self.forward_reset {
            reset.retry();
        }
        self.refusal_error = Some(message);
        self.needs_reattach = false;
        self.withheld = Some(refusal);
    }

    /// Adopt the epoch of the attach block this client asked for, and
    /// prepare `chat` for the backfill that follows.
    fn open_attach_block(&mut self, chat: &mut ChatState, epoch: String) {
        if let Some(reset) = &mut self.forward_reset
            && let Some(latest_refusal) = self.refusal_error.take()
        {
            // Action presentation is one-shot, while forward recovery may be
            // refused repeatedly. The authoritative replacement retains the
            // latest peer answer even after the pending action was consumed.
            reset.rows_mut().peer_error = Some(latest_refusal);
            // The old guidance may describe a different refusal class. Without
            // an action context to word the new one, retaining only the exact
            // latest peer answer is safer than restoring stale advice.
            reset.rows_mut().warning = None;
        }
        let forced_recovery = self.forward_reset.as_mut().map(|reset| {
            reset.start_applying();
            reset.rows().clone()
        });
        if let Some(recovery) = forced_recovery {
            // An accepted Head command is stronger evidence than the local
            // epoch. In particular, a refusal clears that epoch without making
            // the projection current again, so the authoritative block must
            // still replace everything the abandoned branch built.
            chat.reset(&mut self.lifecycle);
            self.committed = None;
            self.applied = None;
            self.refusal_error = None;
            self.forced_replacement_opened = true;
            // These rows explain the action that caused the replacement. They
            // are local rather than durable, so a reset has to put them back
            // before replay starts or an interrupted retry would erase them.
            recovery.restore(chat, &mut self.lifecycle);
        } else if let Some(error) = self.refusal_error.take() {
            // Refusal deliberately keeps the last-known transcript visible but
            // clears its epoch. The next full block can name another branch, so
            // first-attach append semantics would merge two authoritative
            // histories. Replace the cache before applying the new block.
            chat.reset(&mut self.lifecycle);
            self.committed = None;
            self.applied = None;
            self.forced_replacement_opened = true;
            let _ = reduce(
                chat,
                &mut self.lifecycle,
                AgentEvent::Error {
                    agent_id: AgentId::Main,
                    text: error,
                },
                None,
            );
        } else {
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
        }
        self.epoch = Some(epoch);
        self.attach = Attach::Applying;
    }

    /// Reconciles the main agent's running mark from a state frame.
    ///
    /// A client whose stream died before an `AgentEnd` would otherwise spin
    /// forever: no projected event carries a lifecycle bracket. Between state
    /// frames, live lifecycle events are authoritative.
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
            thinking_display: "default".into(),
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

    /// A durable event with a body: a projected state notice, which
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
            account: None,
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

    /// The Main transcript's notice rows at `level`.
    fn notices_at(chat: &ChatState, level: NoticeLevel) -> Vec<String> {
        chat.transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Notice(notice) if notice.level == level => {
                            Some(notice.text.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The Main transcript's informational rows, which is what the durable
    /// `Notice` frames above land as.
    fn notices(chat: &ChatState) -> Vec<String> {
        notices_at(chat, NoticeLevel::Info)
    }

    /// Every Main notice row in projection order.
    fn notice_rows(chat: &ChatState) -> Vec<(NoticeLevel, String)> {
        chat.transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Notice(notice) => Some((notice.level, notice.text.clone())),
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

    /// The Main transcript's error rows, which is what a refusal surfaces as.
    fn errors(chat: &ChatState) -> Vec<String> {
        notices_at(chat, NoticeLevel::Error)
    }

    fn refusal(session: &str, code: &str, message: &str) -> Frame {
        Frame::Error {
            session: session.to_string(),
            epoch: None,
            code: code.to_string(),
            message: message.to_string(),
            lock_generation: None,
        }
    }

    /// A refused attach is surfaced and ends the attachment (spec 6.5): the
    /// session is gone, so there is nothing left to offer a cursor for and
    /// nothing to fold.
    #[test]
    fn a_refused_attach_surfaces_and_drops_the_attachment() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("one")));
        assert!(client.cursor().is_some(), "the fixture holds a cursor");

        assert!(
            client
                .apply(
                    &mut chat,
                    refusal(SESSION, "unknown_session", "unknown session session-1"),
                )
                .0
        );

        assert_eq!(
            errors(&chat),
            vec!["unknown session session-1"],
            "the host's own sentence reaches the user (spec 6.6)",
        );
        assert_eq!(
            client.cursor(),
            None,
            "a cursor for a session the server cannot resolve asks for a history \
             nobody has",
        );
        assert_eq!(
            notices(&chat),
            vec!["one"],
            "and what the session did show stays on screen",
        );

        // Nothing more is folded for it: the epoch it applied under went with
        // the attachment.
        assert!(
            !client
                .apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("after")))
                .0
        );
        assert_eq!(notices(&chat), vec!["one"]);
    }

    /// The refusal is not a `reset`: one says continuity broke and asks for a
    /// re-attach, the other says there is nothing left to attach to. A client
    /// that collapsed them would spin against a session that is gone.
    #[test]
    fn a_refusal_withdraws_the_re_attach_a_reset_asked_for() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            Frame::Reset {
                session: SESSION.to_string(),
            },
        );
        assert!(client.needs_reattach(), "the reset asked for one");

        let _ = client.apply(
            &mut chat,
            refusal(SESSION, "unknown_session", "unknown session session-1"),
        );

        assert!(
            !client.needs_reattach(),
            "the re-attach the reset asked for would be refused again",
        );
    }

    /// A refusal answers the attach the client asked for, so it disarms it.
    /// Left armed, the next `state` frame to arrive would be taken for the
    /// block this refusal replaced, and the fold would silently resume on a
    /// session it was told is gone.
    #[test]
    fn a_refusal_disarms_the_attach_it_answered() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        client.expect_attach();

        let _ = client.apply(
            &mut chat,
            refusal(SESSION, "unknown_session", "unknown session session-1"),
        );

        // What a stray `state` frame would do to an armed client: adopt its
        // epoch and open a block.
        let _ = client.apply(&mut chat, state("epoch-2", false));
        assert!(
            !client
                .apply(&mut chat, durable("epoch-2", 1, "entry-1", notice("stray")))
                .0
        );
        assert_eq!(notices(&chat), Vec::<String>::new());
        assert_eq!(client.cursor(), None);
    }

    /// An error frame for someone else's session is ignored, like every other
    /// session-scoped frame.
    #[test]
    fn a_refusal_for_another_session_is_ignored() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("one")));
        let before = client.cursor();

        assert!(
            !client
                .apply(
                    &mut chat,
                    refusal("session-2", "unknown_session", "unknown session session-2"),
                )
                .0
        );

        assert_eq!(errors(&chat), Vec::<String>::new());
        assert_eq!(
            client.cursor(),
            before,
            "another session's refusal drops nothing of ours",
        );
        assert!(
            client
                .apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("after")))
                .0,
            "and the fold carries on",
        );
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
                hosts: Vec::new(),
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

    /// An accepted Head command is authoritative even if its first follow is
    /// refused and clears the epoch. Each later attempt replaces the abandoned
    /// branch, puts the action-local failure rows back before replay, and keeps
    /// that obligation until one whole block commits.
    #[test]
    fn an_accepted_head_replaces_after_refusal_and_keeps_recovery_rows_through_retry() {
        const HEAD_EPOCH: &str = "epoch-after-head";
        const PEER_ERROR: &str = "session session-1 is temporarily unavailable";
        const WARNING: &str =
            "The branch switch was not served through. Reconnecting to the switched branch.";

        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            durable(EPOCH, 3, "old-entry", notice("the abandoned branch")),
        );

        client.prepare_committed_head();
        assert!(
            client.needs_reattach(),
            "accepting Head makes its authoritative projection owed",
        );
        client.expect_attach();
        let _ = client.apply(&mut chat, refusal(SESSION, "unknown_session", PEER_ERROR));
        assert_eq!(client.cursor(), None, "the refusal cleared the epoch");
        assert_eq!(notices(&chat), vec!["the abandoned branch"]);

        // The refusal frame already painted the peer's exact error. The
        // frontend paints its warning and retains both so a later replacement
        // can reconstruct these local, non-durable rows.
        client.recover_committed_head(Some(PEER_ERROR.to_string()), WARNING.to_string());
        let _ = client.apply_local(
            &mut chat,
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: WARNING.to_string(),
            },
        );

        // The directory edge says the refusal may now succeed. Despite the
        // missing epoch, the Head obligation makes this a replacement rather
        // than a first attach that appends to the abandoned branch.
        client.owe_reattach();
        client.expect_attach();
        let _ = client.apply(&mut chat, state(HEAD_EPOCH, false));
        assert!(client.take_forced_replacement_opened());
        assert!(
            !client.take_forced_replacement_opened(),
            "the frontend signal is one-shot per opening",
        );
        assert_eq!(notices(&chat), Vec::<String>::new());
        assert_eq!(errors(&chat), vec![PEER_ERROR]);
        assert_eq!(notices_at(&chat, NoticeLevel::Warning), vec![WARNING]);

        let _ = client.apply(
            &mut chat,
            durable(HEAD_EPOCH, 1, "partial", notice("partial attempt")),
        );
        let _ = client.apply(
            &mut chat,
            Frame::Reset {
                session: SESSION.to_string(),
            },
        );
        client.abandon_attach();
        // This interruption supplied no peer error of its own. Retaining its
        // warning must not erase the exact refusal text from the earlier try.
        client.recover_committed_head(None, WARNING.to_string());

        client.expect_attach();
        let _ = client.apply(&mut chat, state(HEAD_EPOCH, false));
        assert!(
            client.take_forced_replacement_opened(),
            "an interrupted replacement raises a fresh retirement signal",
        );
        assert_eq!(
            notice_rows(&chat),
            vec![
                (NoticeLevel::Error, PEER_ERROR.to_string()),
                (NoticeLevel::Warning, WARNING.to_string()),
            ],
            "the retry discarded partial backfill and restored each recovery row once",
        );

        let _ = client.apply(
            &mut chat,
            durable(HEAD_EPOCH, 2, "committed", notice("the switched branch")),
        );
        let _ = client.apply(&mut chat, caught_up(HEAD_EPOCH, 2));
        assert_eq!(
            notice_rows(&chat),
            vec![
                (NoticeLevel::Error, PEER_ERROR.to_string()),
                (NoticeLevel::Warning, WARNING.to_string()),
                (NoticeLevel::Info, "the switched branch".to_string()),
            ],
            "recovery rows precede the committed branch's durable backfill",
        );
        assert_eq!(
            client.cursor(),
            Some(Cursor {
                epoch: HEAD_EPOCH.to_string(),
                seq: 2,
            }),
            "the authoritative branch landed and committed",
        );

        // CaughtUp discharges the forward reset and its retained rows. A later
        // ordinary epoch replacement must neither restore them nor raise the
        // forced-replacement signal.
        client.expect_attach();
        let _ = client.apply(&mut chat, state("later-epoch", false));
        assert_eq!(notice_rows(&chat), Vec::new());
        assert!(!client.take_forced_replacement_opened());
    }

    /// A refusal leaves last-known chat visible, but clears the epoch. The next
    /// full block may represent a branch another writer selected, so it replaces
    /// that cache and restores only the local refusal ahead of new history.
    #[test]
    fn a_refused_cached_session_replaces_old_branch_rows_on_rejoin() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            durable(EPOCH, 1, "old-branch", notice("old branch only")),
        );
        let reason = "another writer held this session";
        client.expect_attach();
        let _ = client.apply(&mut chat, refusal(SESSION, "locked", reason));
        assert_eq!(notices(&chat), vec!["old branch only"]);
        assert_eq!(errors(&chat), vec![reason]);

        client.owe_reattach();
        client.expect_attach();
        let _ = client.apply(&mut chat, state("new-branch", false));
        assert!(
            client.take_forced_replacement_opened(),
            "the frontend was not told to retire stale projection artifacts",
        );
        let _ = client.apply(
            &mut chat,
            durable("new-branch", 1, "new-branch", notice("new branch only")),
        );
        let _ = client.apply(&mut chat, caught_up("new-branch", 1));

        assert_eq!(
            notices(&chat),
            vec!["new branch only"],
            "the old branch survived the authoritative full backfill",
        );
        assert_eq!(
            errors(&chat),
            vec![reason],
            "the action-local refusal vanished with the stale cache",
        );
        assert_eq!(
            client.cursor(),
            Some(Cursor {
                epoch: "new-branch".to_string(),
                seq: 1,
            }),
        );
    }

    /// Forward recovery outlives action presentation. If a later attempt is
    /// refused too, the final replacement carries the latest peer answer rather
    /// than restoring an older cached reason.
    #[test]
    fn accepted_head_recovery_retains_a_later_refusal_after_the_action_settles() {
        let (mut client, mut chat) = attached();
        client.prepare_committed_head();
        client.recover_committed_head(None, "recovering the accepted Head".to_string());

        client.expect_attach();
        let _ = client.apply(&mut chat, state("first-recovery", false));
        let latest = "the later recovery attempt was refused";
        let _ = client.apply(&mut chat, refusal(SESSION, "locked", latest));

        client.owe_reattach();
        client.expect_attach();
        let _ = client.apply(&mut chat, state("final-recovery", false));
        let _ = client.apply(&mut chat, caught_up("final-recovery", 0));

        assert_eq!(errors(&chat), vec![latest]);
        assert_eq!(
            notices_at(&chat, NoticeLevel::Warning),
            Vec::<String>::new(),
            "guidance from the earlier failure survived a later refusal",
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
        assert_eq!(
            chat.footers().settings(AgentId::Main),
            Some(&changed),
            "the authoritative state frame updates what the footer renders",
        );
        assert!(client.working());
        assert!(
            client.lifecycle().is_running(AgentId::Main),
            "every state frame self-heals the main lifecycle mark",
        );
    }

    #[test]
    fn stale_epoch_frames_are_dropped_outside_an_attach_block() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("ours")));
        client.set_tasks(&mut chat, TaskTable::default());
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

    /// Inside a block the cursor does not move per frame, so a durable
    /// frame that came out below an earlier one still applies (spec 6.5).
    ///
    /// The block is atomic and its `caught_up` commits it once, which is
    /// what lets the projection order its events by thread bracketing
    /// rather than by seq. Advancing per frame would make the cursor
    /// invariant read everything after the first descent as a duplicate,
    /// and a thread-scoped backfill would lose most of its rows to it.
    #[test]
    fn a_block_keeps_a_frame_that_came_out_below_an_earlier_one() {
        let (mut client, mut chat) = attached();

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, durable(EPOCH, 9, "entry-9", notice("nine")));
        let _ = client.apply(&mut chat, durable(EPOCH, 5, "entry-5", notice("five")));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 9));

        assert_eq!(
            notices(&chat),
            vec!["nine", "five"],
            "a block frame below an earlier one was dropped as a duplicate",
        );
        assert_eq!(
            client.cursor().map(|cursor| cursor.seq),
            Some(9),
            "the block committed whole at its own mark, which is also what \
             says this fixture was inside one: a `caught_up` outside a block \
             commits nothing",
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
    fn first_attach_settings_are_available_once_without_reconnect_duplication() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        assert_eq!(
            client
                .take_first_attach_settings()
                .map(|settings| settings.model_id),
            Some("scripted".to_string()),
        );
        assert!(client.take_first_attach_settings().is_none());
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        client.expect_attach();
        let _ = client.apply(&mut chat, state(EPOCH, false));
        assert!(
            client.take_first_attach_settings().is_none(),
            "a reconnect does not regenerate the local restore summary",
        );
    }

    #[test]
    fn caught_up_flags_a_task_refetch_that_set_tasks_clears() {
        let (mut client, mut chat) = attached();
        assert!(
            client.needs_task_refetch(),
            "task events are not replayable",
        );

        client.set_tasks(
            &mut chat,
            TaskTable {
                tasks: vec![task_summary(7)],
            },
        );

        assert!(!client.needs_task_refetch());
        assert_eq!(chat.tasks().len(), 1);
        assert_eq!(
            chat.tasks().get(&7).map(|task| task.call_id.as_str()),
            Some("call-1"),
            "the read replaces the task model the reducer and footer use",
        );

        // In the interim between `caught_up` and the read landing, a
        // snapshot for a task the client does not know is inert: the
        // reducer freezes output for an untracked task, so the client needs
        // no filter of its own.
        assert!(!client.apply(&mut chat, live(EPOCH, task_output(9))).0);
        assert_eq!(chat.tasks().len(), 1, "the unknown task was ignored");
    }

    /// The queue read is owed after every block too: `QueueUpdate` is
    /// reliable-transient, so a backfill regenerates none of it and a joiner
    /// would show no pending messages at all.
    #[test]
    fn caught_up_flags_a_queue_refetch_that_set_queue_clears() {
        let (mut client, mut chat) = attached();
        assert!(client.needs_queue_refetch());
        assert!(chat.queue().queues.is_empty());

        client.set_queue(
            &mut chat,
            QueueState {
                queues: vec![AgentQueue {
                    agent_id: AgentId::Main,
                    steering: Vec::new(),
                    follow_up: vec![queued("from the read")],
                }],
            },
        );

        assert!(!client.needs_queue_refetch());
        assert_eq!(chat.queue().queues.len(), 1);
        assert_eq!(
            chat.queue().queues[0].follow_up.len(),
            1,
            "the read replaces the queue model the pending box renders",
        );
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

    /// The first-attach twin of the seed test above: a client that was NOT
    /// attached when a background sub started has no mark to keep, so the
    /// block's synthesized opening bracket (an untagged `AgentStart(Sub n)`
    /// the host emits before `caught_up`) is what creates it. After the
    /// fold the inherited sub reads as running.
    #[test]
    fn a_first_attach_block_marks_an_inherited_sub_running() {
        let (mut client, mut chat) = attached();
        assert!(
            !client.lifecycle().is_running(AgentId::Sub(1)),
            "a fresh client holds no mark, the block has to create it, \
             otherwise this test measures nothing",
        );

        client.expect_attach();
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            ),
        );
        let _ = client.apply(&mut chat, state(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        assert!(
            !client.lifecycle().is_running(AgentId::Main),
            "the idle state seed still holds for the main agent",
        );
        assert!(
            client.lifecycle().is_running(AgentId::Sub(1)),
            "the synthesized bracket marks the inherited sub running",
        );
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
        assert!(chat.queue().queues.is_empty());

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

        assert_eq!(chat.queue().queues.len(), 1);
        assert_eq!(chat.queue().queues[0].agent_id, AgentId::Main);
        assert_eq!(chat.queue().queues[0].follow_up.len(), 1);

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

        assert_eq!(chat.queue().queues.len(), 1);
        assert_eq!(chat.queue().queues[0].steering.len(), 1);
        assert!(chat.queue().queues[0].follow_up.is_empty());

        client.set_queue(&mut chat, QueueState::default());
        assert!(chat.queue().queues.is_empty());
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
