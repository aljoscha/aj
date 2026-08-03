//! Catch-up equivalence for the client fold: a client that loses its
//! connection mid-turn and re-attaches has to converge on the same
//! durable-derived state as one that never dropped (spec 2, 6.5, 11.2).
//!
//! The turn is a real scripted-provider run, so the frames are the exact
//! shapes the agent emits, tagged with their log entries by the same
//! forwarder a session host uses. The suffix a re-attach applies is the
//! real `project_suffix` output, not a filtered replay of the live stream,
//! and the fold under test is the real [`SessionClient`].

use std::collections::BTreeSet;
use std::sync::Arc;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::{ChatState, reduce};
use aj_app::client::SessionClient;
use aj_app::session::AgentLifecycle;
use aj_app::test_support::{
    CanonicalState, assert_canonical_eq, assert_no_dangling, build_tagged_test_agent,
    finalized_text_message, scripted_run_config,
};
use aj_models::types::{AssistantContent, StopReason, ToolCall};
use aj_session::{ConversationPersistence, LogSnapshot, TaggedEvent, project_suffix};
use aj_wire::{DurableEvent, Frame};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// The session every frame in this harness belongs to.
const SESSION: &str = "harness-session";

/// The epoch the host serves under. One materialization means one epoch,
/// which the client adopts on its first attach and keeps across every
/// re-attach.
const EPOCH: &str = "epoch-1";

/// One attached client: the fold under test plus the [`ChatState`] it
/// folds into, which stays outside [`SessionClient`] because the TUI holds
/// it behind widgets it cannot repoint.
struct Client {
    client: SessionClient,
    chat: ChatState,
}

impl Client {
    /// A client that has just attached an empty session: the host serves
    /// the opening `state`, an empty backfill, and `caught_up` at seq 0.
    fn attached() -> Self {
        let mut this = Self {
            client: SessionClient::new(SESSION.to_string()),
            chat: ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new())),
        };
        this.client.expect_attach();
        this.apply(state_frame(EPOCH, 0, false));
        this.apply(caught_up_frame(EPOCH, 0));
        this
    }

    fn apply(&mut self, frame: Frame) {
        let _ = self.client.apply(&mut self.chat, frame);
    }

    /// Apply one live frame of the recorded run.
    fn live(&mut self, tagged: &TaggedEvent) {
        self.apply(event_frame(EPOCH, tagged));
    }

    /// The attach block the host serves for the cursor this client offers:
    /// the opening `state`, the projected suffix, `caught_up`, then the
    /// conclusion sweep the host runs for every sub-agent it knows to be
    /// idle.
    fn reattach(&mut self, log: &LogSnapshot, epoch: &str) {
        self.client.expect_attach();
        // A real client names its cursor in the stream request. A cursor
        // from another epoch says nothing about this one, so the server
        // serves everything instead.
        let cursor = self
            .client
            .cursor()
            .filter(|cursor| cursor.epoch == epoch)
            .map(|cursor| cursor.seq);
        // The projection leaves open exactly the brackets of the runs the
        // host knows are still live. This attach is served against a
        // finished log, so the host knows of none.
        let backfill = project_suffix(log, cursor, &BTreeSet::new());
        // The block's opening `state` carries the `working` seed, which no
        // projected event can carry: a bracket whose `AgentEnd` fell into
        // the disconnected window would otherwise leave a spinner running
        // forever. This host is idle, the turn having finished.
        self.apply(state_frame(epoch, log.last_seq(), false));
        for tagged in &backfill.events {
            self.apply(event_frame(epoch, tagged));
        }
        self.apply(caught_up_frame(epoch, log.last_seq()));
        // After `caught_up` the host concludes every sub-agent it knows to
        // be idle, which is what unwedges a box whose `SubAgentEnd` fell
        // into the disconnected window with no durable entry behind it. It
        // sweeps the runs the projection walked, not the log's full set,
        // which is what keeps an abandoned branch's runs out of it.
        for child in backfill.subs.difference(&backfill.open_subs) {
            self.apply(agent_end_frame(epoch, AgentId::Sub(*child)));
        }
    }

    fn canonical(&self) -> CanonicalState {
        CanonicalState::of(&self.chat, &self.client)
    }
}

fn event_frame(epoch: &str, tagged: &TaggedEvent) -> Frame {
    Frame::Event {
        session: SESSION.to_string(),
        epoch: epoch.to_string(),
        durability: tagged.entry.as_ref().map(|entry| DurableEvent {
            seq: entry.seq,
            entry_id: entry.id.clone(),
        }),
        event: tagged.event.clone().into(),
    }
}

fn state_frame(epoch: &str, last_seq: u64, working: bool) -> Frame {
    Frame::State {
        session: SESSION.to_string(),
        epoch: epoch.to_string(),
        working,
        settings: scripted_settings(),
        last_seq,
    }
}

fn caught_up_frame(epoch: &str, last_seq: u64) -> Frame {
    Frame::CaughtUp {
        session: SESSION.to_string(),
        epoch: epoch.to_string(),
        last_seq,
    }
}

fn agent_end_frame(epoch: &str, agent_id: AgentId) -> Frame {
    Frame::Event {
        session: SESSION.to_string(),
        epoch: epoch.to_string(),
        durability: None,
        event: AgentEvent::AgentEnd {
            agent_id,
            messages: Vec::new(),
        }
        .into(),
    }
}

/// The three cumulative-snapshot events (spec 6.4).
fn is_lossy(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::MessageUpdate { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::TaskOutput { .. }
    )
}

fn scripted_settings() -> AgentSettings {
    AgentSettings {
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
}

/// Drive one scripted turn that calls a tool, and return its tagged live
/// frames plus the log they were appended to.
async fn scripted_tool_turn() -> (TempDir, Vec<TaggedEvent>, LogSnapshot) {
    let mut calling = finalized_text_message("let me check the list");
    calling.content.push(AssistantContent::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "todo_read".into(),
        arguments: serde_json::json!({}),
    }));
    calling.stop_reason = StopReason::ToolUse;
    recorded_turn(
        vec![calling, finalized_text_message("nothing on it")],
        "check the todos",
    )
    .await
}

/// Drive one scripted turn that spawns a blocking sub-agent, and return
/// its tagged live frames plus the log.
///
/// The parent and the sub-agent share one scripted provider, so the
/// scripts are consumed in run order across both: the parent's `agent`
/// tool call, the sub-agent's single-turn report, then the parent's
/// concluding text.
async fn scripted_sub_agent_turn() -> (TempDir, Vec<TaggedEvent>, LogSnapshot) {
    let mut spawning = finalized_text_message("delegating that");
    spawning.content.push(AssistantContent::ToolCall(ToolCall {
        id: "call-sub".into(),
        name: "agent".into(),
        arguments: serde_json::json!({"task": "look into it"}),
    }));
    spawning.stop_reason = StopReason::ToolUse;
    recorded_turn(
        vec![
            spawning,
            finalized_text_message("the sub found nothing"),
            finalized_text_message("nothing to report"),
        ],
        "delegate the search",
    )
    .await
}

/// Drive `prompt` against a scripted agent replaying `messages`, and
/// return the tagged frames it emitted plus the resulting log.
async fn recorded_turn(
    messages: Vec<aj_models::types::AssistantMessage>,
    prompt: &str,
) -> (TempDir, Vec<TaggedEvent>, LogSnapshot) {
    let dir = TempDir::new().expect("tempdir");
    let persistence = ConversationPersistence::new(dir.path().to_path_buf());
    let run_config = scripted_run_config(messages);
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    agent
        .prompt(prompt.to_string(), CancellationToken::new())
        .await
        .expect("scripted turn");

    let mut recorded = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        recorded.push(frame);
    }
    let snapshot = log.lock().await.snapshot();
    (dir, recorded, snapshot)
}

/// Fold every frame with no interruption: the reference state.
fn uninterrupted(frames: &[TaggedEvent]) -> Client {
    let mut client = Client::attached();
    for frame in frames {
        client.live(frame);
    }
    client
}

/// The number of transcript rows one scripted tool turn renders: the user
/// prompt, the tool-calling assistant message and its usage, the tool
/// cell, then the concluding assistant message and its usage.
const TURN_ROWS: usize = 6;

/// The sweep: for every pair `(cut, resume)` simulate a disconnect after
/// `cut` frames and a re-attach that resumes live delivery at `resume`,
/// losing everything in between, and require the same canonical state as
/// the uninterrupted fold.
///
/// `expected_pairs` is pinned by the caller so a change that quietly
/// shortens the recorded stream cannot shrink the sweep with it.
fn sweep(
    frames: &[TaggedEvent],
    log: &LogSnapshot,
    expected: &CanonicalState,
    expected_pairs: usize,
) {
    let mut pairs = 0;
    for cut in 0..=frames.len() {
        for resume in cut..=frames.len() {
            let mut client = Client::attached();
            for frame in &frames[..cut] {
                client.live(frame);
            }
            client.reattach(log, EPOCH);
            for frame in &frames[resume..] {
                // Spec 6.5 has the server drop the lossy frames that were
                // in flight when an attach was served, because a
                // cumulative snapshot delivered after the durable frame
                // that superseded it resurrects stale transient state: a
                // `MessageUpdate` for a message the backfill already
                // finalized would paint a second, unfinalized copy of it.
                // This backfill is projected from the finished log, so it
                // supersedes every snapshot left in the stream.
                if is_lossy(&frame.event) {
                    continue;
                }
                client.live(frame);
            }
            assert_canonical_eq(
                &client.canonical(),
                expected,
                &format!("cut {cut}, resume {resume}"),
            );
            assert_no_dangling(&client.chat);
            pairs += 1;
        }
    }
    let n = frames.len() + 1;
    assert_eq!(pairs, n * (n + 1) / 2, "every pair was exercised");
    assert_eq!(pairs, expected_pairs, "the sweep covers the whole stream");
}

#[tokio::test]
async fn every_cut_and_resume_of_a_tool_turn_converges() {
    let (_dir, frames, log) = scripted_tool_turn().await;
    assert!(
        frames.iter().any(|f| matches!(
            &f.event,
            AgentEvent::ToolExecutionEnd { tool, .. } if tool == "todo_read"
        )),
        "the turn ran its tool call",
    );
    let durable = frames.iter().filter(|f| f.entry.is_some()).count();
    assert!(durable >= 4, "the turn wrote several log entries");

    let reference = uninterrupted(&frames);
    let expected = reference.canonical();
    // The comparison is only worth making if the fold built the whole
    // turn rather than converging on something empty.
    assert_eq!(
        expected
            .agent(AgentId::Main)
            .expect("main transcript")
            .entries
            .len(),
        TURN_ROWS,
        "the reference fold built the whole turn",
    );
    assert_no_dangling(&reference.chat);

    sweep(&frames, &log, &expected, 528);
}

#[tokio::test]
async fn every_cut_and_resume_of_a_sub_agent_turn_converges() {
    let (_dir, frames, log) = scripted_sub_agent_turn().await;
    let reference = uninterrupted(&frames);
    let expected = reference.canonical();
    let sub = expected
        .agent(AgentId::Sub(1))
        .expect("the sub-agent built a transcript");
    assert!(
        !sub.entries.is_empty(),
        "the sub-agent's own transcript is non-empty",
    );
    assert_eq!(
        expected.sub_boxes.len(),
        1,
        "the parent transcript holds the box",
    );
    assert_no_dangling(&reference.chat);

    sweep(&frames, &log, &expected, 1176);
}

/// A host restart mints a fresh epoch, so the cursor the client offers is
/// stale and the whole log comes back under the new epoch. The client
/// drops everything it built under the old epoch and rebuilds from the
/// full backfill, which has to land on the same state.
#[tokio::test]
async fn an_attach_under_a_new_epoch_rebuilds_the_same_state() {
    for (_dir, frames, log) in [scripted_tool_turn().await, scripted_sub_agent_turn().await] {
        let mut client = uninterrupted(&frames);
        let expected = client.canonical();
        assert_eq!(
            expected
                .agent(AgentId::Main)
                .expect("main transcript")
                .entries
                .len(),
            TURN_ROWS,
            "the compared state is a whole turn",
        );

        client.reattach(&log, "epoch-2");

        assert_canonical_eq(
            &client.canonical(),
            &expected,
            "full backfill under a new epoch",
        );
        assert_no_dangling(&client.chat);
        assert_eq!(
            client.client.cursor().map(|cursor| cursor.epoch),
            Some("epoch-2".to_string()),
            "the client offers the adopted epoch",
        );
    }
}

/// The degenerate re-application: every event of the real projection,
/// durable frames included, applied a second time onto complete state.
///
/// This folds through `reduce` rather than through [`SessionClient`]
/// deliberately. The client's cursor invariant drops the durable frames of
/// entries it already applied, and spec 6.5 is explicit that the invariant
/// is a de-duplication optimization rather than the correctness mechanism.
/// Idempotent application is, so this pins the property the invariant is
/// not allowed to stand in for.
#[tokio::test]
async fn reapplying_the_whole_projected_suffix_changes_nothing() {
    for (_dir, frames, log) in [scripted_tool_turn().await, scripted_sub_agent_turn().await] {
        let mut chat = ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new()));
        let mut life = AgentLifecycle::default();
        for tagged in &frames {
            fold(&mut chat, &mut life, tagged);
        }
        let before = CanonicalState::of_reduced(&chat, &life);
        // The comparison is worthless if the state is empty.
        assert_eq!(
            before
                .agent(AgentId::Main)
                .expect("main transcript")
                .entries
                .len(),
            TURN_ROWS,
            "the compared state is a whole turn",
        );
        let backfill = project_suffix(&log, None, &BTreeSet::new());
        assert!(
            !backfill.events.is_empty(),
            "the projection emits events to re-apply",
        );

        for tagged in &backfill.events {
            fold(&mut chat, &mut life, tagged);
        }

        assert_canonical_eq(
            &CanonicalState::of_reduced(&chat, &life),
            &before,
            "full backfill over complete state",
        );
        assert_no_dangling(&chat);
    }
}

/// Fold one tagged event straight into the reducer, handing it the log
/// entry a durable frame's envelope would carry.
fn fold(chat: &mut ChatState, life: &mut AgentLifecycle, tagged: &TaggedEvent) {
    let _ = reduce(
        chat,
        life,
        tagged.event.clone(),
        tagged.entry.as_ref().map(|entry| &entry.id),
    );
}

/// A guard on the harness itself: durable identity is what absorbs the
/// re-application, so a fold with the identities stripped has to diverge.
/// Otherwise the sweep could be passing vacuously.
#[tokio::test]
async fn a_fold_without_durable_identity_diverges() {
    let (_dir, frames, log) = scripted_tool_turn().await;
    let reference = uninterrupted(&frames);

    let mut chat = ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new()));
    let mut life = AgentLifecycle::default();
    let strip = |frame: &TaggedEvent| {
        let mut event = frame.event.clone();
        // Dropping the id the log adopted is what leaves the message arms
        // with nothing to key on.
        if let AgentEvent::MessageEnd { message, .. } = &mut event {
            message.set_id(String::new());
        }
        event
    };
    for frame in &frames {
        let _ = reduce(&mut chat, &mut life, strip(frame), None);
    }
    for frame in &project_suffix(&log, None, &BTreeSet::new()).events {
        let _ = reduce(&mut chat, &mut life, strip(frame), None);
    }

    assert_ne!(
        CanonicalState::of_reduced(&chat, &life),
        reference.canonical(),
        "an identity-blind fold has to duplicate rows",
    );
}

/// A second guard on the oracle: a pending queued message is part of the
/// state two clients have to agree on (spec 11.2), so a client that was
/// told about one and a client that was not must not compare equal.
///
/// The reducer treats `QueueUpdate` as a redraw ping and drops the payload,
/// so nothing in the transcript records this. The client's own snapshot is
/// the only witness, and an oracle blind to it would call a client with a
/// queued follow-up converged with one that has none.
#[tokio::test]
async fn a_client_with_a_queued_message_differs_from_one_without() {
    let (_dir, frames, _log) = scripted_tool_turn().await;
    let mut told = uninterrupted(&frames);
    let mut untold = uninterrupted(&frames);
    assert_eq!(
        told.canonical(),
        untold.canonical(),
        "the two folds start out identical",
    );

    told.apply(queue_update_frame(
        EPOCH,
        AgentId::Main,
        "queued while busy",
    ));

    assert_ne!(
        told.canonical(),
        untold.canonical(),
        "the queued follow-up is state the oracle has to see",
    );

    // And the same update lands them back on each other, so the projection
    // reports a real difference rather than an unstable one.
    untold.apply(queue_update_frame(
        EPOCH,
        AgentId::Main,
        "queued while busy",
    ));
    assert_canonical_eq(
        &told.canonical(),
        &untold.canonical(),
        "both clients heard the same update",
    );
}

/// The frame the host publishes on the enqueue side: a full snapshot of one
/// agent's queues, here a single pending follow-up.
fn queue_update_frame(epoch: &str, agent_id: AgentId, text: &str) -> Frame {
    Frame::Event {
        session: SESSION.to_string(),
        epoch: epoch.to_string(),
        durability: None,
        event: AgentEvent::QueueUpdate {
            agent_id,
            steering: Vec::new(),
            follow_up: vec![aj_agent::message::AgentMessage::wire(
                aj_models::types::Message::User(aj_models::types::UserMessage::text(text)),
            )],
        }
        .into(),
    }
}
