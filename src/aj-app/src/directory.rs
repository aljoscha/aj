//! The client's view of every session a peer offers (spec 6.8, 9.2).
//!
//! One [`SessionDirectory`] holds two very different kinds of knowledge, and
//! keeping them apart is the point of the type:
//!
//! - A row per session the peer reports, from the `list` frames. This is all
//!   a client knows about a session it has never opened, and it is where the
//!   unseen-output glyph comes from.
//! - A [`SessionClient`] plus a transcript per session the client has
//!   attached. Live frames keep arriving for these while they sit in the
//!   background, which is what makes switching to one a view swap rather
//!   than a rebuild.
//!
//! The focused session's transcript is **not** stored here. A frontend holds
//! it behind widgets that cannot be repointed, so it lives in the frontend's
//! own cell and the directory borrows it for the duration of a call. Focusing
//! another session swaps the two. Every entry point that can touch the
//! focused session therefore takes `focused_chat`, and the invariant is that
//! exactly the focused session's stored transcript is `None`.
//!
//! Attaching is not this type's job: it owns no stream and does no IO. The
//! caller opens the stream and arms the client (see
//! [`SessionClient::expect_attach`]); the directory only records that the
//! session is one it now folds frames for.

use std::collections::HashMap;

use aj_wire::{Cursor, Frame, SessionSummary};
use chrono::{DateTime, Utc};

use crate::chat::{ChatState, Redraw};
use crate::client::SessionClient;

/// One session the client folds frames for.
struct Attached {
    client: SessionClient,
    /// The transcript while this session sits in the background. `None` for
    /// the focused session, whose transcript the frontend holds.
    chat: Option<ChatState>,
}

/// Every session a peer offers, plus the fold state for the ones this client
/// has attached.
pub struct SessionDirectory {
    focused: String,
    sessions: HashMap<String, Attached>,
    /// The last `list` frame's rows, in the order the peer sent them.
    rows: Vec<SessionSummary>,
    /// Each session's activity stamp as of when the user last looked at it,
    /// on the host's clock. Compared against the row's current stamp to
    /// derive unseen output, so this client never consults its own clock
    /// (spec 6.8).
    viewed: HashMap<String, DateTime<Utc>>,
}

impl SessionDirectory {
    /// A directory focused on `session`, whose transcript the caller holds.
    ///
    /// The session counts as attached from the start: a frontend reaches this
    /// type having already opened its first stream.
    pub fn new(session: String) -> Self {
        let mut sessions = HashMap::new();
        sessions.insert(
            session.clone(),
            Attached {
                client: SessionClient::new(session.clone()),
                chat: None,
            },
        );
        Self {
            focused: session,
            sessions,
            rows: Vec::new(),
            viewed: HashMap::new(),
        }
    }

    /// The session the frontend is rendering.
    pub fn focused(&self) -> &str {
        &self.focused
    }

    /// The focused session's fold state.
    pub fn client(&self) -> &SessionClient {
        self.client_for(&self.focused)
            .expect("the focused session is attached")
    }

    /// The focused session's fold state, mutably.
    pub fn client_mut(&mut self) -> &mut SessionClient {
        let focused = self.focused.clone();
        self.sessions
            .get_mut(&focused)
            .map(|attached| &mut attached.client)
            .expect("the focused session is attached")
    }

    /// One session's fold state, `None` for a session this client has not
    /// attached.
    pub fn client_for(&self, session: &str) -> Option<&SessionClient> {
        self.sessions.get(session).map(|attached| &attached.client)
    }

    /// Whether this client folds frames for `session`.
    pub fn is_attached(&self, session: &str) -> bool {
        self.sessions.contains_key(session)
    }

    /// The rows from the last `list` frame.
    pub fn rows(&self) -> &[SessionSummary] {
        &self.rows
    }

    /// Fold one frame.
    ///
    /// A session-scoped frame goes to its own session's client, writing into
    /// that session's transcript, or into `focused_chat` when the frame
    /// belongs to the focused session. A frame for a session this client has
    /// not attached is dropped: the host only sends those for sessions the
    /// stream named, so one arriving here is a peer bug, and folding it would
    /// need a transcript no gesture has asked for.
    pub fn apply(&mut self, focused_chat: &mut ChatState, frame: Frame) -> Redraw {
        let Some(session) = frame.session() else {
            return self.apply_host_frame(frame);
        };
        let focused = session == self.focused;
        let Some(attached) = self.sessions.get_mut(session) else {
            return Redraw(false);
        };
        let redraw = match &mut attached.chat {
            Some(chat) => attached.client.apply(chat, frame),
            None => attached.client.apply(focused_chat, frame),
        };
        // Only the focused session's transcript is on screen, so a background
        // session's fold changes nothing a redraw would show. Its row can
        // still change, but that arrives as a `list` frame of its own.
        Redraw(redraw.0 && focused)
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
            // `vms` belongs to whatever renders VM state and `heartbeat`
            // exists to keep the connection warm, so neither is the
            // directory's to hold.
            _ => Redraw(false),
        }
    }

    /// Move focus to `session`, swapping its transcript into `focused_chat`.
    ///
    /// `mint` builds the transcript for a session focused for the first time,
    /// which also records it as attached. It is only called in that case, so
    /// a caller can put whatever a fresh transcript costs behind it.
    ///
    /// Focusing the already-focused session leaves everything alone rather
    /// than cycling its transcript out and back.
    pub fn focus(
        &mut self,
        focused_chat: &mut ChatState,
        session: &str,
        mint: impl FnOnce() -> ChatState,
    ) {
        if session == self.focused {
            return;
        }
        // Take the incoming transcript before parking the outgoing one, so a
        // session that is not attached yet is minted while the focused
        // session's transcript is still in the frontend's cell. Parking first
        // would leave no transcript on loan if `mint` panicked.
        let incoming = match self.sessions.get_mut(session) {
            Some(attached) => attached
                .chat
                .take()
                .expect("only the focused session's transcript is on loan"),
            None => {
                let chat = mint();
                self.sessions.insert(
                    session.to_string(),
                    Attached {
                        client: SessionClient::new(session.to_string()),
                        chat: None,
                    },
                );
                chat
            }
        };
        let outgoing = std::mem::replace(focused_chat, incoming);
        let previous = std::mem::replace(&mut self.focused, session.to_string());
        self.sessions
            .get_mut(&previous)
            .expect("the focused session is attached")
            .chat = Some(outgoing);
        // Everything the session did while it was the focused one was on
        // screen, so leaving is the moment its output counts as seen.
        self.mark_viewed(&previous);
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
    /// one recorded at the last view. A working session is excluded because
    /// its glyph says it is working, which is the more useful fact, and the
    /// unseen mark is what remains once it stops (spec 6.8).
    ///
    /// The focused session is never unseen: the user is looking at it.
    pub fn has_unseen_output(&self, session: &str) -> bool {
        if session == self.focused {
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
            // Never viewed. A session the client has attached was opened at
            // some point, so its output is not news; one it has not is a row
            // the user has never asked for, and announcing it as unseen would
            // light up every session in the store on connect.
            None => false,
        }
    }

    /// Arm every session the peer reports it served, for the attach blocks a
    /// freshly opened stream carries.
    ///
    /// `served` is the peer's own answer, never the request we sent: an arm
    /// for a block that never arrives strands that session's fold, and a
    /// session the peer did attach but we left unarmed folds its block as
    /// live frames (see [`SessionClient::expect_attach`]). Arming the whole
    /// set in one call is what keeps those two failures out of reach of a
    /// caller loop that covers only some of it.
    pub fn expect_attach(&mut self, served: impl Fn(&str) -> bool) {
        for (session, attached) in self.sessions.iter_mut() {
            if served(session) {
                attached.client.expect_attach();
            }
        }
    }

    /// Point the focused entry at `session`, keeping its fold, its transcript,
    /// and whatever it owes.
    ///
    /// Staging only, for the frontend tests that need a client folding a
    /// session the peer does not have. That is what a permanently refused
    /// attach looks like from this side, and no honest gesture produces it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn rename_focused(&mut self, session: String) -> String {
        let previous = std::mem::replace(&mut self.focused, session.clone());
        let entry = self
            .sessions
            .remove(&previous)
            .expect("the focused session is attached");
        self.sessions.insert(session, entry);
        previous
    }

    /// The attach requests that re-establish every session this client folds,
    /// each offering its own cursor.
    ///
    /// One re-attach carries all of them, because a stream serves the set it
    /// was opened with (spec 6.5) and a client that lost one lost them all.
    /// The focused session comes first so its catch-up is the first block on
    /// the new stream, which is the one the user is waiting to see.
    pub fn attach_requests(&self) -> Vec<(String, Option<Cursor>)> {
        let mut requests = vec![(
            self.focused.clone(),
            self.client_for(&self.focused)
                .expect("the focused session is attached")
                .cursor(),
        )];
        requests.extend(
            self.sessions
                .iter()
                .filter(|(session, _)| *session != &self.focused)
                .map(|(session, attached)| (session.clone(), attached.client.cursor())),
        );
        requests
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

        let redraw = directory.apply(&mut focused_chat, durable("session-stranger", 1, "stray"));
        assert!(!redraw.0);
        assert!(
            notices(&focused_chat).is_empty(),
            "a stray frame does not reach the focused transcript: {:?}",
            notices(&focused_chat),
        );
        assert!(!directory.is_attached("session-stranger"));
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

        directory.mark_viewed(OTHER);
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

        let requests = directory.attach_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, FOCUSED, "the focused session's block first");
        assert_eq!(requests[0].1.as_ref().map(|c| c.seq), Some(7));
        assert_eq!(requests[1].0, OTHER);
        assert_eq!(
            requests[1].1.as_ref().map(|c| c.seq),
            Some(3),
            "the background session offers its own position, not the focused one's",
        );
    }
}
