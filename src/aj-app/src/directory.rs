//! The client's view of every session a peer offers (spec 6.8, 9.2).
//!
//! One [`SessionDirectory`] holds two very different kinds of knowledge, and
//! keeping them apart is the point of the type:
//!
//! - A row per session the peer reports, from the `list` frames. This is all
//!   a client knows about a session it has never opened, and it is where the
//!   unseen-output glyph comes from.
//! - A [`SessionClient`] plus a transcript per session in the working set.
//!   Live frames keep arriving for these while they sit in the background,
//!   which is what makes switching to one a view swap rather than a rebuild.
//!
//! Background attachment is a bounded working set, LRU over focus (spec
//! section 5). Retaining a session costs a live driver and a held lock on the
//! host, so browsing the store must not leave one behind per session visited.
//! [`WORKING_SET`] bounds it, the focused session is never the one dropped,
//! and a session that falls out keeps its `list` row and so keeps carrying its
//! attention signal. The set holds no archived session but the one the user is
//! in: archiving says they are done there, and dropping it is what lets the
//! host release it. The bit arrives on a row and the exemption moves with
//! focus, so the rule is applied at both (see
//! [`SessionDirectory::retire_archived`]).
//!
//! The focused session's transcript is **not** stored here. A frontend holds
//! it behind widgets that cannot be repointed, so it lives in the frontend's
//! own cell and the directory borrows it for the duration of a call. Focusing
//! another session swaps the two. Every entry point that can touch the focused
//! session therefore takes `focused_chat`, and the invariant is that exactly
//! the focused session's stored transcript is `None`.
//!
//! Attaching is not this type's job: it owns no stream and does no IO. The
//! caller attaches the set [`SessionDirectory::attach_requests`] names and arms
//! the folds the peer served, and a session dropped from the working set is
//! detached by that same reopen leaving it unnamed (spec 6.5).

use std::collections::{HashMap, HashSet};

use aj_agent::events::{AgentEvent, AgentId};
use aj_wire::{DirectoryHost, Frame, SessionSummary};

use crate::chat::{ChatState, Redraw};
use crate::client::{Refusal, SessionClient};
use crate::host::AttachRequest;

/// What a refused session costs the user, folded once when the refusal lands.
///
/// The peer's own refusal is folded immediately above this and says what
/// happened. This says what it means and what will happen next, because the
/// connection stays healthy either way and a transcript that simply stops
/// reads exactly like one that is quiet.
///
/// The promise is one the directory keeps: the row's return to the peer's list
/// is what asks again (see [`SessionDirectory::rejoin_edges_fired`]). Worded as
/// a condition rather than a reassurance, because a session that never leaves
/// the list never returns to it, and a user watching for that is watching for
/// the right thing.
pub const WITHHELD_NOTICE: &str = "Nothing is following this session now. It re-attaches by itself \
                                   if the session returns to the peer's list.";

/// The same, for a session a rival writer holds.
///
/// Its own sentence rather than the one above, because the condition above is
/// the wrong thing to watch for here: a locked session stays on the peer's list
/// for as long as the hold lasts, and what asks again is the hold ending, which
/// the peer's rows report (spec 6.8).
///
/// A condition and not a promise, for the same reason the one above is one, and
/// the condition is what the peer reports rather than what the rival does. A
/// peer that never publishes the bit cannot report the release and this session
/// waits, which spec 6.5 chooses over a retry. Saying it re-attaches "once that
/// writer lets go" would promise the user exactly what that degradation
/// withholds.
pub const WITHHELD_LOCKED_NOTICE: &str = "Nothing is following this session now. Another writer \
                                          holds it, and it re-attaches by itself if the peer \
                                          reports that writer letting go.";

/// What to tell an action that observes a persistence-failed attachment.
pub const WITHHELD_PERSISTENCE_NOTICE: &str = "Nothing is following this session now. It is \
                                               re-attaching through the stopped session's \
                                               replacement.";

/// What to tell the user about a refusal, which is what will end it.
pub fn withheld_notice(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Locked { .. } => WITHHELD_LOCKED_NOTICE,
        Refusal::PersistenceFailed => WITHHELD_PERSISTENCE_NOTICE,
        Refusal::Other => WITHHELD_NOTICE,
    }
}

/// How many sessions a client keeps attached, the focused one included.
///
/// The tradeoff is re-attach cost against what browsing leaves running on the
/// host. Larger, and juggling a working set of sessions never pays for a
/// re-attach, but a walk through a big store piles up live drivers and held
/// locks other processes in the same directory may want (spec section 5).
/// Smaller, and switching among a few sessions starts re-attaching, which
/// costs a full backfill once the host has released one.
///
/// Eight is sized for the juggling the sidebar exists to serve, and caps what
/// a browse can leave behind at eight live sessions.
const WORKING_SET: usize = 8;

/// One session the client folds frames for.
struct Attached {
    session: String,
    client: SessionClient,
    /// The transcript while this session sits in the background. `None` for
    /// the focused session, whose transcript the frontend holds.
    chat: Option<ChatState>,
    /// The durable position the stream has carried this session to: the seq
    /// of the last durable frame routed into its transcript, `None` until one
    /// arrives.
    ///
    /// This is what the user has had on screen while the session was focused,
    /// which is the position [`SessionDirectory::mark_viewed`] records.
    ///
    /// NOTE: deliberately not [`SessionClient::cursor`], which lags by one
    /// durable frame so a re-attach never claims an entry whose trailing
    /// untagged events it may have missed. That conservatism is right for
    /// resumption and wrong here: the last entry of every turn the user
    /// watched would read back as unseen.
    delivered: Option<u64>,
}

/// Every session a peer offers, plus the fold state for the working set.
pub struct SessionDirectory {
    /// The working set, most recently focused first, so the focused session is
    /// the first entry and the eviction candidate is the last. Never longer
    /// than [`WORKING_SET`].
    ///
    /// A vector rather than a map plus a recency list: the bound is small
    /// enough that these scans are cheaper than keeping two structures in
    /// step, and ordering by recency here is what makes "the focused session
    /// is never detached" true by construction.
    attached: Vec<Attached>,
    /// The last `list` frame's rows, in the order the peer sent them.
    rows: Vec<SessionSummary>,
    /// The hosts the last `list` frame named, empty on a plain host's frames.
    ///
    /// Held beside the rows rather than derived from them, because a gateway
    /// names hosts it holds no rows for: after a restart it has none for a host
    /// that is down, and it stores none deliberately (spec 7.1). Those are
    /// exactly the hosts a scan over the rows cannot find, and they are what
    /// tells "unreachable, contents unknown" from "no such host".
    hosts: Vec<DirectoryHost>,
    /// Each session's durable position as of when the user last looked at it,
    /// compared against a row's `last_seq` to derive unseen output (spec 6.8).
    /// Both sides are the host's own sequence numbers, so neither clock enters
    /// and skew cannot make a session look either stale or fresh.
    ///
    /// Kept for sessions dropped from the working set too: being detached does
    /// not make what happened while the user was away seen, and a row outlives
    /// its attachment.
    viewed: HashMap<String, u64>,
    /// Sessions derived to have output the user has not looked at.
    ///
    /// Latched rather than recomputed per read, because the evidence is
    /// perishable while the attention is not (spec 6.8). A cold row carries no
    /// `last_seq`, so a session that moved and then went cold would answer the
    /// quiet way if each read re-derived from the row. The row was only ever
    /// evidence, the attention is this client's own state, so it is derived
    /// where live evidence arrives and cleared where the user looks.
    unseen: HashSet<String>,
    /// The highest released generation each session has fired on (spec 6.5).
    ///
    /// Directory-scoped so eviction, archive retirement, and a later refocus do
    /// not turn an already consumed row into new evidence.
    consumed_lock_generations: HashMap<String, u64>,
}

impl SessionDirectory {
    /// A directory focused on `session`, whose transcript the caller holds.
    ///
    /// The session counts as attached from the start: a frontend reaches this
    /// type having already opened its first stream.
    pub fn new(session: String) -> Self {
        Self {
            attached: vec![Attached {
                client: SessionClient::new(session.clone()),
                session,
                chat: None,
                delivered: None,
            }],
            rows: Vec::new(),
            hosts: Vec::new(),
            viewed: HashMap::new(),
            unseen: HashSet::new(),
            consumed_lock_generations: HashMap::new(),
        }
    }

    /// The session the frontend is rendering.
    pub fn focused(&self) -> &str {
        &self.focused_entry().session
    }

    /// The focused session's fold, and the owner of its agent lifecycle,
    /// cursor, and settings view.
    pub fn client(&self) -> &SessionClient {
        &self.focused_entry().client
    }

    /// The focused session's fold, mutably.
    pub fn client_mut(&mut self) -> &mut SessionClient {
        &mut self.attached[0].client
    }

    /// One session's fold, `None` for a session outside the working set.
    pub fn client_for(&self, session: &str) -> Option<&SessionClient> {
        self.attached
            .iter()
            .find(|attached| attached.session == session)
            .map(|attached| &attached.client)
    }

    /// Whether this client folds frames for `session`.
    pub fn is_attached(&self, session: &str) -> bool {
        self.attached
            .iter()
            .any(|attached| attached.session == session)
    }

    /// The rows from the last `list` frame.
    pub fn rows(&self) -> &[SessionSummary] {
        &self.rows
    }

    /// The hosts from the last `list` frame, empty against a plain host.
    ///
    /// A gateway names every host it has enrolled, the ones it holds no rows
    /// for included, each by its id or, while it has none, by its configured
    /// address (spec 7.1).
    pub fn hosts(&self) -> &[DirectoryHost] {
        &self.hosts
    }

    /// The working set always holds at least the focused session, which
    /// [`Self::new`] establishes and no operation empties.
    fn focused_entry(&self) -> &Attached {
        self.attached
            .first()
            .expect("the working set always holds the focused session")
    }

    /// Fold one frame.
    ///
    /// A session-scoped frame goes to its own session's fold, writing into that
    /// session's transcript, or into `focused_chat` when the frame belongs to
    /// the focused session. A frame for a session outside the working set is
    /// dropped: the host only sends those for sessions the stream named, so one
    /// arriving here is either a peer bug or the tail of a stream this client
    /// has already replaced, and folding it would need a transcript no gesture
    /// has asked for.
    pub fn apply(&mut self, focused_chat: &mut ChatState, frame: Frame) -> Redraw {
        let Some(session) = frame.session() else {
            return self.apply_host_frame(frame);
        };
        let folds_refusal = matches!(&frame, Frame::Error { .. });
        let Some(index) = self
            .attached
            .iter()
            .position(|attached| attached.session == session)
        else {
            return Redraw(false);
        };
        let attached = &mut self.attached[index];
        // Read off the envelope before the fold consumes the frame.
        let delivered = delivered_seq(&frame);
        // What the client was waiting on before this frame, so the notice below
        // can be raised on the transition rather than latched beside it. A flag
        // recording "already said" is a second copy of this fact, and it drifts
        // from it on every path that resumes asking without an edge firing: a
        // reconnect arms the session, a `reset` sends it back, and a refusal
        // after either of those would find the flag still set and say nothing.
        let asked_before = attached.client.withheld();
        let mut redraw = match &mut attached.chat {
            Some(chat) => attached.client.apply(chat, frame),
            None => attached.client.apply(focused_chat, frame),
        };
        // The client raises this when it drops an attachment, so this asks the
        // one place that decides what a refusal is rather than matching the
        // frame kind a second time.
        let asked_now = attached.client.withheld();
        let current_refusal = folds_refusal
            .then_some(asked_now)
            .flatten()
            .filter(|refusal| matches!(refusal, Refusal::Locked { .. }));
        if let Some(refusal) = asked_now
            && asked_now != asked_before
        {
            let chat = match &mut attached.chat {
                Some(chat) => chat,
                None => focused_chat,
            };
            let noticed = attached.client.apply_local(
                chat,
                AgentEvent::Warning {
                    agent_id: AgentId::Main,
                    text: withheld_notice(refusal).to_string(),
                },
            );
            redraw = Redraw(redraw.0 || noticed.0);
        }
        if let Some(seq) = delivered {
            // Last write wins rather than a maximum. A block re-delivers
            // entries at or below the position already reached and commits the
            // true one in its `caught_up`, and a block under a new epoch
            // restarts the numbering below the old one, which a maximum would
            // never come back down from.
            attached.delivered = Some(seq);
            // A frame folded into a background session is first-hand evidence
            // that it moved, and it arrives whether or not a row follows.
            self.latch_unseen();
        }
        // A gateway sends its latest merged list before the spliced refusal.
        // Evaluate that row now because no later list is required to follow.
        let rejoined =
            current_refusal.is_some_and(|refusal| self.current_generation_fired(index, refusal));
        // Only the focused session's transcript is on screen, so a background
        // session's fold changes nothing a redraw would show. Its row can still
        // change, but that arrives as a `list` frame of its own.
        Redraw((redraw.0 || rejoined) && index == 0)
    }

    /// Consume the current row's generation when it answers a refusal that just
    /// folded.
    fn current_generation_fired(&mut self, index: usize, refusal: Refusal) -> bool {
        let session = &self.attached[index].session;
        let row = self.rows.iter().find(|row| row.id == *session);
        let consumed = self.consumed_lock_generations.get(session).copied();
        let Some(generation) = released_generation(refusal, row, consumed) else {
            return false;
        };
        self.consumed_lock_generations
            .insert(session.clone(), generation);
        self.attached[index].client.owe_reattach();
        true
    }

    /// Put the re-attach obligation back on every withheld session whose rejoin
    /// edge has just fired, answering whether any did.
    ///
    /// This is the discriminator the refusal rule turns on
    /// ([`SessionClient::drop_attachment`]). A refusal says attaching cannot
    /// succeed *now*; the peer's directory is the only thing that says when that
    /// could have changed, and the refusal's code names which edge that is
    /// ([`Refusal`]). Asking on any other schedule is either a retry loop or a
    /// timer, and the protocol hands us a fact instead (spec 6.5, 7.1).
    ///
    /// Two edges, and either one re-asks:
    ///
    /// - Absent then present, which every refusal keeps. Ordering-tolerance
    ///   matters here: a withdrawal can refuse the attach before or after the
    ///   row leaves the list, and only the transition says the answer changed.
    /// - The row's `locked` bit true then false, for a `locked` refusal alone. A
    ///   session a rival writer holds stays listed for as long as the hold
    ///   lasts, so absence has no edge to offer there, and the rival letting go
    ///   is the fact that changes that refusal's answer (spec 6.8).
    ///
    /// Both are transitions between the rows one folded list replaces and the
    /// next, never live watches, so a change that happened while the client was
    /// disconnected is read at the first frame after it. Both are set-wide,
    /// because the obligation is: a user who had five sessions attached when a
    /// host went down expects five back when it returns, not the one they happen
    /// to be looking at.
    ///
    /// And a third for a locked refusal, which is not a transition at all: a row
    /// reporting the lock free at the refusal's generation or beyond. A
    /// transition cannot be carried by `list`, which is lossy-coalescible by
    /// contract (spec 6.4), and the locked bit's rise and fall are seconds apart
    /// by design, so a client that did not drain in between is handed the fall's
    /// snapshot alone and has a baseline that never saw the rise. The generation
    /// is what makes that one snapshot sufficient (spec 6.5, 6.8). Two rules
    /// keep it from becoming the poll the other two exist to avoid: it reads at
    /// or beyond rather than different, so a snapshot older than the refusal can
    /// never fire, and a fire consumes the generation it read
    /// in [`Self::consumed_lock_generations`], so a peer that keeps republishing
    /// one released generation is asked once.
    ///
    /// Two consequences of reading transitions, both intended. Against a peer
    /// that publishes neither the bit nor a generation a locked refusal waits on
    /// absence alone: that is the gap an old peer always had, disclosed rather
    /// than filled with a timer. And a client holding no rows yet, before its
    /// first `list`, has no baseline to transition from, so a first row arriving
    /// after a refusal does ask again, whatever the code: that is new
    /// information rather than a spin, and it can happen once.
    fn rejoin_edges_fired(&mut self, sessions: &[SessionSummary]) -> bool {
        fn row<'a>(rows: &'a [SessionSummary], id: &str) -> Option<&'a SessionSummary> {
            rows.iter().find(|row| row.id == id)
        }
        let mut asked = false;
        let consumed = &mut self.consumed_lock_generations;
        for attached in self.attached.iter_mut() {
            let Some(refusal) = attached.client.withheld() else {
                continue;
            };
            let before = row(&self.rows, &attached.session);
            let after = row(sessions, &attached.session);
            let returned = before.is_none() && after.is_some();
            let released = matches!(refusal, Refusal::Locked { .. })
                && before.is_some_and(|row| row.locked)
                && after.is_some_and(|row| !row.locked);
            let spent = consumed.get(&attached.session).copied();
            let published = released_generation(refusal, after, spent);
            if !returned && !released && published.is_none() {
                continue;
            }
            if let Some(generation) = published {
                consumed.insert(attached.session.clone(), generation);
            }
            attached.client.owe_reattach();
            asked = true;
        }
        asked
    }

    /// Fold a frame carrying no session: the directory's own rows, or a kind
    /// this type has no use for.
    fn apply_host_frame(&mut self, frame: Frame) -> Redraw {
        match frame {
            Frame::List { sessions, hosts } => {
                // The hosts are part of the answer: a host the gateway holds no
                // rows for has no row to carry its label or its reachability,
                // so comparing rows alone would render such a host once and
                // never update it again (spec 7.1).
                let changed = self.rows != sessions || self.hosts != hosts;
                let rejoined = self.rejoin_edges_fired(&sessions);
                self.rows = sessions;
                self.hosts = hosts;
                self.latch_unseen();
                // A session archived from anywhere else lands here, so this is
                // where it leaves the set. The session on screen stays: it is
                // the one the user is in, and it goes when they leave it.
                let focused = self.focused().to_string();
                let retired = self.retire_archived(&focused);
                Redraw(changed || !retired.is_empty() || rejoined)
            }
            // `vms` belongs to whatever renders VM state and `heartbeat` exists
            // to keep the connection warm, so neither is the directory's to
            // hold.
            _ => Redraw(false),
        }
    }

    /// Move focus to `session`, swapping its transcript into `focused_chat`,
    /// and answer the session this displaced from the working set.
    ///
    /// `mint` builds the transcript for a session focused for the first time,
    /// which is also what attaches it (spec 9.2). It runs only in that case, so
    /// a caller can put whatever a fresh transcript costs behind it.
    ///
    /// A displaced session is detached, which takes effect when the caller
    /// reopens its stream over [`Self::attach_requests`] without it. Its
    /// transcript is dropped: re-attach reconciliation absorbs a rebuild, so
    /// keeping it would buy nothing a cursor does not.
    ///
    /// Focusing the already-focused session leaves everything alone rather than
    /// cycling its transcript out and back, and displaces nothing.
    pub fn focus(
        &mut self,
        focused_chat: &mut ChatState,
        session: &str,
        mint: impl FnOnce() -> ChatState,
    ) -> Option<String> {
        if session == self.focused() {
            return None;
        }
        // The incoming entry goes to the front, which pushes the session being
        // left to index 1 either way. Taking the incoming transcript before
        // parking the outgoing one means a `mint` that panicked would leave the
        // frontend's cell holding a live transcript rather than none.
        let incoming = match self
            .attached
            .iter()
            .position(|attached| attached.session == session)
        {
            Some(index) => {
                let chat = self.attached[index]
                    .chat
                    .take()
                    .expect("only the focused session's transcript is on loan");
                let entry = self.attached.remove(index);
                self.attached.insert(0, entry);
                chat
            }
            None => {
                let chat = mint();
                self.attached.insert(
                    0,
                    Attached {
                        client: SessionClient::new(session.to_string()),
                        session: session.to_string(),
                        chat: None,
                        delivered: None,
                    },
                );
                chat
            }
        };
        let outgoing = std::mem::replace(focused_chat, incoming);
        let previous = &mut self.attached[1];
        previous.chat = Some(outgoing);
        let previous = previous.session.clone();
        // Everything the session did while it was the focused one was on
        // screen, so leaving is the moment its output counts as seen.
        self.mark_viewed(&previous);
        // Leaving an archived session is leaving it for good: the user said
        // they were done there, so it goes rather than sitting in the set
        // holding a lock the host could release. Only here, once the incoming
        // transcript is in the frontend's cell and the outgoing one is parked,
        // because dropping an entry before that would take the transcript with
        // it.
        self.retire_archived(session);
        // NOTE: the truncation here and the one `attach_requests` applies to
        // the same admission have to agree, or the reopened stream would name a
        // session this no longer folds (or drop one it does). Both keep the
        // first `WORKING_SET` entries after the incoming session takes the
        // front, and only one session is ever admitted at a time.
        (self.attached.len() > WORKING_SET)
            .then(|| self.attached.pop().expect("longer than the bound").session)
    }

    /// Whether the working set may hold `session` while `keep` is the one the
    /// user is on: everything but an archived session the user is not in.
    ///
    /// The one place the rule is written. [`Self::attach_requests`] names the
    /// set the peer should serve and [`Self::retire_archived`] lands the set on
    /// it, so the two answering differently would leave the client folding a
    /// session no stream feeds, or holding a lock for one it has dropped.
    ///
    /// Archived is off the rows and nothing else: the bit is the peer's to
    /// publish (spec 6.8), so a session the client has seen no row for is one
    /// it keeps holding.
    fn held(&self, session: &str, keep: &str) -> bool {
        session == keep
            || !self
                .rows
                .iter()
                .find(|row| row.id == session)
                .is_some_and(|row| row.archived)
    }

    /// Drop every archived session but `keep` from the working set, answering
    /// the ids dropped.
    ///
    /// The invariant's one enforcement point, run wherever it can break: a
    /// focus, which changes who is exempt, and a `list` frame, which is where
    /// the bit arrives. A dropped session keeps its row and its viewed stamp,
    /// like any other session that falls out of the set.
    ///
    /// Dropping it here is what makes the strip stop drawing it. The peer stops
    /// serving it when the client next opens a stream, which a focus does at
    /// once (see [`Self::would_retire`]) and which a bit arriving from
    /// elsewhere waits for.
    fn retire_archived(&mut self, keep: &str) -> Vec<String> {
        let retiring: Vec<String> = self
            .attached
            .iter()
            .map(|attached| attached.session.clone())
            .filter(|session| !self.held(session, keep))
            .collect();
        self.attached
            .retain(|attached| !retiring.contains(&attached.session));
        retiring
    }

    /// Record the archive bit the peer accepted for `session`.
    ///
    /// The rows are the peer's answer and the next `list` frame overwrites
    /// this. Writing the accepted result in ahead of that frame is what makes
    /// the gesture answer to itself: the peer coalesces its rows on a tick, and
    /// a user pressing the chord twice inside one would otherwise archive an
    /// archived session rather than undoing it.
    ///
    /// Nothing is written for a session the client has no row for, which is a
    /// session the strip is not drawing either.
    pub fn mark_archived(&mut self, session: &str, archived: bool) {
        let Some(row) = self.rows.iter_mut().find(|row| row.id == session) else {
            return;
        };
        row.archived = archived;
        let focused = self.focused().to_string();
        self.retire_archived(&focused);
    }

    /// Whether focusing `session` would drop anything from the working set.
    ///
    /// The caller reopens its stream on a focus that changes the set, and a
    /// focus onto a session already in the set is otherwise served by the
    /// stream it has. Without this the archived session the user just left
    /// would stay named on that stream, and the host would go on holding it
    /// for a client that has stopped drawing it.
    pub fn would_retire(&self, session: &str) -> bool {
        self.attached
            .iter()
            .any(|attached| !self.held(&attached.session, session))
    }

    /// Note that the user has stopped looking at `session`, so its output up to
    /// this point counts as seen.
    ///
    /// Recorded as the user leaves rather than as they arrive. A session's
    /// output climbs while it is the focused one, and all of that was on
    /// screen, so a position taken on arrival would make everything the user
    /// just watched read as unseen the moment they switched away.
    ///
    /// What is recorded is this client's own fold position, never the one the
    /// row reports. `list` frames are coalesced on a tick (spec 6.8), so the
    /// row in hand at this moment predates output the user watched arrive, and
    /// recording it would announce that output as unseen.
    fn mark_viewed(&mut self, session: &str) {
        let delivered = self
            .attached
            .iter()
            .find(|attached| attached.session == session)
            .and_then(|attached| attached.delivered)
            // A session left before its stream delivered anything showed the
            // user nothing, and position zero is exactly that. Recording
            // nothing instead would leave the never-viewed rule answering for
            // a session the user did view, so it could never report unseen
            // until they had visited it a second time.
            .unwrap_or(0);
        self.viewed.insert(session.to_string(), delivered);
        // Looking at it is what discharges the mark.
        self.unseen.remove(session);
    }

    /// Derive the unseen mark from whatever live evidence is currently in hand,
    /// for every session the user has viewed at least once.
    ///
    /// Only ever sets. Clearing is [`Self::mark_viewed`]'s job, which is what
    /// makes the mark survive its evidence going away.
    fn latch_unseen(&mut self) {
        let mut moved: Vec<String> = Vec::new();
        for (session, viewed) in &self.viewed {
            // NOTE: the focused session is not skipped here. It can pick up the
            // mark while the user watches it, which is harmless twice over:
            // `is_unseen` answers no for it whatever this holds, and leaving it
            // runs `mark_viewed`, which discharges the mark on the way out.
            //
            // Both kinds of evidence spec 6.8 admits: frames folded while
            // attached, and a live row's own position while not.
            let applied = self
                .attached
                .iter()
                .find(|attached| attached.session == *session)
                .and_then(|attached| attached.delivered);
            let listed = self
                .rows
                .iter()
                .find(|row| row.id == *session)
                .and_then(|row| row.last_seq);
            if [applied, listed]
                .into_iter()
                .flatten()
                .any(|seq| seq > *viewed)
            {
                moved.push(session.clone());
            }
        }
        self.unseen.extend(moved);
    }

    /// Whether `row` has output the user has not looked at.
    ///
    /// True when the session is idle and its durable position is past the one
    /// recorded at the last view. A working session is excluded because its
    /// glyph says it is working, which is the more useful fact, and the unseen
    /// mark is what remains once it stops (spec 6.8).
    ///
    /// The focused session is never unseen: the user is looking at it.
    ///
    /// The caller passes the row rather than a session id because the one
    /// caller walks the whole directory asking about each row, and recovering
    /// a row it already holds would make that walk quadratic.
    pub fn is_unseen(&self, row: &SessionSummary) -> bool {
        if row.id == self.focused() || row.working {
            return false;
        }
        self.unseen.contains(&row.id)
    }

    /// Whether any session in the working set owes a re-attach.
    ///
    /// A `reset` obliges the session it names, and one reopen discharges the
    /// whole set (spec 6.5), so the question a caller has to ask is set-wide.
    /// Asking only about the focused session would leave a background session
    /// that was reset folding nothing: every later frame carries an epoch its
    /// fold filters out, so its transcript would freeze on the abandoned branch
    /// and a switch onto it would paint that.
    pub fn needs_reattach(&self) -> bool {
        self.attached
            .iter()
            .any(|attached| attached.client.needs_reattach())
    }

    /// Arm every session the peer reports it served, for the attach blocks a
    /// freshly opened stream carries.
    ///
    /// `served` is the peer's own answer, never the request we sent: an arm for
    /// a block that never arrives strands that session's fold, and a session
    /// the peer did attach but we left unarmed folds its block as live frames
    /// (see [`SessionClient::expect_attach`]). Arming the whole set in one call
    /// is what keeps those two failures out of reach of a caller loop that
    /// covers only some of it.
    pub fn expect_attach(&mut self, served: impl Fn(&str) -> bool) {
        for attached in self.attached.iter_mut() {
            if served(&attached.session) {
                attached.client.expect_attach();
            }
        }
    }

    /// The attach set to open a stream over, each session offering its own
    /// cursor, focused first.
    ///
    /// One stream carries all of them, because a stream serves the set it was
    /// opened with (spec 6.5) and a client that lost one lost them all. The
    /// focused session comes first so its catch-up is the first block on the new
    /// stream, which is the one the user is waiting to see.
    ///
    /// `admitting` names a session about to be focused, so the answer is the set
    /// that focus will leave: the new session first, and the session it
    /// displaces from the working set absent, which is how the reopen detaches
    /// it. Pass `None` to re-attach the set as it stands.
    ///
    /// An admitted session leads whether or not it is already attached, and is
    /// named exactly once. A reset on a background session reopens the stream
    /// admitting a session the set already holds, and the consumer waits for
    /// the admitted session's catch-up before it paints the switch.
    ///
    /// Archived sessions are passed over, so the stream this opens is what
    /// detaches them. The admitted one is named regardless: a user who focuses
    /// an archived session is asking to work in it, and a focus that attached
    /// nothing would leave the frontend on a transcript no stream feeds.
    pub fn attach_requests(&self, admitting: Option<&str>) -> Vec<AttachRequest> {
        let mut requests = Vec::with_capacity(WORKING_SET);
        if let Some(session) = admitting {
            requests.push(AttachRequest {
                session: session.to_string(),
                // A session already in the set offers what it folded, so the
                // reopen serves it a suffix rather than a whole history.
                cursor: self.client_for(session).and_then(|client| client.cursor()),
            });
        }
        let kept = admitting.unwrap_or_else(|| self.focused());
        let room = WORKING_SET - requests.len();
        requests.extend(
            self.attached
                .iter()
                .filter(|attached| Some(attached.session.as_str()) != admitting)
                .filter(|attached| self.held(&attached.session, kept))
                .take(room)
                .map(|attached| AttachRequest {
                    session: attached.session.clone(),
                    cursor: attached.client.cursor(),
                }),
        );
        requests
    }

    /// Drop every attached session except `keep`, which is what a narrowed
    /// re-attach leaves behind on the peer, and answer the ids dropped.
    ///
    /// A dropped session stops being folded here, so a later focus onto it
    /// takes the full attach path and re-attaches it rather than swapping to a
    /// transcript the stream no longer feeds. Its `list` row and its viewed
    /// stamp both stay: the session is still one the user is meant to be aware
    /// of, and being detached does not make the output it produced while they
    /// were away seen.
    ///
    /// The focused session is never dropped, `keep` or not. Its transcript is
    /// the one on loan to the frontend and this type cannot repoint that cell,
    /// so dropping it would leave the frontend rendering a session nothing
    /// folds. In practice `keep` is the focused session, and a caller that
    /// narrows onto another one gets both back.
    pub fn drop_all_but(&mut self, keep: &str) -> Vec<String> {
        let focused = self.focused().to_string();
        let mut dropped = Vec::new();
        self.attached.retain(|attached| {
            if attached.session == keep || attached.session == focused {
                return true;
            }
            dropped.push(attached.session.clone());
            false
        });
        dropped
    }

    /// A background session's parked transcript, `None` for the focused session
    /// (whose transcript the frontend holds) or one outside the working set.
    ///
    /// For tests that need to watch a session fold while nothing renders it.
    /// Production readers go through the focused transcript, which is the only
    /// one on screen.
    #[cfg(any(test, feature = "test-support"))]
    pub fn parked_chat(&self, session: &str) -> Option<&ChatState> {
        self.attached
            .iter()
            .find(|attached| attached.session == session)
            .and_then(|attached| attached.chat.as_ref())
    }

    /// Point the focused entry at `session`, keeping its fold, its transcript,
    /// and whatever it owes.
    ///
    /// Staging only, for the frontend tests that need a client folding a session
    /// the peer does not have. That is what a permanently refused attach looks
    /// like from this side, and no honest gesture produces it.
    ///
    /// The fold keeps the id it was built with, so it also keeps what it owes,
    /// which is the point: the attach this names will be refused and the
    /// obligation has to survive the refusal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn rename_focused(&mut self, session: String) -> String {
        std::mem::replace(&mut self.attached[0].session, session)
    }
}

/// The durable position a frame carries into a transcript, `None` for a frame
/// that carries none.
///
/// A `state` frame reports one too, but it names where the host stood when it
/// was emitted, which is ahead of anything the client has folded: the block
/// that frame opens is what delivers the entries up to it.
fn delivered_seq(frame: &Frame) -> Option<u64> {
    match frame {
        Frame::Event { durability, .. } => durability.as_ref().map(|durable| durable.seq),
        Frame::CaughtUp { last_seq, .. } => Some(*last_seq),
        _ => None,
    }
}

/// The generation at which `row` is evidence that the hold behind `refusal` has
/// ended, `None` when it is no such evidence (spec 6.5).
///
/// `consumed` is what this session's edge has already fired on. Both
/// comparisons are load-bearing and neither may be loosened:
///
/// - At or beyond the refusal's generation, never merely different. A snapshot
///   taken before the refusal carries a smaller generation, and under a
///   difference test every one of them would fire, which is the retry loop the
///   whole rule exists to refuse.
/// - Strictly beyond what was already fired on, which is what one release firing
///   once means. Without it a peer that keeps republishing an unchanged released
///   generation is asked once per `list`, and a peer refusing with a generation
///   it has already published as free spins.
fn released_generation(
    refusal: Refusal,
    row: Option<&SessionSummary>,
    consumed: Option<u64>,
) -> Option<u64> {
    let refused_at = refusal.generation()?;
    let row = row?;
    if row.locked {
        // A row still claiming the hold says nothing about it ending, whatever
        // its generation.
        return None;
    }
    let generation = row.lock_generation?;
    let spent = consumed.is_some_and(|fired| generation <= fired);
    (generation >= refused_at && !spent).then_some(generation)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_wire::QueueCounts;
    use chrono::DateTime;

    use super::*;
    use crate::chat::EntryKind;

    const FOCUSED: &str = "session-focused";
    const OTHER: &str = "session-other";
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

    /// The notices in a transcript, which is what these tests read a fold
    /// off: a notice takes its whole identity from the frame.
    fn notices(chat: &ChatState) -> Vec<String> {
        chat.transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Notice(notice) => Some(notice.text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn state(session: &str) -> Frame {
        Frame::State {
            session: session.to_string(),
            epoch: EPOCH.to_string(),
            working: false,
            settings: settings(),
            last_seq: 0,
        }
    }

    fn caught_up(session: &str, last_seq: u64) -> Frame {
        Frame::CaughtUp {
            session: session.to_string(),
            epoch: EPOCH.to_string(),
            last_seq,
        }
    }

    fn durable(session: &str, seq: u64, text: &str) -> Frame {
        Frame::Event {
            session: session.to_string(),
            epoch: EPOCH.to_string(),
            durability: Some(aj_wire::DurableEvent {
                seq,
                entry_id: format!("entry-{seq}"),
            }),
            event: AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: text.to_string(),
            }
            .into(),
        }
    }

    /// A live row at durable position `last_seq`.
    ///
    /// `last_activity` is fixed, and far enough in the past that any answer
    /// reaching for the local clock would read it as ancient: the unseen mark
    /// is derived from positions, so no test here may turn on the stamp.
    fn row(id: &str, working: bool, last_seq: u64) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            live: true,
            working,
            queued: QueueCounts::default(),
            tasks: 0,
            last_seq: Some(last_seq),
            last_activity: DateTime::from_timestamp(0, 0).expect("a valid timestamp"),
            tag: None,
            host: None,
            unreachable: false,
            archived: false,
            locked: false,
            lock_generation: None,
        }
    }

    /// A released session's row: no durable position, which is what makes the
    /// latch load-bearing (spec 6.8).
    fn cold_row(id: &str) -> SessionSummary {
        SessionSummary {
            last_seq: None,
            live: false,
            ..row(id, false, 0)
        }
    }

    /// A row for a session a rival writer holds, so asking this peer for it
    /// would be refused right now (spec 6.8).
    ///
    /// No generation, which is what a peer that publishes none says. The
    /// generation-carrying rows are [`held_at`] and [`free_at`].
    fn held_row(id: &str) -> SessionSummary {
        SessionSummary {
            locked: true,
            ..row(id, false, 0)
        }
    }

    /// A held row naming which hold it is (spec 6.8).
    fn held_at(id: &str, generation: u64) -> SessionSummary {
        SessionSummary {
            lock_generation: Some(generation),
            ..held_row(id)
        }
    }

    /// A row reporting the lock free as of `generation`: the snapshot a client
    /// refused over that hold, or an earlier one, reads the release off.
    fn free_at(id: &str, generation: u64) -> SessionSummary {
        SessionSummary {
            lock_generation: Some(generation),
            ..row(id, false, 0)
        }
    }

    /// A per-session attach refusal, as a peer sends one (spec 6.5).
    fn refusal(session: &str, code: &str) -> Frame {
        refusal_naming(session, code, None)
    }

    /// A locked refusal that names the acquire it answered.
    fn refused_at(session: &str, generation: u64) -> Frame {
        refusal_naming(session, "locked", Some(generation))
    }

    fn refusal_naming(session: &str, code: &str, lock_generation: Option<u64>) -> Frame {
        Frame::Error {
            session: session.to_string(),
            epoch: None,
            code: code.to_string(),
            message: format!("this peer will not serve {session}: {code}"),
            lock_generation,
        }
    }

    fn list(sessions: Vec<SessionSummary>) -> Frame {
        list_of(sessions, Vec::new())
    }

    /// A gateway's `list` frame: rows, and the hosts they came from.
    fn list_of(sessions: Vec<SessionSummary>, hosts: Vec<DirectoryHost>) -> Frame {
        Frame::List { sessions, hosts }
    }

    /// A host the gateway has spoken to, so it knows its id.
    fn learned(id: &str, unreachable: bool) -> DirectoryHost {
        DirectoryHost {
            id: Some(id.to_string()),
            address: None,
            name: None,
            unreachable,
        }
    }

    /// A configured host the gateway has never reached: no id to name it by,
    /// its address instead, and no rows at all (spec 7.1).
    fn configured(address: &str) -> DirectoryHost {
        DirectoryHost {
            id: None,
            address: Some(address.to_string()),
            name: None,
            unreachable: true,
        }
    }

    /// What the sidebar asks: the unseen mark for the row the directory holds
    /// under `id`, `false` when there is no such row.
    fn unseen(directory: &SessionDirectory, id: &str) -> bool {
        directory
            .rows()
            .iter()
            .find(|row| row.id == id)
            .is_some_and(|row| directory.is_unseen(row))
    }

    /// Exactly the focused entry's transcript is on loan to the frontend.
    fn transcripts_are_on_loan_once(directory: &SessionDirectory) {
        for (index, attached) in directory.attached.iter().enumerate() {
            assert_eq!(
                attached.chat.is_none(),
                index == 0,
                "{} holds the wrong side of the transcript loan",
                attached.session,
            );
        }
    }

    /// A directory focused on `FOCUSED` with `OTHER` attached in the
    /// background, both caught up, plus the frontend's transcript.
    ///
    /// `OTHER` gets there the way a real client does: a first focus attaches it
    /// (spec 9.2), then the user switches back.
    fn two_sessions() -> (SessionDirectory, ChatState) {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();

        directory.focus(&mut focused_chat, OTHER, chat);
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        directory.expect_attach(|_| true);

        for session in [FOCUSED, OTHER] {
            let _ = directory.apply(&mut focused_chat, state(session));
            let _ = directory.apply(&mut focused_chat, caught_up(session, 0));
        }
        (directory, focused_chat)
    }

    /// A background session's frames fold into its own transcript, never into
    /// the one on screen. Without the per-session routing every attached
    /// session's output would pile into the focused transcript, which is the
    /// whole reason the sidebar can keep sessions attached at all.
    #[test]
    fn a_background_session_folds_into_its_own_transcript() {
        let (mut directory, mut focused_chat) = two_sessions();

        let redraw = directory.apply(&mut focused_chat, durable(OTHER, 1, "from the background"));
        assert!(
            !redraw.0,
            "a transcript nobody is looking at does not ask for a repaint",
        );
        assert!(
            notices(&focused_chat).is_empty(),
            "the focused transcript is untouched: {:?}",
            notices(&focused_chat),
        );

        let redraw = directory.apply(&mut focused_chat, durable(FOCUSED, 1, "in the foreground"));
        assert!(redraw.0, "the focused session's fold does ask for one");
        assert_eq!(notices(&focused_chat), vec!["in the foreground"]);

        // The background fold really happened, it just landed elsewhere.
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert_eq!(notices(&focused_chat), vec!["from the background"]);
    }

    /// Focusing a background session swaps the two transcripts, so what it
    /// folded while out of view is on screen immediately and the session
    /// being left keeps everything it had. This is the "view swap, not a
    /// rebuild" the sidebar rests on (spec 9.2).
    #[test]
    fn focusing_swaps_the_transcripts_both_ways() {
        let (mut directory, mut focused_chat) = two_sessions();
        let _ = directory.apply(&mut focused_chat, durable(FOCUSED, 1, "in the foreground"));
        let _ = directory.apply(&mut focused_chat, durable(OTHER, 1, "from the background"));

        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert_eq!(directory.focused(), OTHER);
        assert_eq!(
            notices(&focused_chat),
            vec!["from the background"],
            "the background session's own history is what comes on screen",
        );

        // And back, onto state that was parked rather than rebuilt.
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        assert_eq!(
            notices(&focused_chat),
            vec!["in the foreground"],
            "the session left behind kept its transcript",
        );

        // Routing follows the swap: what is now the background session folds
        // out of view.
        let redraw = directory.apply(&mut focused_chat, durable(OTHER, 2, "later"));
        assert!(!redraw.0);
        assert_eq!(notices(&focused_chat), vec!["in the foreground"]);
    }

    /// A first focus mints the transcript and attaches, which is how a
    /// session the user has only ever seen as a row becomes one they can
    /// read (spec 9.2).
    #[test]
    fn a_first_focus_mints_and_attaches() {
        let (mut directory, mut focused_chat) = two_sessions();
        assert!(!directory.is_attached("session-fresh"));

        let mut minted = false;
        directory.focus(&mut focused_chat, "session-fresh", || {
            minted = true;
            chat()
        });
        assert!(minted, "a session with no transcript gets one");
        assert!(directory.is_attached("session-fresh"));
        assert_eq!(directory.focused(), "session-fresh");
        assert!(notices(&focused_chat).is_empty());

        // The session left behind is parked, not lost.
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        assert_eq!(directory.focused(), FOCUSED);
    }

    /// Focusing the session already focused changes nothing. Its transcript
    /// is the one on loan, so a swap would have to park and un-park the same
    /// cell, and `mint` must not run.
    #[test]
    fn refocusing_the_focused_session_is_inert() {
        let (mut directory, mut focused_chat) = two_sessions();
        let _ = directory.apply(&mut focused_chat, durable(FOCUSED, 1, "in the foreground"));

        directory.focus(&mut focused_chat, FOCUSED, || panic!("no mint, no swap"));
        assert_eq!(directory.focused(), FOCUSED);
        assert_eq!(notices(&focused_chat), vec!["in the foreground"]);
    }

    /// A frame for a session this client never attached is dropped rather
    /// than folded into whatever transcript happens to be on screen.
    #[test]
    fn a_frame_for_an_unattached_session_is_dropped() {
        let (mut directory, mut focused_chat) = two_sessions();

        let before = directory.client().cursor();
        let redraw = directory.apply(&mut focused_chat, durable("session-stranger", 1, "stray"));
        assert!(!redraw.0);
        assert!(
            !directory.is_attached("session-stranger"),
            "a frame is not a reason to attach the session it names",
        );
        // NOTE: the focused fold would also turn this frame away, by its own
        // session id, so what the directory's routing owns here is the pair
        // above: no attach, and no repaint. The cursor is checked to pin that
        // nothing was folded on the way to being dropped.
        assert_eq!(directory.client().cursor(), before);
        assert!(
            notices(&focused_chat).is_empty(),
            "and nothing reached the focused transcript: {:?}",
            notices(&focused_chat),
        );
    }

    /// The directory owns the rows, and an unchanged list asks for no
    /// repaint: `list` is cumulative, so a resend carries no news.
    #[test]
    fn list_frames_own_the_rows_and_only_changes_repaint() {
        let (mut directory, mut focused_chat) = two_sessions();
        assert!(directory.rows().is_empty());

        let sessions = vec![row(FOCUSED, false, 10), row(OTHER, true, 20)];
        let redraw = directory.apply(&mut focused_chat, list(sessions.clone()));
        assert!(redraw.0, "the first rows are news");
        assert_eq!(directory.rows().len(), 2);

        let redraw = directory.apply(&mut focused_chat, list(sessions));
        assert!(!redraw.0, "the same rows again are not");

        // The other host-level kinds are nobody's business here.
        for frame in [Frame::Heartbeat, Frame::Vms { vms: Vec::new() }] {
            assert!(!directory.apply(&mut focused_chat, frame).0);
        }
        assert_eq!(directory.rows().len(), 2, "and they leave the rows alone");
    }

    /// A gateway's `list` frame names the hosts it has enrolled beside the
    /// rows, and a change confined to those hosts is news like any other.
    ///
    /// A host the gateway holds no rows for has no row to carry its state, so
    /// a repaint predicate that read only the rows would draw such a host once
    /// and never again: it could go out, come back, or learn its id, and the
    /// strip would keep the first label and mark it was given (spec 7.1).
    #[test]
    fn a_change_confined_to_the_hosts_is_still_news() {
        let (mut directory, mut focused_chat) = two_sessions();
        assert!(directory.hosts().is_empty(), "a plain host names none");

        // One host answering, one configured host that never has, and a row on
        // neither of them: the rows stay untouched throughout.
        let sessions = vec![row(FOCUSED, false, 10)];
        let enrolled = vec![learned("builder-1", false), configured("10.0.0.7:7777")];
        let redraw = directory.apply(
            &mut focused_chat,
            list_of(sessions.clone(), enrolled.clone()),
        );
        assert!(redraw.0, "the first hosts are news");
        assert_eq!(directory.hosts(), enrolled.as_slice());

        let redraw = directory.apply(
            &mut focused_chat,
            list_of(sessions.clone(), enrolled.clone()),
        );
        assert!(!redraw.0, "the same directory again is not");

        // The host goes out. Nothing about the rows says so.
        let gone = vec![learned("builder-1", true), configured("10.0.0.7:7777")];
        let redraw = directory.apply(&mut focused_chat, list_of(sessions.clone(), gone.clone()));
        assert!(
            redraw.0,
            "a host going out is news the rows cannot carry: {:?}",
            directory.hosts(),
        );
        assert_eq!(directory.hosts(), gone.as_slice());

        // And the configured host answers for the first time, which is where
        // its id comes from and what the strip relabels its group by.
        let met = vec![learned("builder-1", true), learned("builder-2", false)];
        let redraw = directory.apply(&mut focused_chat, list_of(sessions.clone(), met.clone()));
        assert!(redraw.0, "a host learning its id is news too");
        assert_eq!(directory.hosts(), met.as_slice());
        assert_eq!(
            directory.rows(),
            sessions.as_slice(),
            "and none of it disturbed the rows",
        );
    }

    /// Output produced while a session was the focused one is output the user
    /// watched, so switching away must not leave it marked unseen.
    ///
    /// The row in hand when the user leaves predates that output: `list` frames
    /// are coalesced on a tick (spec 6.8), so the frame reporting the turn
    /// lands after the switch. What the client records is therefore its own
    /// fold position, which is exactly what was on screen.
    #[test]
    fn what_the_user_watched_while_focused_is_not_unseen_afterwards() {
        let (mut directory, mut focused_chat) = two_sessions();

        // Away and back, so `FOCUSED` has a recorded position and the
        // never-viewed rule cannot answer for it.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 10), row(OTHER, false, 10)]),
        );
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));

        // A turn runs in `FOCUSED`, on screen the whole time. No `list` frame
        // reports it yet: the coalescing tick has not fired.
        let _ = directory.apply(&mut focused_chat, durable(FOCUSED, 50, "watched"));

        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 50), row(OTHER, false, 10)]),
        );
        assert!(
            !unseen(&directory, FOCUSED),
            "the user watched that turn happen, so leaving cannot mark it unseen",
        );

        // What does count is what happens after they left.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 90), row(OTHER, false, 10)]),
        );
        assert!(
            unseen(&directory, FOCUSED),
            "output after the switch is unseen",
        );
    }

    /// A session the user left before its first row still records what they
    /// saw. The client's own fold position is known whether or not a row has
    /// arrived, so the never-viewed rule cannot go on answering for a session
    /// the user did view (spec 6.8).
    #[test]
    fn a_session_left_before_its_first_row_still_reports_later_output() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();

        // Left inside the window before any `list` frame, which is where every
        // freshly created session and every connect starts.
        directory.focus(&mut focused_chat, OTHER, chat);

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 5), row(OTHER, false, 0)]),
        );
        assert!(
            unseen(&directory, FOCUSED),
            "the user did view it, so a row past what they saw is unseen",
        );
    }

    /// The unseen mark latches: derived while the evidence was live, it holds
    /// after the session goes cold and its row loses `last_seq` (spec 6.8). The
    /// row was only ever evidence, the attention is this client's own state.
    #[test]
    fn the_unseen_mark_outlives_the_row_that_proved_it() {
        let (mut directory, mut focused_chat) = two_sessions();
        // Leave FOCUSED, which records what the user had seen of it.
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 5), row(OTHER, false, 0)]),
        );
        assert!(unseen(&directory, FOCUSED), "the live row proves it moved");

        // The session is released and its row goes cold, taking the evidence
        // with it. The mark must not go with it.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![cold_row(FOCUSED), row(OTHER, false, 0)]),
        );
        assert!(
            unseen(&directory, FOCUSED),
            "a cold row does not un-see what the user never looked at",
        );

        // Looking at it is what discharges the mark.
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert!(
            !unseen(&directory, FOCUSED),
            "viewing it clears the mark, cold row or not",
        );
    }

    /// A frame folded into a background session is evidence in its own right,
    /// so the mark does not wait on a `list` frame to follow it.
    #[test]
    fn a_background_fold_is_evidence_enough() {
        let (mut directory, mut focused_chat) = two_sessions();
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        // A row exists, but a stale one: it says FOCUSED has moved nowhere.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![cold_row(FOCUSED), row(OTHER, false, 0)]),
        );
        assert!(!unseen(&directory, FOCUSED), "nothing has moved yet");

        // A durable frame folds into it while the user is elsewhere.
        let _ = directory.apply(&mut focused_chat, durable(FOCUSED, 9, "while away"));
        assert!(
            unseen(&directory, FOCUSED),
            "the fold is first-hand evidence, no row needed",
        );
    }

    /// The rows outlive a focus change. They are the peer's directory, not the
    /// focused session's, and a sidebar has to keep listing every session while
    /// the user moves between them.
    #[test]
    fn the_rows_survive_a_focus_change() {
        let (mut directory, mut focused_chat) = two_sessions();
        let sessions = vec![row(FOCUSED, false, 10), row(OTHER, true, 20)];
        let _ = directory.apply(&mut focused_chat, list(sessions.clone()));

        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert_eq!(directory.rows(), sessions.as_slice());
        directory.focus(&mut focused_chat, "session-fresh", chat);
        assert_eq!(
            directory.rows(),
            sessions.as_slice(),
            "attaching a session the rows do not mention leaves them alone",
        );
    }

    /// Unseen output is derived by comparing two of the host's own durable
    /// positions, the row's current one against the one the client had folded
    /// when the user last looked. Neither side is a clock, so no skew and no
    /// stale stamp can invent or hide the glyph (spec 6.8).
    #[test]
    fn unseen_output_compares_durable_positions() {
        let (mut directory, mut focused_chat) = two_sessions();

        // `OTHER` folds up to the position its row reports, which is what the
        // user watching it would have had on screen.
        let _ = directory.apply(&mut focused_chat, durable(OTHER, 10, "watched"));
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 10), row(OTHER, false, 10)]),
        );
        directory.mark_viewed(OTHER);
        assert!(
            !unseen(&directory, OTHER),
            "nothing has happened since the user looked",
        );

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 10), row(OTHER, false, 20)]),
        );
        assert!(
            unseen(&directory, OTHER),
            "the session moved on after the user looked away",
        );

        // Folding that output and looking again clears it.
        let _ = directory.apply(&mut focused_chat, durable(OTHER, 20, "caught up on"));
        directory.mark_viewed(OTHER);
        assert!(!unseen(&directory, OTHER));
    }

    /// A working session's glyph is that it is working, and the focused
    /// session is by definition being looked at, so neither carries the
    /// unseen mark. A session never viewed does not either, or connecting
    /// would light up every row in the store.
    #[test]
    fn working_focused_and_never_viewed_sessions_are_not_unseen() {
        let (mut directory, mut focused_chat) = two_sessions();

        // Away and back, which records a position for both. Without one, the
        // never-viewed arm would answer for them and the two rules below would
        // never be reached.
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![
                row(FOCUSED, false, 99),
                row(OTHER, true, 99),
                // Idle, moved on, and never viewed: it reaches the
                // never-viewed arm rather than being turned away for working
                // or for being the focused one.
                row("session-never-opened", false, 99),
            ]),
        );
        assert!(
            !unseen(&directory, OTHER),
            "a working session reports working, not unseen",
        );
        assert!(
            !unseen(&directory, FOCUSED),
            "the focused session is being looked at right now",
        );
        assert!(
            !unseen(&directory, "session-never-opened"),
            "a row the user never opened is not unseen output",
        );
    }

    /// Arming follows the peer's answer, not our request. A session the peer
    /// did not serve must stay unarmed, or its fold waits for a block that
    /// never comes and stops advancing its cursor.
    #[test]
    fn arming_covers_the_set_the_peer_served_and_no_more() {
        let (mut directory, mut focused_chat) = two_sessions();
        let cursor = |directory: &SessionDirectory, session: &str| {
            directory
                .client_for(session)
                .expect("attached")
                .cursor()
                .map(|cursor| cursor.seq)
        };
        // Both sessions are past their first block and folding live frames.
        let _ = directory.apply(&mut focused_chat, durable(FOCUSED, 1, "foreground"));
        let _ = directory.apply(&mut focused_chat, durable(OTHER, 1, "background"));

        // A reopened stream that served only the focused session.
        directory.expect_attach(|session| session == FOCUSED);

        // The armed fold is in the block phase, so it honours the block's
        // `caught_up` and takes its high-water mark.
        let _ = directory.apply(&mut focused_chat, state(FOCUSED));
        let _ = directory.apply(&mut focused_chat, caught_up(FOCUSED, 9));
        assert_eq!(
            cursor(&directory, FOCUSED),
            Some(9),
            "the armed session took the block's high-water mark",
        );

        // The session left out was not armed, so the same shape of frames
        // folds as live traffic and its `caught_up` is ignored. An arm that
        // covered the whole set regardless of what the peer served would move
        // this cursor.
        let before = cursor(&directory, OTHER);
        let _ = directory.apply(&mut focused_chat, state(OTHER));
        let _ = directory.apply(&mut focused_chat, caught_up(OTHER, 5));
        assert_eq!(
            cursor(&directory, OTHER),
            before,
            "an unarmed session must not take a block's high-water mark",
        );

        // Arming the rest of the set then covers the one left out.
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, state(OTHER));
        let _ = directory.apply(&mut focused_chat, caught_up(OTHER, 5));
        assert_eq!(cursor(&directory, OTHER), Some(5));
    }

    /// Visiting past the bound detaches exactly the least recently focused
    /// session, and never the focused one (spec section 5). Browsing must not
    /// leave a live driver and a held lock behind per session visited.
    #[test]
    fn visiting_past_the_bound_drops_the_least_recently_focused() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();

        // Fill the set exactly. Nothing is displaced on the way.
        for n in 1..WORKING_SET {
            let displaced = directory.focus(&mut focused_chat, &format!("session-{n}"), chat);
            assert_eq!(displaced, None, "the set had room at {n}");
        }
        for n in 0..WORKING_SET {
            assert!(directory.is_attached(&format!("session-{n}")));
        }

        // One more, so the oldest focus goes. `session-0` was focused first and
        // never again, so it is the one.
        let displaced = directory.focus(&mut focused_chat, "session-8", chat);
        assert_eq!(displaced, Some("session-0".to_string()));
        assert!(!directory.is_attached("session-0"));
        assert!(directory.is_attached("session-8"));
        assert_eq!(directory.attach_requests(None).len(), WORKING_SET);

        // Re-focusing a session in the set renews it, so the next admission
        // takes the one that has now gone longest without focus.
        directory.focus(&mut focused_chat, "session-1", || {
            panic!("still in the set")
        });
        directory.focus(&mut focused_chat, "session-8", || {
            panic!("still in the set")
        });
        let displaced = directory.focus(&mut focused_chat, "session-9", chat);
        assert_eq!(
            displaced,
            Some("session-2".to_string()),
            "renewing session-1 moved the axe onto session-2",
        );
        assert!(directory.is_attached("session-1"));
    }

    /// An archived row for a session the user has just left takes it out of
    /// the working set: leaving is when archiving takes effect, and a session
    /// still in the set is one the host cannot release.
    ///
    /// The set the reopen names has to agree with the set this keeps, so both
    /// are asserted here.
    #[test]
    fn leaving_an_archived_session_drops_it_from_the_working_set() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, "session-1", chat);
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![
                SessionSummary {
                    archived: true,
                    ..row("session-1", false, 1)
                },
                row("session-0", false, 1),
            ]),
        );
        assert!(
            directory.is_attached("session-1"),
            "archiving the session on screen detached it, so the user lost what they were reading",
        );

        // The reopen this focus asks for is what detaches it on the peer, so
        // the set that reopen names has to leave it out already. Asked before
        // the focus, which is the order the caller works in.
        let leaving = directory.attach_requests(Some("session-0"));
        let named: Vec<&str> = leaving
            .iter()
            .map(|request| request.session.as_str())
            .collect();
        assert_eq!(
            named,
            vec!["session-0"],
            "the reopen still names the archived session, so the host keeps holding it",
        );

        directory.focus(&mut focused_chat, "session-0", || {
            panic!("still in the set")
        });
        assert!(
            !directory.is_attached("session-1"),
            "the archived session is still in the working set, holding a lock the host could release",
        );
        let requests = directory.attach_requests(None);
        let named: Vec<&str> = requests
            .iter()
            .map(|request| request.session.as_str())
            .collect();
        assert_eq!(
            named,
            vec!["session-0"],
            "a later re-attach picked the archived session back up",
        );
    }

    /// Focusing an archived session attaches it like any other. Archiving hides
    /// a row, it does not close a door: the reveal is there so a session can be
    /// opened again, and one that attached nothing would paint a transcript no
    /// stream feeds.
    #[test]
    fn focusing_an_archived_session_attaches_it() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![
                SessionSummary {
                    archived: true,
                    ..row("session-1", false, 1)
                },
                row("session-0", false, 1),
            ]),
        );

        let requests = directory.attach_requests(Some("session-1"));
        let named: Vec<&str> = requests
            .iter()
            .map(|request| request.session.as_str())
            .collect();
        assert_eq!(
            named[0], "session-1",
            "the session being focused was passed over for being archived: {named:?}",
        );
        directory.focus(&mut focused_chat, "session-1", chat);
        assert!(
            directory.is_attached("session-1"),
            "and the focus itself dropped it",
        );
    }

    /// The rows are the only source for the bit, so a session the peer has not
    /// published stays in the set and stays named on the stream. A client that
    /// dropped what it could not account for would detach the whole set on its
    /// first frame.
    ///
    /// Both readers of the rule are asked, because they are what a wrong answer
    /// would reach: the set the peer is told to serve, and the set this keeps.
    #[test]
    fn a_session_with_no_row_is_not_treated_as_archived() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, "session-1", chat);
        // One session has a row, the other has never been published. Neither is
        // archived, so the two must be treated alike.
        let _ = directory.apply(&mut focused_chat, list(vec![row("session-1", false, 1)]));

        let requests = directory.attach_requests(Some("session-0"));
        let named: Vec<&str> = requests
            .iter()
            .map(|request| request.session.as_str())
            .collect();
        assert_eq!(
            named,
            vec!["session-0", "session-1"],
            "a session the peer has published no row for was left off the stream",
        );
        assert!(
            !directory.would_retire("session-0"),
            "leaving would drop a session nothing says is archived",
        );

        directory.focus(&mut focused_chat, "session-0", || {
            panic!("still in the set")
        });
        assert!(
            directory.is_attached("session-1"),
            "a session the peer has published no row for was dropped as archived",
        );
    }

    /// A session archived while it sits in the background leaves the working
    /// set as the row arrives, without waiting for the user to switch: the bit
    /// can be set from another client or over the control port, and a session
    /// left in the set is one the host cannot release.
    #[test]
    fn a_background_session_archived_elsewhere_leaves_the_set() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, "session-1", chat);
        assert!(
            directory.is_attached("session-0"),
            "the background session is not in the set, so this measures nothing",
        );

        let redraw = directory.apply(
            &mut focused_chat,
            list(vec![
                SessionSummary {
                    archived: true,
                    ..row("session-0", false, 1)
                },
                row("session-1", false, 1),
            ]),
        );
        assert!(
            !directory.is_attached("session-0"),
            "the archived background session is still in the working set",
        );
        assert!(redraw.0, "the strip is not redrawn, so the row stays on it");
        assert!(
            directory.is_attached("session-1"),
            "the session on screen went with it",
        );
    }

    /// A re-attach of the set as it stands keeps the session the user is on,
    /// archived or not. It is the exemption the focused session has everywhere
    /// else, and losing it here would detach the very session on screen.
    #[test]
    fn a_reattach_keeps_the_archived_session_the_user_is_on() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![SessionSummary {
                archived: true,
                ..row("session-0", false, 1)
            }]),
        );

        let requests = directory.attach_requests(None);
        let named: Vec<&str> = requests
            .iter()
            .map(|request| request.session.as_str())
            .collect();
        assert_eq!(
            named,
            vec!["session-0"],
            "the re-attach dropped the session on screen for being archived",
        );
    }

    /// The rejoin rule is set-wide: every withheld session whose row returns is
    /// re-owed, not only the one on screen. A host going down refuses the whole
    /// working set, so its return has to bring the whole set back, and a rule
    /// that asked only for the focused session would strand the background ones
    /// silently: the discharge path folds no notice, so a stranded session
    /// reads exactly like a quiet one.
    #[test]
    fn a_returning_row_re_owes_every_withheld_session() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, "session-1", chat);
        let both = || vec![row("session-0", false, 0), row("session-1", false, 0)];
        let _ = directory.apply(&mut focused_chat, list(both()));
        for session in ["session-0", "session-1"] {
            let _ = directory.apply(&mut focused_chat, refusal(session, "unknown_session"));
        }
        // The premise: both sessions are withheld and neither owes a re-attach,
        // without which the return below re-owes nothing this test can see.
        for session in ["session-0", "session-1"] {
            let client = directory.client_for(session).expect("an attached client");
            assert!(client.withheld().is_some(), "{session} is not withheld");
            assert!(!client.needs_reattach(), "{session} still owes a re-attach");
        }

        let _ = directory.apply(&mut focused_chat, list(Vec::new()));
        let _ = directory.apply(&mut focused_chat, list(both()));

        // The background session first, so a rule that only re-owes the focused
        // one fails on the session it strands.
        for session in ["session-0", "session-1"] {
            assert!(
                directory
                    .client_for(session)
                    .expect("an attached client")
                    .needs_reattach(),
                "{session}'s row returned and nothing asks for it again",
            );
        }
    }

    #[test]
    fn a_persistence_failed_session_rejoins_when_its_shared_stream_stays_live() {
        let (mut directory, mut focused_chat) = two_sessions();
        let both_live = vec![row(FOCUSED, false, 1), row(OTHER, false, 1)];
        let _ = directory.apply(&mut focused_chat, list(both_live.clone()));

        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "persistence_failed"));
        assert_eq!(
            directory.client().withheld(),
            None,
            "the terminal frame left the session waiting on a coalescible directory edge",
        );
        assert!(
            directory.client().needs_reattach(),
            "another client can rematerialize before a cold row is published, so the terminal frame itself must ask for replacement",
        );
        let _ = directory.apply(&mut focused_chat, list(both_live));
        assert!(
            directory.client().needs_reattach(),
            "an unchanged live row withdrew the replacement already owed",
        );

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![cold_row(FOCUSED), row(OTHER, false, 1)]),
        );

        assert!(
            directory.client().needs_reattach(),
            "the failed materialization was released while another attachment kept the stream open, but no edge asks for its replacement",
        );
        assert!(
            directory
                .client_for(OTHER)
                .expect("the healthy sibling")
                .holds_attachment(),
            "recovering the failed session dropped its healthy sibling",
        );

        directory.expect_attach(|session| session == FOCUSED);
        assert!(
            !directory.client().needs_reattach(),
            "arming the replacement did not discharge its one re-attach obligation",
        );
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![cold_row(FOCUSED), row(OTHER, false, 1)]),
        );
        assert!(
            !directory.client().needs_reattach(),
            "the same cold snapshot re-fired an edge whose replacement is already armed",
        );
    }

    #[test]
    fn a_persistence_failed_refusal_rejoins_from_an_already_cold_row() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![cold_row(FOCUSED)]));

        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "persistence_failed"));

        assert!(
            directory.client().needs_reattach(),
            "a gateway's cold snapshot arrived before the terminal refusal, but the refusal did not ask for replacement",
        );
    }

    #[test]
    fn an_unknown_code_does_not_re_ask_when_a_row_goes_cold() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 1)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "future_storage_answer"));

        let _ = directory.apply(&mut focused_chat, list(vec![cold_row(FOCUSED)]));

        assert!(
            !directory.client().needs_reattach(),
            "an unknown refusal became an immediate retry when its row went cold",
        );
    }

    /// What a refusal costs is folded once, and the sentence names what will
    /// end it. Not the same sentence for both edges: a locked session's row does
    /// not leave the peer's list while the hold lasts, so a user told to watch
    /// for its return is watching for something that will not happen.
    ///
    /// Both directory-waiting codes share one body, and each arm asserts the
    /// other sentence is absent. Comparing only against the constant the
    /// implementation folds pins nothing about what the constant says.
    #[test]
    fn a_refusal_notice_names_the_edge_that_will_end_it() {
        for (code, expected, wrong) in [
            ("locked", WITHHELD_LOCKED_NOTICE, WITHHELD_NOTICE),
            ("unknown_session", WITHHELD_NOTICE, WITHHELD_LOCKED_NOTICE),
        ] {
            let mut directory = SessionDirectory::new(FOCUSED.to_string());
            let mut focused_chat = chat();
            let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, code));
            let folded = notices(&focused_chat);
            assert!(
                folded.iter().any(|text| text == expected),
                "the {code} refusal told the user to watch for the wrong \
                 thing: {folded:?}",
            );
            assert!(
                !folded.iter().any(|text| text == wrong),
                "the {code} refusal folded another edge's sentence too, so \
                 two say the same thing and one of them is a lie: \
                 {folded:?}",
            );
        }
        assert_eq!(
            withheld_notice(Refusal::PersistenceFailed),
            WITHHELD_PERSISTENCE_NOTICE,
        );
    }

    /// Nothing that resumes asking leaves a session marked refused. The
    /// withheld state means "refused, and nothing is asking again yet", so
    /// every path back to following has to end it, not only the row edges.
    ///
    /// Two such paths, and neither goes through the edges: a reconnect arms
    /// every session `attach_requests` names, refused ones included, and a
    /// gateway sends a `reset` per session when a host's link returns.
    #[test]
    fn nothing_that_resumes_asking_leaves_a_session_withheld() {
        type Resume = fn(&mut SessionDirectory, &mut ChatState);
        let paths: [(&str, Resume); 2] = [
            ("an arm for a reconnect's attach", |directory, _| {
                directory.expect_attach(|_| true)
            }),
            ("a reset from the peer", |directory, chat| {
                let _ = directory.apply(
                    chat,
                    Frame::Reset {
                        session: FOCUSED.to_string(),
                    },
                );
            }),
        ];
        for (what, resume) in paths {
            let mut directory = SessionDirectory::new(FOCUSED.to_string());
            let mut focused_chat = chat();
            let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
            let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
            assert_eq!(
                directory.client().withheld(),
                Some(Refusal::Locked { generation: None }),
                "the session is not withheld before {what}, so this leg \
                 measures nothing",
            );

            resume(&mut directory, &mut focused_chat);

            assert_eq!(
                directory.client().withheld(),
                None,
                "{what} left the session marked refused, so a later row \
                 transition re-asks for a session this client is already \
                 following",
            );
        }
    }

    /// The harm the rule above prevents, which for a locked refusal is not a
    /// race but a certainty.
    ///
    /// A reconnect that succeeds is this client taking the lock the rival had,
    /// and a host publishes its own live sessions unlocked. So the very attach
    /// that fixes the session manufactures the fall the edge watches for, and a
    /// stale withheld mark turns it into a redundant re-attach of the whole
    /// working set.
    #[test]
    fn a_landed_attach_is_not_re_asked_when_the_host_publishes_it_unlocked() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked { generation: None }),
            "the session is not withheld, so the reconnect below resumes nothing",
        );

        // The reconnect arms it and its block lands: this client now holds the
        // session, and the row says so.
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));

        assert!(
            !directory.client().needs_reattach(),
            "the client asked again for a session it had just attached, which \
             costs a stream reopen and a backfill for the whole working set",
        );
    }

    /// The headline of the second edge: a locked refusal re-asks when the rival
    /// lets go, which the row's `locked` bit reports by going true then false.
    ///
    /// The only edge a held session offers. Its row stays on the peer's list for
    /// as long as the hold lasts, so absence never transitions and a client
    /// waiting on that one alone waits forever (spec 6.5, 6.8).
    #[test]
    fn a_locked_refusal_re_asks_when_the_bit_falls() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        // The premise: the refusal was read as a locked one and nothing is
        // asking, without which the fall below re-owes nothing this test can
        // see.
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked { generation: None }),
            "the refusal was not recorded as a locked one",
        );
        assert!(
            !directory.client().needs_reattach(),
            "something already owes the re-attach",
        );

        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));

        assert!(
            directory.client().needs_reattach(),
            "the rival let go and nothing asks for the session again, so a \
             locked refusal is still a dead end",
        );
    }

    /// A locked refusal keeps the absence edge besides. A held session's row can
    /// leave the list anyway (a host restart, a withdrawal) and comes back
    /// rebuilt with the bit already false, so there is no fall left to see and
    /// the return is the only transition that happened.
    #[test]
    fn a_locked_refusal_keeps_the_absence_edge() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked { generation: None }),
            "the refusal was not recorded as a locked one",
        );

        // The row leaves with the bit still set, which is what makes this the
        // absence edge and not the other one: the host that published the hold
        // went away, taking the transition the client was watching for.
        let _ = directory.apply(&mut focused_chat, list(Vec::new()));
        assert!(
            !directory.client().needs_reattach(),
            "a row leaving the list is not on its own a reason to ask again",
        );
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));

        assert!(
            directory.client().needs_reattach(),
            "the row returned rebuilt and nothing asks for the session again, \
             so a locked refusal traded one edge for the other",
        );
    }

    /// A code this build has never heard of keeps exactly the absence edge, and
    /// gains nothing from the bit. An unknown refusal has to behave like the
    /// refusals this build knows rather than like the most specific one, which
    /// is spec 6.6's additive codes applied to rejoining.
    #[test]
    fn an_unknown_code_does_not_re_ask_when_the_bit_falls() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "kettle_overheated"));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Other),
            "an unknown code was read as something this build knows",
        );

        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));
        assert!(
            !directory.client().needs_reattach(),
            "the bit fell and an unknown refusal followed it, so an unknown \
             code takes the most specific rule instead of the general one",
        );

        // And the edge it does keep still fires, so the assertion above is
        // about the bit rather than about a refusal that stopped watching.
        let _ = directory.apply(&mut focused_chat, list(Vec::new()));
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));
        assert!(
            directory.client().needs_reattach(),
            "an unknown refusal lost the edge every refusal keeps",
        );
    }

    /// The locked edge is set-wide, exactly as the absence edge is. One rival
    /// process holds every session in a store, so one release brings back more
    /// than the session on screen, and a rule scoped to the focused one would
    /// strand the rest silently: the discharge path folds no notice.
    #[test]
    fn the_locked_edge_is_set_wide() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, OTHER, chat);
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![held_row(FOCUSED), held_row(OTHER)]),
        );
        for session in [FOCUSED, OTHER] {
            let _ = directory.apply(&mut focused_chat, refusal(session, "locked"));
            let client = directory.client_for(session).expect("an attached client");
            assert_eq!(
                client.withheld(),
                Some(Refusal::Locked { generation: None }),
                "{session} is not withheld on a locked refusal",
            );
            assert!(!client.needs_reattach(), "{session} still owes a re-attach");
        }

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, 0), row(OTHER, false, 0)]),
        );

        // The background session first, so a rule that only re-owes the focused
        // one fails on the session it strands.
        for session in [FOCUSED, OTHER] {
            assert!(
                directory
                    .client_for(session)
                    .expect("an attached client")
                    .needs_reattach(),
                "{session}'s rival let go and nothing asks for it again",
            );
        }
    }

    /// A re-ask that is refused again re-enters the withheld state with its
    /// edges re-armed. The refusal is itself fresh evidence the bit is true, so
    /// a race against a release still in flight costs one refusal and keeps
    /// watching, rather than stranding the session on an edge already spent.
    #[test]
    fn a_re_refused_locked_session_re_arms_its_edge() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));
        assert!(
            directory.client().needs_reattach(),
            "the first fall did not fire, so the re-arm below measures nothing",
        );

        // The re-ask goes out and the rival still has the session. The held row
        // after the refusal is not fixture convenience: a host sets the bit on
        // the very acquire it refuses and publishes within its list debounce,
        // while the earliest clear is a probe tick away, so a refusal is
        // followed by a fresh `true` by construction. That is what re-arms this
        // edge, rather than the client assuming a refusal means the bit is set,
        // which against a peer that never publishes it would fire on every list.
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        assert!(
            !directory.client().needs_reattach(),
            "the second refusal did not withdraw the obligation, so the fall \
             below cannot be told from the discharge that never happened",
        );
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));

        assert!(
            directory.client().needs_reattach(),
            "the rival let go a second time and nothing asks for the session \
             again, so a re-refused session is stranded",
        );
    }

    /// Against a peer that never publishes the bit, the locked edge cannot fire
    /// and the refusal waits on absence alone. That is the gap an old peer
    /// always had, disclosed rather than filled with a timer: what this must
    /// never become is a client that reads every list of an unchanged row as a
    /// reason to ask again.
    #[test]
    fn a_peer_that_never_publishes_the_bit_leaves_a_locked_refusal_waiting() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        // Rows that never carry the key, which is what an older host publishes
        // and what a reader must treat as no promise of anything (spec 6.8).
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));
        let _ = directory.apply(&mut focused_chat, refusal(FOCUSED, "locked"));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked { generation: None }),
            "the session is not withheld on a locked refusal, so the lists \
             below are folded by a client that was never waiting",
        );

        for seq in 1..8 {
            let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, seq)]));
            assert!(
                !directory.client().needs_reattach(),
                "list {seq} of an unchanged hold asked again, which is the \
                 retry loop this rule exists to refuse",
            );
        }
    }

    /// The headline of the generation clause: a hold whose rise the client never
    /// received still ends visibly.
    ///
    /// `list` is lossy-coalescible by contract (spec 6.4), so the snapshot
    /// carrying the bit's rise may be superseded in the fan-out before this
    /// client drains it, and the rise and the fall are seconds apart by design
    /// (the host publishes the rise within its list debounce of its own refused
    /// acquire, and the earliest fall is a probe tick later). A client that
    /// missed the rise holds a baseline where the bit is already false, so the
    /// transition edge has nothing to fire on and the peer has published both
    /// edges correctly. Only the generation on the row makes the release legible
    /// from the latest snapshot alone (spec 6.5, 6.8).
    ///
    /// No frame in this test carries a set bit, which is the whole point: the
    /// two landed edges are inert throughout, so nothing here can pass on their
    /// behalf.
    #[test]
    fn a_coalesced_away_rise_re_asks_on_the_published_generation() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        // The baseline: the last snapshot this client received before the hold,
        // free at the generation of an earlier hold.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        // The rival takes the lock and the host refuses this client's attach,
        // minting generation 7 for that hold. The snapshot carrying `locked` at
        // 7 is the one the transport dropped, so it never appears here.
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked {
                generation: Some(7)
            }),
            "the refusal did not carry the hold it was issued over, so the row \
             below is compared against nothing",
        );
        assert!(
            !directory.client().needs_reattach(),
            "something already owes the re-attach",
        );

        // The fall, as the probe tick publishes it: the same bit the baseline
        // had, at the generation the refusal named.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));

        assert!(
            directory.client().needs_reattach(),
            "the hold the client was refused over is over, the peer said so on \
             the row, and nothing asks for the session again: a correct peer \
             strands this client for as long as the hold's rise was coalesced \
             away",
        );
    }

    /// A gateway opens with its latest merged list and only then forwards the
    /// host's refusal. The already-folded row must answer that refusal without
    /// waiting for another list that may never come.
    #[test]
    fn a_list_before_its_refusal_re_asks_immediately() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));

        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));

        assert!(
            directory.client().needs_reattach(),
            "the current row already says hold 7 is over, but a client that gets \
             no post-refusal list is stranded",
        );
        let notices = notices(&focused_chat);
        assert!(
            notices
                .iter()
                .any(|notice| notice == "this peer will not serve session-focused: locked"),
            "evaluating the current row dropped the peer's refusal: {notices:?}",
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice == WITHHELD_LOCKED_NOTICE),
            "evaluating the current row dropped the withheld warning: {notices:?}",
        );
    }

    /// A snapshot older than the refusal is inert, however far its generation is
    /// from the refusal's.
    ///
    /// This is what `>=` buys over `!=`. A client can be handed such a row: a
    /// gateway relays what it last heard for a host it cannot reach (spec 6.8),
    /// and a reconnect can land on rows a whole hold behind. Under a difference
    /// test every one of them re-asks, which is the retry loop the refusal rule
    /// exists to refuse, and it re-asks fastest exactly when the peer is least
    /// able to answer.
    #[test]
    fn a_generation_older_than_the_refusal_never_re_asks() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert!(
            !directory.client().needs_reattach(),
            "the current stale row at generation 6 fired while refusal 7 folded, \
             so the ordered comparison is reversed",
        );
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked {
                generation: Some(7)
            }),
            "the refusal did not retain the generation the rows compare against",
        );

        // Stale free rows, from before the hold that refused. Every one of them
        // reports the lock free, and none of them is evidence about hold 7.
        for generation in [6, 5, 4] {
            let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, generation)]));
            assert!(
                !directory.client().needs_reattach(),
                "a snapshot from generation {generation} re-asked about hold 7, \
                 so the comparison reads difference rather than order",
            );
        }

        // And the client is still watching, so the refusals above were not
        // ignored for some other reason.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));
        assert!(
            directory.client().needs_reattach(),
            "the refusal's own generation did not fire either, so this test \
             proved nothing about which comparison is used",
        );
    }

    /// Folding a list replaces the row's generation with the latest value. It
    /// does not keep a maximum, since generations from a restarted publisher
    /// may move backwards under the current contract.
    #[test]
    fn a_folded_row_keeps_the_latest_generation_not_the_maximum() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 9)]));
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));

        assert_eq!(
            directory.rows()[0].lock_generation,
            Some(7),
            "the fold retained a high-water mark instead of the latest row",
        );
    }

    /// One released generation re-asks once, however often the peer republishes
    /// it.
    ///
    /// The spin bound, and the reason a snapshot rule is safe where a poll is
    /// not. A conforming host advances the generation on every acquire, so a
    /// re-refusal names a later one and the comparison alone would do. This is
    /// what holds against a peer that refuses with a generation it has already
    /// published as free. Without it the fire repeats per `list` frame, which is
    /// the regression that made refusals an answer rather than a schedule.
    #[test]
    fn one_released_generation_re_asks_once() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));
        assert!(
            directory.client().needs_reattach(),
            "the first fire did not happen, so the rest of this test measures \
             nothing",
        );

        // The re-ask goes out and is refused naming the same hold, which is a
        // peer contradicting the row it just published. The generation the fire
        // read is spent, so nothing on it may fire again.
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert!(
            !directory.client().needs_reattach(),
            "the second refusal did not withdraw the obligation, so a fire \
             below cannot be told from the discharge that never happened",
        );
        for round in 1..8 {
            let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));
            assert!(
                !directory.client().needs_reattach(),
                "list {round} of an unchanged released generation asked again, \
                 which is one re-ask per frame for as long as the peer keeps \
                 publishing",
            );
        }

        // A generation the client has not fired on is news, so the bound is a
        // bound and not deafness.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 8)]));
        assert!(
            directory.client().needs_reattach(),
            "a later hold ended and the client did not ask, so consuming a \
             generation silenced the edge for good",
        );
    }

    /// A landed bit edge may ask even when the generation clause is false, but
    /// it must not consume the row's generation on that clause's behalf.
    #[test]
    fn a_landed_edge_that_fires_does_not_consume_an_unsatisfied_generation() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_at(FOCUSED, 10)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 10));

        // The bit falls, but generation 9 is stale against refusal 10. Only the
        // landed edge fires.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 9)]));
        assert!(
            directory.client().needs_reattach(),
            "the landed locked edge did not fire, so consumption is unmeasured",
        );

        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 9));
        assert!(
            directory.client().needs_reattach(),
            "the earlier landed edge consumed generation 9 even though its \
             generation clause was false",
        );
    }

    /// Consumed generations belong to the directory, not its bounded working
    /// set. Eviction, archive retirement, and a later user refocus cannot make
    /// an old row new.
    #[test]
    fn a_consumed_generation_survives_eviction_archive_and_refocus() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));
        assert!(
            directory.client().needs_reattach(),
            "generation 7 never fired, so there is nothing to preserve",
        );
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));

        for n in 1..=WORKING_SET {
            directory.focus(&mut focused_chat, &format!("session-{n}"), chat);
        }
        assert!(
            !directory.is_attached(FOCUSED),
            "the session never left the working set, so attachment state could \
             still be preserving the generation",
        );

        directory.focus(&mut focused_chat, FOCUSED, chat);
        directory.expect_attach(|session| session == FOCUSED);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert!(
            !directory.client().needs_reattach(),
            "refocusing made the current free row at consumed generation 7 fire",
        );
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 7)]));
        assert!(
            !directory.client().needs_reattach(),
            "an unchanged free row fired after eviction erased its consumption",
        );

        // Put the session in the background, then archive it through the row so
        // retirement removes the attachment by the production path.
        directory.focus(&mut focused_chat, "session-8", chat);
        let mut archived = free_at(FOCUSED, 7);
        archived.archived = true;
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![archived, row("session-8", false, 0)]),
        );
        assert!(
            !directory.is_attached(FOCUSED),
            "the archived background session was not retired, so its directory \
             state could not have been lost",
        );

        directory.focus(&mut focused_chat, FOCUSED, chat);
        directory.expect_attach(|session| session == FOCUSED);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert!(
            !directory.client().needs_reattach(),
            "archive retirement made the current row at consumed generation 7 \
             fire again",
        );
    }

    /// A peer that publishes the bit but no generation keeps exactly the landed
    /// behaviour: the transition still fires, and an unchanged row never does.
    ///
    /// The degradation pinned from the new side. A client is handed such rows on
    /// the ordinary path, because a restarted host has no lock history yet and
    /// its rows carry no generation until it sees a hold, while a refusal held
    /// over the restart still names one. Absent must therefore read as no
    /// knowledge rather than as any particular generation (spec 6.8): a client
    /// that read it as a number would either spin against every old peer or
    /// treat a silent row as evidence.
    #[test]
    fn a_peer_that_publishes_no_generation_keeps_the_landed_edges() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));
        assert_eq!(
            directory.client().withheld(),
            Some(Refusal::Locked {
                generation: Some(7)
            }),
            "the refusal names no hold, so the rows below are inert for the \
             wrong reason and this test would pass against any rule",
        );

        // Rows that carry the bit and nothing else. The hold is still on, and a
        // client that took silence for a generation would compare something.
        for seq in 1..4 {
            let _ = directory.apply(&mut focused_chat, list(vec![held_row(FOCUSED)]));
            assert!(
                !directory.client().needs_reattach(),
                "list {seq} of an unchanged hold asked again",
            );
        }
        // And a row with the bit off but still no generation: the fall, which
        // the landed edge owns, and the only thing here that may fire.
        let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, 0)]));
        assert!(
            directory.client().needs_reattach(),
            "the bit fell on a peer that publishes no generation and nothing \
             asked, so the generation clause took the fall's edge away from the \
             peers that only have that one",
        );

        // The other half of the degradation: unchanged free rows from such a
        // peer are not evidence either.
        directory.expect_attach(|_| true);
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 8));
        for seq in 1..8 {
            let _ = directory.apply(&mut focused_chat, list(vec![row(FOCUSED, false, seq)]));
            assert!(
                !directory.client().needs_reattach(),
                "list {seq} of a row that says nothing about generations was \
                 read as a release, so a silent row is evidence and this client \
                 spins against every peer that does not publish the field",
            );
        }
    }

    /// A row that still claims the hold is not a release, however new its
    /// generation is.
    ///
    /// The generation says which hold a row is about, never that it ended: the
    /// bit is the answer and the generation only makes it comparable. A rule
    /// that read the generation alone would re-ask on every snapshot of a hold
    /// that is still on, which is a refusal per `list` frame against the peer
    /// least able to serve one, and the host publishes the rise for exactly the
    /// sessions where that is true.
    #[test]
    fn a_row_that_still_claims_the_hold_is_not_a_release() {
        let mut directory = SessionDirectory::new(FOCUSED.to_string());
        let mut focused_chat = chat();
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 6)]));
        let _ = directory.apply(&mut focused_chat, refused_at(FOCUSED, 7));

        // The rise this client does receive, and then the peer keeps saying it.
        // Every one of these rows carries a generation at or beyond the
        // refusal's, so only the bit tells them from the release.
        for seq in 1..8 {
            let _ = directory.apply(&mut focused_chat, list(vec![held_at(FOCUSED, 6 + seq)]));
            assert!(
                !directory.client().needs_reattach(),
                "list {seq} of a hold that is still on re-asked, so the client \
                 reads the generation as the answer instead of the bit",
            );
        }

        // The same generation as the last held row, with the bit off: the
        // release, and the one frame here that may fire.
        let _ = directory.apply(&mut focused_chat, list(vec![free_at(FOCUSED, 13)]));
        assert!(
            directory.client().needs_reattach(),
            "the hold ended and nothing asked, so the loop above was quiet for \
             a reason other than the bit",
        );
    }

    /// The attach set a focus will leave is what the caller must attach, so the
    /// session about to be displaced is already absent from it and the session
    /// coming in is first. Naming the displaced session would keep the peer
    /// holding it, and naming the incoming one last would gate the switch's
    /// first paint behind every other session's backfill.
    #[test]
    fn the_attach_set_for_a_focus_leads_with_it_and_omits_what_it_displaces() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        for n in 1..WORKING_SET {
            directory.focus(&mut focused_chat, &format!("session-{n}"), chat);
        }

        let requests = directory.attach_requests(Some("session-8"));
        let named: Vec<&str> = requests.iter().map(|r| r.session.as_str()).collect();
        assert_eq!(named.len(), WORKING_SET);
        assert_eq!(
            named[0], "session-8",
            "the session being focused leads, so its block is served first",
        );
        assert!(
            !named.contains(&"session-0"),
            "the displaced session is unnamed, which is what detaches it: {named:?}",
        );
        assert_eq!(
            requests[0].cursor, None,
            "a session never attached offers no cursor",
        );

        // And the answer agrees with what the focus actually does.
        let displaced = directory.focus(&mut focused_chat, "session-8", chat);
        assert_eq!(displaced, Some("session-0".to_string()));
        assert_eq!(
            directory
                .attach_requests(None)
                .iter()
                .map(|r| r.session.clone())
                .collect::<Vec<_>>(),
            named.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "the set predicted for the focus is the set the focus left",
        );
    }

    /// A session already in the working set leads the attach set just the same
    /// when it is the one being admitted, and is named once. A `reset` on a
    /// background session drives exactly this: the reopen admits a session the
    /// set already holds, and the consumer waits for that session's catch-up
    /// before it paints the switch, so leading with another session gates the
    /// first paint behind an unrelated backfill.
    #[test]
    fn the_attach_set_leads_with_an_admitted_session_already_attached() {
        let (mut directory, mut focused_chat) = two_sessions();
        // A position to offer, so leading with `OTHER` cannot be confused with
        // the fresh-admission path, which offers none.
        for seq in [3, 4] {
            let _ = directory.apply(&mut focused_chat, durable(OTHER, seq, "background"));
        }
        // A third session, so "leads with the admitted one" cannot pass by
        // luck: with two entries an arbitrary order is right half the time.
        directory.focus(&mut focused_chat, "session-third", chat);
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));

        let requests = directory.attach_requests(Some(OTHER));
        let named: Vec<String> = requests.iter().map(|r| r.session.clone()).collect();
        assert_eq!(
            named,
            vec![
                OTHER.to_string(),
                FOCUSED.to_string(),
                "session-third".to_string(),
            ],
            "the admitted session leads and is named once",
        );
        assert_eq!(
            requests[0].cursor.as_ref().map(|c| c.seq),
            Some(3),
            "an admitted session already folding offers the position it reached",
        );

        // And the answer agrees, as a set, with what the focus leaves.
        let displaced = directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert_eq!(displaced, None, "an attached session displaces nothing");
        let mut left: Vec<String> = directory
            .attach_requests(None)
            .into_iter()
            .map(|r| r.session)
            .collect();
        let mut predicted = named;
        left.sort();
        predicted.sort();
        assert_eq!(
            left, predicted,
            "the set predicted for the focus is the set the focus left",
        );
    }

    /// A re-attach carries every session this client folds, each with its own
    /// cursor, focused first. One stream serves the whole set, so offering a
    /// single session's cursor would silently drop the rest.
    #[test]
    fn a_reattach_offers_every_session_its_own_cursor() {
        let (mut directory, mut focused_chat) = two_sessions();
        // Two per session: the offered cursor lags the applied high-water
        // mark by one durable frame, so the second is what pins the first.
        for seq in [7, 8] {
            let _ = directory.apply(&mut focused_chat, durable(FOCUSED, seq, "foreground"));
        }
        for seq in [3, 4] {
            let _ = directory.apply(&mut focused_chat, durable(OTHER, seq, "background"));
        }

        // A third session, so "focused first" cannot pass by luck: with two
        // entries an arbitrary order puts the right one first half the time.
        directory.focus(&mut focused_chat, "session-third", chat);
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));

        let requests = directory.attach_requests(None);
        assert_eq!(
            requests
                .iter()
                .map(|r| r.session.as_str())
                .collect::<Vec<_>>(),
            vec![FOCUSED, "session-third", OTHER],
            "focused first, then most recently focused",
        );
        assert_eq!(requests[0].cursor.as_ref().map(|c| c.seq), Some(7));
        assert_eq!(
            requests[2].cursor.as_ref().map(|c| c.seq),
            Some(3),
            "the background session offers its own position, not the focused one's",
        );
    }

    /// A narrowed re-attach leaves the peer holding one session, so the rest
    /// have to leave the working set: a later focus onto one of them must take
    /// the attach path rather than swapping onto a transcript nothing feeds.
    /// Their rows and their viewed positions both survive, because a detached
    /// session is still one the user is meant to be aware of.
    #[test]
    fn dropping_all_but_one_detaches_the_rest_and_keeps_what_they_owe() {
        let (mut directory, mut focused_chat) = two_sessions();
        directory.focus(&mut focused_chat, "session-third", chat);
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![
                row(FOCUSED, false, 0),
                row(OTHER, false, 7),
                row("session-third", false, 0),
            ]),
        );
        assert!(
            unseen(&directory, OTHER),
            "`OTHER` moved on after the user left it",
        );

        let dropped = directory.drop_all_but(FOCUSED);

        assert_eq!(
            dropped,
            vec!["session-third".to_string(), OTHER.to_string()],
            "dropped in the order they were held, most recently focused first",
        );
        assert!(!directory.is_attached(OTHER));
        assert!(!directory.is_attached("session-third"));
        assert!(directory.is_attached(FOCUSED));
        transcripts_are_on_loan_once(&directory);
        assert_eq!(
            directory.rows().len(),
            3,
            "a detached session is still one the peer lists",
        );
        assert!(
            unseen(&directory, OTHER),
            "being detached does not make what happened while away seen",
        );

        // A later focus onto a dropped session takes the attach path.
        let mut minted = false;
        directory.focus(&mut focused_chat, OTHER, || {
            minted = true;
            chat()
        });
        assert!(minted, "the dropped session is attached afresh");
        transcripts_are_on_loan_once(&directory);
    }

    /// The focused session survives a drop that does not name it. Its
    /// transcript is the one on loan to the frontend, and this type cannot
    /// repoint that cell, so dropping it would leave the frontend rendering a
    /// session nothing folds.
    #[test]
    fn dropping_all_but_a_background_session_spares_the_focused_one() {
        let (mut directory, mut focused_chat) = two_sessions();
        directory.focus(&mut focused_chat, "session-third", chat);
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));

        let dropped = directory.drop_all_but(OTHER);

        assert_eq!(dropped, vec!["session-third".to_string()]);
        assert!(directory.is_attached(FOCUSED));
        assert!(directory.is_attached(OTHER));
        transcripts_are_on_loan_once(&directory);
    }
}
