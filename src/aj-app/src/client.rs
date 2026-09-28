//! Client-side fold of one session's frame stream: attach blocks, epoch
//! adoption, cursor bookkeeping, and live application.
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
//! Host-level frames are deliberately not this type's business. `list`
//! and `heartbeat` carry no `session` field, so they belong to
//! whatever owns the session directory and the connection, not to one
//! session's fold. Unknown frame kinds never arrive here at all:
//! `aj-wire` decodes them into `DecodedFrame::Unknown`, which an endpoint
//! client discards (only a gateway forwards them).
//!
//! Nothing in the fold can fail. A frame is either applied or dropped, so
//! no operation here returns a `Result`.

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_wire::{AgentQueue, Cursor, DecodedAgentEvent, Frame};

use crate::chat::{ChatState, Redraw, reduce};
use crate::host::PERSISTENCE_FAILED_CODE;
use crate::session::AgentLifecycle;

/// The `error` code a gateway answers for a session whose host it cannot
/// reach. Transient: the host's return is announced with `reset`.
pub const HOST_UNREACHABLE_CODE: &str = "host_unreachable";

/// Why a session is withheld and what can ask for it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A rival writer holds the session. Only an explicit user retry reopens it.
    Locked,
    /// The gateway cannot reach the session's host. The `reset` it sends when
    /// the host returns asks again, and the attachment is kept for that.
    Unreachable,
    /// Other codes, including unknown ones, recover when the row leaves and returns.
    Other,
}

impl Refusal {
    /// Classify a peer refusal. Unknown codes use directory-return recovery.
    pub fn from_code(code: &str) -> Self {
        match code {
            "locked" => Self::Locked,
            HOST_UNREACHABLE_CODE => Self::Unreachable,
            _ => Self::Other,
        }
    }
}

/// Where the client stands relative to an attach block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    /// Outside any block. Every frame is a live one.
    ///
    /// Where a block ends, whether its `caught_up` committed it or a refusal
    /// answered instead, and where a client starts. So this says nothing on its
    /// own about whether the client holds an attachment.
    Live,
    /// Inside a block, from the `state` frame that opens it. Durable frames
    /// apply without advancing the cursor, because the block is atomic: its
    /// `caught_up` commits it.
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
    /// first one arrives and again after a refusal that drops the attachment.
    /// Session-scoped frames from any other epoch are dropped.
    epoch: Option<String>,
    /// The seq offered on re-attach. It lags `applied` by one durable
    /// frame, because a log entry can project trailing untagged events (an
    /// assistant `UsageUpdate` or a tool-result entry's bracket) and a drop
    /// in between would otherwise make the client claim an entry it only half
    /// applied. Compaction checkpoints carry usage in their durable event.
    committed: Option<u64>,
    /// The durable high-water mark the cursor invariant compares against.
    applied: Option<u64>,
    attach: Attach,
    settings: Option<AgentSettings>,
    oracle_settings: Option<AgentSettings>,
    working: bool,
    first_attach_settings: Option<AgentSettings>,
    /// The host-side credential warning carried by the first attach's opening
    /// state, for a frontend to fold once after Caught. A reconnect may carry a
    /// newer answer, but repeating startup warnings on every reconnect is more
    /// noise than help; explicit account actions report their own outcome.
    first_attach_credential_warning: Option<String>,
    saw_first_attach: bool,
    needs_reattach: bool,
    /// A refusal cleared the epoch while leaving last-known chat visible. The
    /// next block replaces that cache and restores this exact local error ahead
    /// of the new authoritative backfill.
    refusal_error: Option<String>,
    /// Refused, and nothing is asking again yet, carrying the reason so the
    /// directory knows which edge re-asks.
    ///
    /// Set by the `error` arm of [`Self::apply`], which is the one place that
    /// learns a refusal happened, and cleared by [`Self::owe_reattach`] and
    /// [`Self::attach_requested`]. Held here rather than derived from the epoch
    /// because a refusal after an earlier one moves nothing else: the client is
    /// already following nothing, so there is no transition left to read, and a
    /// session refused twice would look attached to anything watching for one.
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
            oracle_settings: None,
            working: false,
            first_attach_settings: None,
            first_attach_credential_warning: None,
            saw_first_attach: false,
            needs_reattach: false,
            refusal_error: None,
            withheld: None,
        }
    }

    /// The session these frames belong to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Record that a stream naming this session was opened.
    ///
    /// Satisfies [`Self::needs_reattach`] and ends a withheld state, since this
    /// is something asking again, which is the one condition [`Self::withheld`]
    /// tracks. It changes nothing about how frames fold: the peer answers with
    /// a block whose opening `state` says it is one, or with an `error`.
    pub fn attach_requested(&mut self) {
        self.needs_reattach = false;
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
                    // frame. The block is atomic and its
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
                // an event this client will never understand.
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
                reduce(chat, &mut self.lifecycle, event, durability.as_ref())
            }
            Frame::State {
                session,
                epoch,
                opens_block,
                working,
                settings,
                oracle_settings,
                goal,
                credential_warning,
            } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                // Only the frame says whether it opens a block. The host sends
                // `state` on every change as well, and one of those must
                // neither adopt an epoch nor quiesce.
                if opens_block {
                    self.open_attach_block(chat, epoch);
                } else if !self.epoch_matches(&epoch) {
                    return Redraw(false);
                }
                if opens_block && !self.saw_first_attach {
                    self.first_attach_settings = Some(settings.clone());
                    self.first_attach_credential_warning = credential_warning.clone();
                }
                // The host is authoritative for all of these, at every
                // emission: neither is derivable from projected events.
                chat.footers_mut()
                    .note_settings(AgentId::Main, settings.clone());
                self.seed_lifecycle(working);
                self.settings = Some(settings);
                self.oracle_settings = oracle_settings;
                chat.note_goal(goal, working);
                self.working = working;
                Redraw(true)
            }
            Frame::CaughtUp {
                session,
                epoch,
                last_seq,
                tasks,
                queues,
            } => {
                // Only an opened block ends here. A `caught_up` outside one
                // names a position whose entries the client never applied, and
                // committing it would silently skip them.
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
                // The first attach's presentation belongs to the first block
                // that commits, not the first opening State observed. A block
                // interrupted after State is replaced by the retry's answer,
                // so its settings and credential warning cannot go stale.
                self.saw_first_attach = true;
                // Neither task events nor queue updates are replayable, so the
                // block's own tables replace whatever the client held. An event
                // racing the host's snapshot arrives after this and re-applies
                // idempotently.
                chat.replace_tasks(tasks);
                chat.replace_queue(queues);
                Redraw(true)
            }
            Frame::Error {
                session,
                code,
                message,
                ..
            } => {
                if !self.is_ours(&session) {
                    return Redraw(false);
                }
                // Not filtered by epoch, the way `reset` is not: the epoch
                // filter applies to frames that carry state under one, and
                // a refusal for a session the server cannot resolve names none.
                //
                // An error answers in place of a block, so no block is open
                // after it whatever the code.
                self.attach = Attach::Live;
                let refusal = Refusal::from_code(&code);
                let was_attached = self.holds_attachment();
                if refusal == Refusal::Unreachable {
                    // Nothing was said about the session, only that its host
                    // is out of reach for now, so the epoch and the cursor
                    // stay: the host's return then costs an incremental
                    // catch-up rather than a full backfill. The gateway's
                    // `reset` for that return is what asks again.
                    self.needs_reattach = false;
                    self.withheld = Some(refusal);
                } else {
                    // Every other code drops the attachment, a code this build
                    // has never heard of included, codes being additive. The
                    // code decides only which edge asks again: a refused client
                    // is following nothing either way.
                    self.drop_attachment(refusal, message.clone());
                }
                // A followed session whose log failed is asked for once more
                // straight away: the host rebuilds it from disk, so the user
                // sees what was saved without waiting for a directory edge the
                // row may never show. A re-ask answered the same way arrives
                // with no attachment held and settles like any refusal, which
                // keeps a broken disk from turning into a retry loop.
                if code == PERSISTENCE_FAILED_CODE && was_attached {
                    self.owe_reattach();
                }
                // The message verbatim: an error's message is always a
                // sufficient human sentence on its own, so a code this build
                // has never heard of still reads.
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
                // A block cut short stays open: its cursor never advanced, and
                // the next block's opening replaces it.
                //
                // A link reset does not retry an attachment the user must
                // explicitly request. Other sessions still recover normally.
                if self.withheld == Some(Refusal::Locked) {
                    return Redraw(false);
                }
                self.owe_reattach();
                Redraw(true)
            }
            Frame::List { .. } | Frame::Heartbeat => Redraw(false),
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

    /// Whether the attach block arriving is rebuilding the projection from
    /// nothing rather than extending a committed one.
    ///
    /// A block into the epoch already applied re-projects only the suffix past
    /// the cursor, so the transcript on screen stays and grows. A first attach,
    /// a new epoch (which an accepted Head mints), and the block after a refusal
    /// all reset the chat at their opening `state` and replay the whole history, and until
    /// their `caught_up` commits, the projection is partial. A view that paints
    /// between frames reads this to keep a partial history off the screen.
    pub fn rebuilding(&self) -> bool {
        self.attach == Attach::Applying && self.cursor().is_none()
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

    /// Oracle settings staged for the next main turn, as of the last `state` frame.
    /// Absent when unresolved or when the host has no Oracle support.
    pub fn oracle_settings(&self) -> Option<&AgentSettings> {
        self.oracle_settings.as_ref()
    }

    /// Takes the settings carried by the first attach state exactly once.
    ///
    /// A frontend that resumed an existing session can render its local
    /// restored-settings summary from this without the host publishing a
    /// notice that would repeat on every reconnect.
    pub fn take_first_attach_settings(&mut self) -> Option<AgentSettings> {
        self.first_attach_settings.take()
    }

    /// Takes the host-side credential warning carried by the first attach
    /// state exactly once. `None` means the host reported no problem or was an
    /// older build that did not carry the observation.
    pub fn take_first_attach_credential_warning(&mut self) -> Option<String> {
        self.first_attach_credential_warning.take()
    }

    /// Whether the host reported a turn in flight, as of the last `state`
    /// frame. The lifecycle sets are the authority for spinners, this is
    /// the host's own flag.
    pub fn working(&self) -> bool {
        self.working
    }

    /// Whether continuity was broken and the caller owes a re-attach.
    pub fn needs_reattach(&self) -> bool {
        self.needs_reattach
    }

    /// Where this client stands on an attach block: inside one from its opening
    /// `state` until the `caught_up` that commits it or a refusal.
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
    /// recovery path that intends to ask again ends here. Settled refusals
    /// deliberately leave that obligation withdrawn.
    pub fn owe_reattach(&mut self) {
        self.needs_reattach = true;
        self.withheld = None;
    }

    /// Drop the attachment for a session the server refused.
    ///
    /// Everything the fold holds about the session comes from an attach block
    /// that is not coming: the epoch it applied under and the cursor it would
    /// offer describe a history the server says it cannot resolve, so keeping
    /// either would have the client ask for one nobody has.
    ///
    /// A locked refusal waits for an explicit retry. Other refusals wait for
    /// directory-return evidence. Neither immediately retries the failed attach.
    fn drop_attachment(&mut self, refusal: Refusal, message: String) {
        self.epoch = None;
        self.committed = None;
        self.applied = None;
        self.refusal_error = Some(message);
        self.needs_reattach = false;
        self.withheld = Some(refusal);
    }

    /// Adopt the epoch of the attach block a `state` frame opens, and prepare
    /// `chat` for the backfill that follows.
    fn open_attach_block(&mut self, chat: &mut ChatState, epoch: String) {
        if let Some(error) = self.refusal_error.take() {
            // Refusal deliberately keeps the last-known transcript visible but
            // clears its epoch. The next full block can name another branch, so
            // first-attach append semantics would merge two authoritative
            // histories. Replace the cache before applying the new block.
            chat.reset(&mut self.lifecycle);
            self.committed = None;
            self.applied = None;
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
    /// Scoped to `Main`, because `working` says nothing about sub-agents.
    /// Clearing their marks here would undercount the running
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

    use aj_agent::events::CompactionReason;
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::{TaskKind, TaskStatus, ToolDetails};
    use aj_models::streaming::AssistantMessageEvent;
    use aj_models::types::{
        AssistantContent, AssistantMessage as WireAssistantMessage, Message, StopReason,
        TextContent, Usage, UserMessage,
    };
    use aj_wire::{QueueState, TaskSummary, TaskTable};
    use chrono::Utc;

    use crate::chat::{EntryKind, NoticeLevel};

    const SESSION: &str = "session-1";
    const EPOCH: &str = "epoch-1";

    fn settings() -> AgentSettings {
        AgentSettings {
            context_window: 0,
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            thinking_display: "default".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    fn chat() -> ChatState {
        ChatState::new(aj_agent::events::AgentSettings {
            context_window: 200_000,
            ..settings()
        })
    }

    /// A client that has attached an empty session: the opening `state`,
    /// an empty backfill, and `caught_up` at seq 0. Every remote client
    /// starts here, so the unit tests do too.
    fn attached() -> (SessionClient, ChatState) {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        let _ = client.apply(&mut chat, opening(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        (client, chat)
    }

    /// An on-change `state`, as a host sends whenever `working` or the
    /// settings move.
    fn state(epoch: &str, working: bool) -> Frame {
        state_with(epoch, working, settings())
    }

    fn state_with(epoch: &str, working: bool, settings: AgentSettings) -> Frame {
        state_frame(epoch, false, working, settings, None)
    }

    /// The `state` an attach block opens with.
    fn opening(epoch: &str, working: bool) -> Frame {
        opening_with(epoch, working, settings())
    }

    fn opening_with(epoch: &str, working: bool, settings: AgentSettings) -> Frame {
        opening_with_warning(epoch, working, settings, None)
    }

    fn opening_with_warning(
        epoch: &str,
        working: bool,
        settings: AgentSettings,
        credential_warning: Option<&str>,
    ) -> Frame {
        state_frame(epoch, true, working, settings, credential_warning)
    }

    fn state_frame(
        epoch: &str,
        opens_block: bool,
        working: bool,
        settings: AgentSettings,
        credential_warning: Option<&str>,
    ) -> Frame {
        Frame::State {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            opens_block,
            working,
            settings,
            oracle_settings: None,
            goal: None,
            credential_warning: credential_warning.map(str::to_string),
        }
    }

    fn caught_up(epoch: &str, last_seq: u64) -> Frame {
        caught_up_with(epoch, last_seq, TaskTable::default(), QueueState::default())
    }

    fn caught_up_with(epoch: &str, last_seq: u64, tasks: TaskTable, queues: QueueState) -> Frame {
        Frame::CaughtUp {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            last_seq,
            tasks,
            queues,
        }
    }

    #[test]
    fn goal_snapshots_replace_clear_and_ignore_other_sessions() {
        use aj_agent::goal::{Goal, GoalStatus};
        let mut client = SessionClient::new(SESSION.into());
        let mut chat = chat();
        let goal = Goal {
            id: "g".into(),
            objective: "finish".into(),
            status: GoalStatus::Paused,
            token_budget: Some(100),
            tokens_used: 20,
            time_used_seconds: 0,
        };
        let mut snapshot = opening(EPOCH, false);
        if let Frame::State { goal: slot, .. } = &mut snapshot {
            *slot = Some(goal.clone());
        }
        let _ = client.apply(&mut chat, snapshot.clone());
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        assert_eq!(chat.goal, Some(goal));
        let mut foreign = state_with(EPOCH, false, settings());
        if let Frame::State { session, .. } = &mut foreign {
            *session = "other".into();
        }
        let _ = client.apply(&mut chat, foreign);
        assert!(chat.goal.is_some());
        let _ = client.apply(&mut chat, state_with(EPOCH, false, settings()));
        assert!(chat.goal.is_none());
        let _ = client.apply(&mut chat, snapshot);
        assert!(chat.goal.is_some());
        let _ = client.apply(&mut chat, opening("new-head", false));
        assert!(chat.goal.is_none());
    }

    #[test]
    fn goal_runtime_advances_from_host_state_without_changing_reported_usage() {
        use aj_agent::goal::{Goal, GoalStatus};
        use std::time::{Duration, Instant};
        let mut client = SessionClient::new(SESSION.into());
        let mut chat = chat();
        let state = |opens_block, status, working, seconds| {
            let mut frame = if opens_block {
                opening(EPOCH, working)
            } else {
                state_with(EPOCH, working, settings())
            };
            if let Frame::State { goal, .. } = &mut frame {
                *goal = Some(Goal {
                    id: "g".into(),
                    objective: "finish".into(),
                    status,
                    token_budget: None,
                    tokens_used: 20,
                    time_used_seconds: seconds,
                });
            }
            frame
        };
        let _ = client.apply(&mut chat, state(true, GoalStatus::Active, true, 90));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        let now = Instant::now();
        assert_eq!(
            chat.goal_runtime_seconds(now + Duration::from_secs(10))
                - chat.goal_runtime_seconds(now),
            10
        );
        assert_eq!(chat.goal.as_ref().unwrap().time_used_seconds, 90);

        for (status, working) in [
            (GoalStatus::Active, false),
            (GoalStatus::Paused, true),
            (GoalStatus::BudgetLimited, true),
            (GoalStatus::Complete, true),
        ] {
            let _ = client.apply(&mut chat, state(false, status, working, 125));
            assert_eq!(
                chat.goal_runtime_seconds(Instant::now() + Duration::from_secs(3600)),
                125
            );
        }
        let _ = client.apply(&mut chat, state(false, GoalStatus::Active, true, 5));
        let now = Instant::now();
        let reported = chat.goal_runtime_seconds(now);
        assert!(
            reported < 125,
            "a fresh subtotal replaces the previous timer"
        );
        assert_eq!(
            chat.goal_runtime_seconds(now + Duration::from_secs(10)) - reported,
            10
        );
        let _ = client.apply(&mut chat, state_with(EPOCH, false, settings()));
        assert_eq!(
            chat.goal_runtime_seconds(now + Duration::from_secs(3600)),
            0
        );
    }

    #[test]
    fn context_capacity_belongs_to_the_host_snapshot() {
        let mut client = SessionClient::new(SESSION.into());
        let mut chat = chat();
        let host_settings = serde_json::from_value(serde_json::json!({
            "provider": "scripted", "model_id": "scripted",
            "thinking": "off", "speed": "standard", "verbosity": "default",
            "context_window": 100_000
        }))
        .unwrap();
        let _ = client.apply(&mut chat, opening_with(EPOCH, false, host_settings));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        assert_eq!(
            chat.footers().context_usage(AgentId::Main).context_window,
            100_000
        );
        chat.footers_mut().set_context_tokens(AgentId::Main, 95_000);
        let display = |chat: &ChatState| {
            crate::footer::context_usage_display(chat.footers().context_usage(AgentId::Main))
        };
        let shown = display(&chat).unwrap();
        assert_eq!(shown.percent.as_deref(), Some("(95.0%)"));
        assert_eq!(shown.severity, crate::footer::UsageSeverity::Critical);

        let changed = AgentSettings {
            model_id: "custom-bundle".into(),
            context_window: 400_000,
            ..settings()
        };
        let _ = client.apply(&mut chat, state_with(EPOCH, false, changed));
        let shown = display(&chat).unwrap();
        assert_eq!(shown.percent.as_deref(), Some("(23.8%)"));
        assert_eq!(shown.severity, crate::footer::UsageSeverity::Normal);

        let legacy = serde_json::from_value(serde_json::json!({
            "provider": "scripted", "model_id": "custom-bundle",
            "thinking": "off", "speed": "standard", "verbosity": "default"
        }))
        .unwrap();
        let _ = client.apply(&mut chat, opening_with("new-epoch", false, legacy));
        let _ = client.apply(&mut chat, caught_up("new-epoch", 0));
        assert!(
            display(&chat).is_none(),
            "missing metadata must clear cached capacity"
        );
    }

    #[test]
    fn oracle_settings_follow_each_accepted_state_without_inference() {
        let mut client = SessionClient::new(SESSION.into());
        let mut chat = chat();
        assert!(client.oracle_settings().is_none());
        let mut oracle = settings();
        oracle.model_id = "oracle-model".into();
        oracle.thinking = "high".into();
        let frame = |session: &str, epoch: &str, opens_block, oracle_settings| Frame::State {
            session: session.into(),
            epoch: epoch.into(),
            opens_block,
            working: false,
            settings: settings(),
            oracle_settings,
            goal: None,
            credential_warning: None,
        };
        let opening = |session, epoch, oracle| frame(session, epoch, true, oracle);
        let state = |session, epoch, oracle| frame(session, epoch, false, oracle);
        let _ = client.apply(&mut chat, opening(SESSION, EPOCH, Some(oracle.clone())));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        assert_eq!(client.oracle_settings(), Some(&oracle));
        assert_eq!(client.settings(), Some(&settings()));
        for (session, epoch) in [("other-session", EPOCH), (SESSION, "old-epoch")] {
            assert!(!client.apply(&mut chat, state(session, epoch, None)).0);
            assert_eq!(client.oracle_settings(), Some(&oracle));
        }
        oracle.thinking = "low".into();
        let _ = client.apply(&mut chat, state(SESSION, EPOCH, Some(oracle.clone())));
        assert_eq!(client.oracle_settings(), Some(&oracle));
        assert_eq!(client.settings(), Some(&settings()));
        let _ = client.apply(&mut chat, opening(SESSION, "new-epoch", None));
        assert!(
            client.oracle_settings().is_none(),
            "older hosts cannot inherit cached Oracle settings"
        );
        let _ = client.apply(&mut chat, state(SESSION, "new-epoch", Some(oracle.clone())));
        let _ = client.apply(&mut chat, state(SESSION, "new-epoch", None));
        assert!(
            client.oracle_settings().is_none(),
            "every accepted State replaces Oracle settings"
        );
    }

    fn durable(epoch: &str, seq: u64, entry_id: &str, event: AgentEvent) -> Frame {
        Frame::Event {
            session: SESSION.to_string(),
            epoch: epoch.to_string(),
            durability: Some(aj_wire::DurableEvent {
                seq,
                entry_id: entry_id.to_string(),
                branch_settings: None,
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
            code: code.to_string(),
            message: message.to_string(),
        }
    }

    /// A refused attach is surfaced and ends the attachment: the
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
            "the host's own sentence reaches the user verbatim",
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

    /// A followed session whose log failed drops its attachment and owes one
    /// re-ask at once: the host rebuilds the session from disk on that ask,
    /// and the row stays listed throughout, so no directory edge would ever
    /// fire. If that re-ask is answered the same way it settles like any
    /// refusal, so a disk that stays broken never becomes a retry loop.
    #[test]
    fn a_persistence_failure_re_asks_once() {
        let (mut client, mut chat) = attached();
        let failure = || {
            refusal(
                SESSION,
                PERSISTENCE_FAILED_CODE,
                "Saving this session failed: no space left on device.",
            )
        };

        let _ = client.apply(&mut chat, failure());

        assert!(client.needs_reattach(), "the re-ask is owed immediately");
        assert_eq!(client.cursor(), None, "the failed epoch is let go of");
        assert!(
            errors(&chat)
                .iter()
                .any(|text| text.starts_with("Saving this session failed:")),
            "the message reaches the transcript: {:?}",
            errors(&chat)
        );

        client.attach_requested();
        let _ = client.apply(&mut chat, failure());

        assert!(
            !client.needs_reattach(),
            "a re-ask refused the same way waits for the user",
        );
    }

    /// A `state` without the opening marker never opens a block, however the
    /// client stands: waiting on a re-attach it just asked for, or refused.
    ///
    /// The host sends `state` on every change as well as at a block's start,
    /// so reading one as an opening would adopt its epoch, quiesce, and freeze
    /// the cursor until a `caught_up` nobody is sending.
    #[test]
    fn an_on_change_state_never_opens_a_block() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(&mut chat, durable(EPOCH, 1, "entry-1", notice("one")));
        // Asked again, so a block is on its way, and the on-change frame
        // overtakes it.
        client.attach_requested();
        let _ = client.apply(&mut chat, live(EPOCH, streaming_text("half a sen")));
        let _ = client.apply(&mut chat, state(EPOCH, true));
        let _ = client.apply(&mut chat, state("epoch-2", true));
        assert_eq!(client.attach_phase(), Attach::Live);
        assert!(streaming(&chat), "an on-change state quiesced");
        let _ = client.apply(&mut chat, durable(EPOCH, 2, "entry-2", notice("two")));
        let _ = client.apply(&mut chat, durable(EPOCH, 3, "entry-3", notice("three")));
        assert_eq!(
            client.cursor(),
            Some(Cursor {
                epoch: EPOCH.into(),
                seq: 2
            }),
            "live frames under the adopted epoch kept committing",
        );
        assert_eq!(notices(&chat), vec!["one", "two", "three"]);

        // Refused, the client follows nothing, and a stray on-change frame does
        // not bring it back.
        let _ = client.apply(
            &mut chat,
            refusal(SESSION, "unknown_session", "unknown session session-1"),
        );
        let _ = client.apply(&mut chat, state("epoch-2", false));
        assert!(
            !client
                .apply(&mut chat, durable("epoch-2", 1, "entry-1", notice("stray")))
                .0
        );
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
        let _ = client.apply(&mut chat, opening("epoch-2", false));
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
        let _ = client.apply(&mut chat, refusal(SESSION, "locked", reason));
        assert_eq!(notices(&chat), vec!["old branch only"]);
        assert_eq!(errors(&chat), vec![reason]);

        client.owe_reattach();
        let _ = client.apply(&mut chat, opening("new-branch", false));
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
        let cursor = client.cursor();

        let mut abandoned = settings();
        abandoned.model_id = "abandoned-branch".to_string();
        let _ = client.apply(
            &mut chat,
            durable("epoch-0", 9, "entry-9", notice("from an abandoned branch")),
        );
        let _ = client.apply(&mut chat, state_with("epoch-0", true, abandoned));
        let _ = client.apply(
            &mut chat,
            caught_up_with(
                "epoch-0",
                99,
                TaskTable {
                    tasks: vec![task_summary(7)],
                },
                QueueState::default(),
            ),
        );

        assert_eq!(notices(&chat), vec!["ours"]);
        assert_eq!(client.cursor(), cursor);
        assert_eq!(client.settings(), Some(&settings()));
        assert!(!client.working());
        assert!(
            chat.tasks().is_empty(),
            "a dropped caught_up replaces no task table",
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

        let _ = client.apply(&mut chat, opening(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 12));

        assert_eq!(
            client.cursor().map(|cursor| cursor.seq),
            Some(12),
            "a block commits whole",
        );
    }

    /// Inside a block the cursor does not move per frame, so a durable
    /// frame that came out below an earlier one still applies.
    ///
    /// The block is atomic and its `caught_up` commits it once, which is
    /// what lets the projection order its events by thread bracketing
    /// rather than by seq. Advancing per frame would make the cursor
    /// invariant read everything after the first descent as a duplicate,
    /// and a thread-scoped backfill would lose most of its rows to it.
    #[test]
    fn a_block_keeps_a_frame_that_came_out_below_an_earlier_one() {
        let (mut client, mut chat) = attached();

        let _ = client.apply(&mut chat, opening(EPOCH, false));
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

        let _ = client.apply(&mut chat, opening(EPOCH, false));

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

        let _ = client.apply(&mut chat, opening(EPOCH, false));

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
        let _ = client.apply(&mut chat, opening(EPOCH, false));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        assert!(!client.lifecycle().is_running(AgentId::Main));
        assert!(!client.working());
    }

    #[test]
    fn state_working_spins_for_a_joiner_mid_turn() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();

        let _ = client.apply(&mut chat, opening(EPOCH, true));
        let _ = client.apply(&mut chat, caught_up(EPOCH, 3));

        assert!(client.lifecycle().is_running(AgentId::Main));
        assert!(client.working());
    }

    #[test]
    fn first_attach_settings_are_available_once_without_reconnect_duplication() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        let _ = client.apply(
            &mut chat,
            opening_with_warning(
                EPOCH,
                false,
                settings(),
                Some("the host has no credentials"),
            ),
        );
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));
        assert_eq!(
            client
                .take_first_attach_settings()
                .map(|settings| settings.model_id),
            Some("scripted".to_string()),
        );
        assert!(client.take_first_attach_settings().is_none());
        assert_eq!(
            client.take_first_attach_credential_warning().as_deref(),
            Some("the host has no credentials"),
        );
        assert!(client.take_first_attach_credential_warning().is_none());

        let _ = client.apply(
            &mut chat,
            opening_with_warning(
                EPOCH,
                false,
                settings(),
                Some("a reconnect found another credential problem"),
            ),
        );
        assert!(
            client.take_first_attach_settings().is_none(),
            "a reconnect does not regenerate the local restore summary",
        );
        assert!(
            client.take_first_attach_credential_warning().is_none(),
            "a reconnect does not repeat its credential warning",
        );
    }

    /// First-attach presentation commits with the block. A State from a block
    /// that never reached Caught cannot freeze a stale settings summary or
    /// credential warning ahead of the retry that did.
    #[test]
    fn an_interrupted_first_attach_keeps_the_successful_blocks_presentation() {
        let mut client = SessionClient::new(SESSION.to_string());
        let mut chat = chat();
        let _ = client.apply(
            &mut chat,
            opening_with_warning(EPOCH, false, settings(), Some("stale warning")),
        );
        // The stream carrying that block was lost before its `caught_up`, and
        // the retry's block opens over it.
        let mut current = settings();
        current.model_id = "current-model".into();
        let _ = client.apply(
            &mut chat,
            opening_with_warning(EPOCH, false, current, Some("current warning")),
        );
        let _ = client.apply(&mut chat, caught_up(EPOCH, 0));

        assert_eq!(
            client
                .take_first_attach_settings()
                .map(|settings| settings.model_id)
                .as_deref(),
            Some("current-model"),
        );
        assert_eq!(
            client.take_first_attach_credential_warning().as_deref(),
            Some("current warning"),
        );
    }

    /// Neither task nor queue events are replayable, so `caught_up` carries
    /// both tables and a same-epoch re-attach replaces what the client held:
    /// a task that ended while it was away reads as ended, and an agent whose
    /// queue drained in the gap shows nothing pending. Events that raced the
    /// host's snapshot replay after it without undoing it.
    #[test]
    fn caught_up_replaces_the_task_table_and_queues() {
        let (mut client, mut chat) = attached();
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::TaskStart {
                    agent_id: AgentId::Main,
                    task_id: 7,
                    call_id: "call-1".into(),
                    kind: TaskKind::Bash {
                        command: "sleep 1".into(),
                    },
                    label: "sleep 1".into(),
                },
            ),
        );
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::QueueUpdate {
                    agent_id: AgentId::Sub(1),
                    steering: vec![queued("drained in the gap")],
                    follow_up: Vec::new(),
                },
            ),
        );
        assert_eq!(chat.tasks()[&7].status, TaskStatus::Running);
        assert_eq!(chat.queue().queues.len(), 1);

        let _ = client.apply(&mut chat, opening(EPOCH, false));
        let _ = client.apply(
            &mut chat,
            caught_up_with(
                EPOCH,
                0,
                TaskTable {
                    tasks: vec![TaskSummary {
                        status: TaskStatus::Exited(Some(0)),
                        ..task_summary(7)
                    }],
                },
                QueueState {
                    queues: vec![AgentQueue {
                        agent_id: AgentId::Main,
                        steering: Vec::new(),
                        follow_up: vec![queued("queued in the gap")],
                    }],
                },
            ),
        );

        assert_eq!(chat.tasks()[&7].status, TaskStatus::Exited(Some(0)));
        assert_eq!(chat.queue().queues.len(), 1);
        assert_eq!(chat.queue().queues[0].agent_id, AgentId::Main);

        // A `TaskStart` published before the snapshot replays after it and
        // must not reopen the task.
        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::TaskStart {
                    agent_id: AgentId::Main,
                    task_id: 7,
                    call_id: "call-1".into(),
                    kind: TaskKind::Bash {
                        command: "sleep 1".into(),
                    },
                    label: "sleep 1".into(),
                },
            ),
        );
        assert_eq!(chat.tasks()[&7].status, TaskStatus::Exited(Some(0)));
        // Output for a task the table does not list is inert: the reducer
        // freezes output for an untracked task.
        assert!(!client.apply(&mut chat, live(EPOCH, task_output(9))).0);
        assert_eq!(chat.tasks().len(), 1, "the unknown task was ignored");
    }

    /// A re-attach seeds the main agent's mark and leaves the sub-agents'
    /// alone: `working` says nothing about them, and clearing a
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
        let _ = client.apply(&mut chat, opening(EPOCH, false));
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

        let _ = client.apply(
            &mut chat,
            live(
                EPOCH,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            ),
        );
        let _ = client.apply(&mut chat, opening(EPOCH, false));
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

        client.attach_requested();
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
