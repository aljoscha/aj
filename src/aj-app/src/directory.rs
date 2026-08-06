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
//! attention signal.
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

use std::collections::HashMap;

use aj_wire::{Frame, SessionSummary};
use chrono::{DateTime, Utc};

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
    /// Each session's activity stamp as of when the user last looked at it, on
    /// the host's clock. Compared against the row's current stamp to derive
    /// unseen output, so this client never consults its own clock (spec 6.8).
    ///
    /// Kept for sessions dropped from the working set too: being detached does
    /// not make what happened while the user was away seen, and a row outlives
    /// its attachment.
    viewed: HashMap<String, DateTime<Utc>>,
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
            }],
            rows: Vec::new(),
            viewed: HashMap::new(),
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
        let redraw = match &mut attached.chat {
            Some(chat) => attached.client.apply(chat, frame),
            None => attached.client.apply(focused_chat, frame),
        };
        // Only the focused session's transcript is on screen, so a background
        // session's fold changes nothing a redraw would show. Its row can still
        // change, but that arrives as a `list` frame of its own.
        Redraw(redraw.0 && index == 0)
    }

    /// Fold a frame carrying no session: the directory's own rows, or a kind
    /// this type has no use for.
    fn apply_host_frame(&mut self, frame: Frame) -> Redraw {
        match frame {
            Frame::List { sessions } => {
                let changed = self.rows != sessions;
                self.rows = sessions;
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
        // NOTE: the truncation here and the one `attach_requests` applies to
        // the same admission have to agree, or the reopened stream would name a
        // session this no longer folds (or drop one it does). Both keep the
        // first `WORKING_SET` entries after the incoming session takes the
        // front, and only one session is ever admitted at a time.
        (self.attached.len() > WORKING_SET)
            .then(|| self.attached.pop().expect("longer than the bound").session)
    }

    /// Note that the user has stopped looking at `session`, so its output up to
    /// this point counts as seen.
    ///
    /// Recorded as the user leaves rather than as they arrive. A session's
    /// activity climbs while it is the focused one, and all of that was on
    /// screen, so a stamp taken on arrival would make everything the user just
    /// watched read as unseen the moment they switched away.
    ///
    /// The stamp is the one the peer last reported, never the current time:
    /// both sides of the [`Self::has_unseen_output`] comparison are host clock,
    /// so this client's own clock never enters and skew cannot make a session
    /// look either stale or fresh (spec 6.8).
    ///
    /// A session with no row yet records nothing, and reads as having no unseen
    /// output until a row arrives.
    fn mark_viewed(&mut self, session: &str) {
        if let Some(row) = self.rows.iter().find(|row| row.id == session) {
            self.viewed.insert(session.to_string(), row.last_activity);
        }
    }

    /// Whether `session` has produced output the user has not looked at.
    ///
    /// True when the session is idle and its activity stamp is newer than the
    /// one recorded at the last view. A working session is excluded because its
    /// glyph says it is working, which is the more useful fact, and the unseen
    /// mark is what remains once it stops (spec 6.8).
    ///
    /// The focused session is never unseen: the user is looking at it.
    pub fn has_unseen_output(&self, session: &str) -> bool {
        if session == self.focused() {
            return false;
        }
        let Some(row) = self.rows.iter().find(|row| row.id == session) else {
            return false;
        };
        if row.working {
            return false;
        }
        match self.viewed.get(session) {
            Some(seen) => row.last_activity > *seen,
            // Never viewed, so there is no "since I last looked" to answer
            // against and the question is vacuous. Reading it as unseen would
            // light up every row of a store on first connect (spec 6.8).
            None => false,
        }
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
    pub fn attach_requests(&self, admitting: Option<&str>) -> Vec<AttachRequest> {
        let mut requests = Vec::with_capacity(WORKING_SET);
        if let Some(session) = admitting.filter(|session| !self.is_attached(session)) {
            requests.push(AttachRequest {
                session: session.to_string(),
                cursor: None,
            });
        }
        let room = WORKING_SET - requests.len();
        requests.extend(
            self.attached
                .iter()
                .take(room)
                .map(|attached| AttachRequest {
                    session: attached.session.clone(),
                    cursor: attached.client.cursor(),
                }),
        );
        requests
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_wire::QueueCounts;

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

    fn row(id: &str, working: bool, last_activity: DateTime<Utc>) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            live: true,
            working,
            queued: QueueCounts::default(),
            tasks: 0,
            last_seq: Some(0),
            last_activity,
            unreachable: false,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("a valid timestamp")
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

        let sessions = vec![row(FOCUSED, false, at(10)), row(OTHER, true, at(20))];
        let redraw = directory.apply(
            &mut focused_chat,
            Frame::List {
                sessions: sessions.clone(),
            },
        );
        assert!(redraw.0, "the first rows are news");
        assert_eq!(directory.rows().len(), 2);

        let redraw = directory.apply(&mut focused_chat, Frame::List { sessions });
        assert!(!redraw.0, "the same rows again are not");

        // The other host-level kinds are nobody's business here.
        for frame in [Frame::Heartbeat, Frame::Vms { vms: Vec::new() }] {
            assert!(!directory.apply(&mut focused_chat, frame).0);
        }
        assert_eq!(directory.rows().len(), 2, "and they leave the rows alone");
    }

    /// Output produced while a session was the focused one is output the user
    /// watched, so switching away must not leave it marked unseen. The stamp is
    /// therefore taken as the user leaves, not as they arrive.
    #[test]
    fn what_the_user_watched_while_focused_is_not_unseen_afterwards() {
        let (mut directory, mut focused_chat) = two_sessions();
        let list = |sessions: Vec<SessionSummary>| Frame::List { sessions };
        let quiet = |at_secs: i64| {
            vec![
                row(FOCUSED, false, at(at_secs)),
                row(OTHER, false, at(at_secs)),
            ]
        };

        // Away and back, so `FOCUSED` has a recorded stamp and the never-viewed
        // rule cannot answer for it.
        let _ = directory.apply(&mut focused_chat, list(quiet(10)));
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));

        // A turn runs in `FOCUSED`, on screen the whole time.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, at(50)), row(OTHER, false, at(10))]),
        );

        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert!(
            !directory.has_unseen_output(FOCUSED),
            "the user watched that turn happen, so leaving cannot mark it unseen",
        );

        // What does count is what happens after they left.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, at(90)), row(OTHER, false, at(10))]),
        );
        assert!(
            directory.has_unseen_output(FOCUSED),
            "output after the switch is unseen",
        );
    }

    /// The rows outlive a focus change. They are the peer's directory, not the
    /// focused session's, and a sidebar has to keep listing every session while
    /// the user moves between them.
    #[test]
    fn the_rows_survive_a_focus_change() {
        let (mut directory, mut focused_chat) = two_sessions();
        let sessions = vec![row(FOCUSED, false, at(10)), row(OTHER, true, at(20))];
        let _ = directory.apply(
            &mut focused_chat,
            Frame::List {
                sessions: sessions.clone(),
            },
        );

        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        assert_eq!(directory.rows(), sessions.as_slice());
        directory.focus(&mut focused_chat, "session-fresh", chat);
        assert_eq!(
            directory.rows(),
            sessions.as_slice(),
            "attaching a session the rows do not mention leaves them alone",
        );
    }

    /// Unseen output is derived by comparing two host-clock stamps, the row's
    /// current one against the one recorded when the user last looked. This
    /// client's own clock never enters, so skew between it and the host
    /// cannot invent or hide the glyph (spec 6.8).
    #[test]
    fn unseen_output_compares_host_stamps_only() {
        let (mut directory, mut focused_chat) = two_sessions();
        let list = |sessions: Vec<SessionSummary>| Frame::List { sessions };

        // Stamps far in the past on any real clock. A comparison that reached
        // for `Utc::now()` on either side would read these as ancient.
        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, at(10)), row(OTHER, false, at(10))]),
        );
        directory.mark_viewed(OTHER);
        assert!(
            !directory.has_unseen_output(OTHER),
            "nothing has happened since the user looked",
        );

        let _ = directory.apply(
            &mut focused_chat,
            list(vec![row(FOCUSED, false, at(10)), row(OTHER, false, at(20))]),
        );
        assert!(
            directory.has_unseen_output(OTHER),
            "the session moved on after the user looked away",
        );

        // Looking again clears it, at the stamp the host reported.
        directory.mark_viewed(OTHER);
        assert!(!directory.has_unseen_output(OTHER));
    }

    /// A working session's glyph is that it is working, and the focused
    /// session is by definition being looked at, so neither carries the
    /// unseen mark. A session never viewed does not either, or connecting
    /// would light up every row in the store.
    #[test]
    fn working_focused_and_never_viewed_sessions_are_not_unseen() {
        let (mut directory, mut focused_chat) = two_sessions();
        let _ = directory.apply(
            &mut focused_chat,
            Frame::List {
                sessions: vec![row(FOCUSED, false, at(10)), row(OTHER, true, at(10))],
            },
        );

        // Away and back, which records a stamp for both. Without one, the
        // never-viewed arm would answer for them and the two rules below would
        // never be reached.
        directory.focus(&mut focused_chat, OTHER, || panic!("already attached"));
        directory.focus(&mut focused_chat, FOCUSED, || panic!("already attached"));
        let _ = directory.apply(
            &mut focused_chat,
            Frame::List {
                sessions: vec![
                    row(FOCUSED, false, at(99)),
                    row(OTHER, true, at(99)),
                    // Idle, moving, and never viewed: it reaches the
                    // never-viewed arm rather than being turned away for
                    // having no row at all.
                    row("session-never-opened", false, at(99)),
                ],
            },
        );
        assert!(
            !directory.has_unseen_output(OTHER),
            "a working session reports working, not unseen",
        );
        assert!(
            !directory.has_unseen_output(FOCUSED),
            "the focused session is being looked at right now",
        );
        assert!(
            !directory.has_unseen_output("session-never-opened"),
            "a row the user never opened is not unseen output",
        );
        assert!(
            !directory.has_unseen_output("session-with-no-row"),
            "and neither is a session with no row yet",
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
}
