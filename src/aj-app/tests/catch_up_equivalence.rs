//! Catch-up equivalence for the chat reducer: a client that loses its
//! connection mid-turn and re-attaches has to converge on the same
//! durable-derived state as one that never dropped (spec 2, 6.5, 11.2).
//!
//! The turn is a real scripted-provider run, so the frames are the exact
//! shapes the agent emits, tagged with their log entries by the same
//! forwarder a session host uses. The suffix a re-attach applies is the
//! real `project_suffix` output, not a filtered replay of the live stream.

use std::collections::BTreeSet;
use std::sync::Arc;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::{ChatState, reduce};
use aj_app::session::AgentLifecycle;
use aj_app::test_support::{
    CanonicalState, assert_canonical_eq, assert_no_dangling, build_tagged_test_agent,
    finalized_text_message, scripted_run_config,
};
use aj_models::types::{AssistantContent, StopReason, ToolCall};
use aj_session::{ConversationPersistence, LogSnapshot, TaggedEvent, project_suffix};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// One attached client, applying spec 6.5's client rules over a stream of
/// tagged frames.
struct Client {
    chat: ChatState,
    life: AgentLifecycle,
    /// Last durable seq applied, which is what the cursor invariant
    /// compares against.
    applied: Option<u64>,
    /// Last durable seq committed, which is what a re-attach offers. It
    /// lags `applied` by one durable frame, because a log entry can
    /// project a trailing untagged event and a connection that dropped in
    /// between would otherwise claim an entry it only partly applied.
    committed: Option<u64>,
    /// Whether the lossy frames still to come are stale by construction.
    ///
    /// Spec 6.5 has the server drop the lossy frames that were in flight
    /// when an attach was served, because a cumulative snapshot delivered
    /// after the durable frame that superseded it resurrects stale
    /// transient state. Here the backfill is projected from the finished
    /// log, so it supersedes every snapshot left in the stream, and the
    /// faithful model of that rule is to drop all of them from the
    /// re-attach on.
    drop_lossy: bool,
}

impl Client {
    fn new() -> Self {
        Self {
            chat: ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new())),
            life: AgentLifecycle::default(),
            applied: None,
            committed: None,
            drop_lossy: false,
        }
    }

    /// Apply one frame.
    ///
    /// `advance` is false inside a backfill block, which the client treats
    /// atomically: it advances its cursor once to the block's `last_seq`
    /// rather than per frame.
    fn apply(&mut self, frame: &TaggedEvent, advance: bool) {
        if let Some(entry) = &frame.entry {
            // Cursor invariant: a durable frame at or below the last
            // applied seq is a duplicate. This is de-duplication, not the
            // correctness mechanism, because the entry's trailing events
            // carry no tag and still apply below.
            if self.applied.is_some_and(|applied| entry.seq <= applied) {
                return;
            }
            if advance {
                self.committed = self.applied;
                self.applied = Some(entry.seq);
            }
            let _ = reduce(
                &mut self.chat,
                &mut self.life,
                frame.event.clone(),
                Some(&entry.id),
            );
            return;
        }
        if self.drop_lossy && is_lossy(&frame.event) {
            return;
        }
        let _ = reduce(&mut self.chat, &mut self.life, frame.event.clone(), None);
    }

    /// Re-attach: quiesce, apply the suffix the server projects from the
    /// cursor we offer, then adopt the block's high-water mark.
    fn reattach(&mut self, log: &LogSnapshot) {
        self.chat.quiesce(&mut self.life);
        // The attach block opens with a `state` frame, whose `working`
        // seeds the lifecycle. Nothing else can: it is not derivable from
        // projected events, and a bracket whose `AgentEnd` fell into the
        // disconnected window would otherwise leave the spinner stuck
        // forever. This host is idle, the turn having finished.
        for agent in self.life.running_agents() {
            self.life.mark_idle(agent);
        }
        // The projection leaves open exactly the brackets of the runs the
        // host knows are still live. This simulated attach is served
        // against a finished log, so the host knows of none.
        let backfill = project_suffix(log, self.committed, &BTreeSet::new());
        for frame in &backfill.events {
            self.apply(frame, false);
        }
        // After `caught_up` every sub-agent the host knows to be idle is
        // concluded, which is what unwedges a box whose `SubAgentEnd` fell
        // into the disconnected window with no durable entry behind it.
        for agent in self.chat.agents() {
            if let AgentId::Sub(n) = agent.id
                && !backfill.open_subs.contains(&n)
            {
                self.chat.conclude_sub_box(n);
            }
        }
        // `caught_up`: the block advances the cursor once.
        self.applied = Some(log.last_seq());
        self.committed = self.applied;
        self.drop_lossy = true;
    }

    fn canonical(&self) -> CanonicalState {
        CanonicalState::of(&self.chat, &self.life)
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
    let mut client = Client::new();
    for frame in frames {
        client.apply(frame, true);
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
fn sweep(frames: &[TaggedEvent], log: &LogSnapshot, expected: &CanonicalState) {
    let mut pairs = 0;
    for cut in 0..=frames.len() {
        for resume in cut..=frames.len() {
            let mut client = Client::new();
            for frame in &frames[..cut] {
                client.apply(frame, true);
            }
            client.reattach(log);
            for frame in &frames[resume..] {
                client.apply(frame, true);
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

    sweep(&frames, &log, &expected);
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

    sweep(&frames, &log, &expected);
}

/// The degenerate cursor: a client with complete state re-attaches offering
/// nothing and gets the whole log back, which is what a stale epoch (a head
/// switch, a host restart) produces. Every event of the real projection is
/// therefore applied a second time.
#[tokio::test]
async fn reapplying_the_whole_projected_suffix_changes_nothing() {
    for (_dir, frames, log) in [scripted_tool_turn().await, scripted_sub_agent_turn().await] {
        let mut client = uninterrupted(&frames);
        let before = client.canonical();
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
        assert!(
            !project_suffix(&log, None, &BTreeSet::new())
                .events
                .is_empty(),
            "the projection emits events to re-apply",
        );

        client.applied = None;
        client.committed = None;
        client.reattach(&log);

        assert_canonical_eq(
            &client.canonical(),
            &before,
            "full backfill over complete state",
        );
        assert_no_dangling(&client.chat);
    }
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
        CanonicalState::of(&chat, &life),
        reference.canonical(),
        "an identity-blind fold has to duplicate rows",
    );
}
