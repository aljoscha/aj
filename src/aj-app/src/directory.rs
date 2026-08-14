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
//! attention signal. The set holds no archived session but the focused one:
//! archiving says the user is done there, so leaving one is what lets the host
//! release it.
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

use aj_wire::{DirectoryHost, Frame, SessionSummary};

use crate::chat::{ChatState, Redraw};
use crate::client::SessionClient;
use crate::host::AttachRequest;

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
        let redraw = match &mut attached.chat {
            Some(chat) => attached.client.apply(chat, frame),
            None => attached.client.apply(focused_chat, frame),
        };
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
        // Only the focused session's transcript is on screen, so a background
        // session's fold changes nothing a redraw would show. Its row can still
        // change, but that arrives as a `list` frame of its own.
        Redraw(redraw.0 && index == 0)
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
                self.rows = sessions;
                self.hosts = hosts;
                self.latch_unseen();
                Redraw(changed)
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
        // it. The reopen the caller has already asked for leaves them unnamed
        // (see [`Self::attach_requests`], which passes over the same entries).
        let archived: HashSet<String> = self
            .rows
            .iter()
            .filter(|row| row.archived)
            .map(|row| row.id.clone())
            .collect();
        self.attached.retain(|attached| {
            attached.session == session || !archived.contains(&attached.session)
        });
        // NOTE: the truncation here and the one `attach_requests` applies to
        // the same admission have to agree, or the reopened stream would name a
        // session this no longer folds (or drop one it does). Both keep the
        // first `WORKING_SET` entries after the incoming session takes the
        // front, and only one session is ever admitted at a time.
        (self.attached.len() > WORKING_SET)
            .then(|| self.attached.pop().expect("longer than the bound").session)
    }

    /// Whether the peer's row for `session` says the user has put it away.
    ///
    /// Off the rows and nothing else: the bit is the peer's to publish (spec
    /// 6.8), so a client that has seen no row for a session treats it as
    /// unarchived and keeps holding it.
    fn rows_archived(&self, session: &str) -> bool {
        self.rows
            .iter()
            .find(|row| row.id == session)
            .is_some_and(|row| row.archived)
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
                .filter(|attached| {
                    attached.session == kept || !self.rows_archived(&attached.session)
                })
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
    /// published stays in the set. A client that dropped what it could not
    /// account for would detach the whole set on its first frame.
    #[test]
    fn a_session_with_no_row_is_not_treated_as_archived() {
        let mut directory = SessionDirectory::new("session-0".to_string());
        let mut focused_chat = chat();
        directory.focus(&mut focused_chat, "session-1", chat);

        directory.focus(&mut focused_chat, "session-0", || {
            panic!("still in the set")
        });
        assert!(
            directory.is_attached("session-1"),
            "a session the peer has published no row for was dropped as archived",
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
