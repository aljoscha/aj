//! Catch-up equivalence for the client fold: a client that loses its
//! connection mid-turn and re-attaches has to converge on the same
//! durable-derived state as one that never dropped.
//!
//! The turn is a real scripted-provider run, so the frames are the exact
//! shapes the agent emits, tagged with their log entries by the same
//! forwarder a session host uses. The suffix a re-attach applies is the
//! real `project_suffix` output, not a filtered replay of the live stream,
//! and the fold under test is the real [`SessionClient`].

use std::collections::BTreeSet;
use std::sync::Arc;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::{ChatState, EntryKind, SubAgentStatus, ToolStatus, reduce};
use aj_app::client::SessionClient;
use aj_app::session::AgentLifecycle;
use aj_app::test_support::{
    CanonicalEntry, CanonicalState, assert_canonical_eq, assert_convergent_eq, assert_no_dangling,
    build_tagged_test_agent, finalized_text_message, scripted_run_config,
};
use aj_models::types::{AssistantContent, AssistantError, ErrorCategory, StopReason, ToolCall};
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
    /// A client that attached the session as soon as it was created: the
    /// host serves the block of the seeded log, which holds the session's
    /// creation records (its context notice) and no turn yet.
    fn attached(seeded: &LogSnapshot) -> Self {
        let mut this = Self {
            client: SessionClient::new(SESSION.to_string()),
            chat: ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new())),
        };
        this.reattach(seeded, EPOCH);
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
            branch_settings: tagged.branch_settings.clone().map(|settings| {
                aj_wire::BranchSettings {
                    oracle_model: settings
                        .oracle_model
                        .map(|(api, name)| aj_wire::RecordedModel { api, name }),
                    oracle_thinking: settings.oracle_thinking,
                    oracle_speed: settings.oracle_speed,
                    oracle_verbosity: settings.oracle_verbosity,
                    model: settings
                        .model
                        .map(|(api, name)| aj_wire::RecordedModel { api, name }),
                    thinking: settings.thinking,
                    speed: settings.speed,
                    verbosity: settings.verbosity,
                    accounts: settings.accounts,
                }
            }),
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
        oracle_settings: None,
        credential_warning: None,
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

/// The three cumulative-snapshot events, the lossy frame class.
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
        thinking_display: "default".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
}

/// One scripted turn as a client would have seen it: the log as seeded at
/// creation (what the first attach block serves), the tagged live frames the
/// turn emitted, and the finished log.
struct Recorded {
    _dir: TempDir,
    seeded: LogSnapshot,
    frames: Vec<TaggedEvent>,
    log: LogSnapshot,
}

async fn scripted_tool_turn() -> Recorded {
    let mut calling = finalized_text_message("let me check the list");
    calling.content.push(AssistantContent::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "todo_read".into(),
        arguments: serde_json::json!({}),
    }));
    calling.stop_reason = StopReason::ToolUse;
    let run = recorded_turn(
        vec![calling, finalized_text_message("nothing on it")],
        "check the todos",
    )
    .await;
    let client = uninterrupted(&run);
    assert_completed_turn(&client.chat, "check the todos", "nothing on it");
    let entries = client
        .chat
        .transcript(AgentId::Main)
        .expect("main transcript")
        .entries();
    let tools: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Tool(tool) => Some((tool.tool.as_str(), tool.status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        [("todo_read", ToolStatus::Done { is_error: false })],
        "the tool completed once"
    );
    run
}

/// Drive one scripted turn that spawns a blocking sub-agent, and return
/// its tagged live frames plus the log.
///
/// The parent and the sub-agent share one scripted provider, so the
/// scripts are consumed in run order across both: the parent's `agent`
/// tool call, the sub-agent's single-turn report, then the parent's
/// concluding text.
async fn scripted_sub_agent_turn() -> Recorded {
    let mut spawning = finalized_text_message("delegating that");
    spawning.content.push(AssistantContent::ToolCall(ToolCall {
        id: "call-sub".into(),
        name: "agent".into(),
        arguments: serde_json::json!({"task": "look into it"}),
    }));
    spawning.stop_reason = StopReason::ToolUse;
    let run = recorded_turn(
        vec![
            spawning,
            finalized_text_message("the sub found nothing"),
            finalized_text_message("nothing to report"),
        ],
        "delegate the search",
    )
    .await;
    let reference = uninterrupted(&run);
    assert_completed_turn(&reference.chat, "delegate the search", "nothing to report");
    let main = reference
        .chat
        .transcript(AgentId::Main)
        .expect("main transcript");
    assert!(
        main.entries().iter().any(|entry| matches!(
            &entry.kind, EntryKind::SubAgent(sub)
                if sub.status == SubAgentStatus::Done
                    && sub.report.as_deref() == Some("the sub found nothing")
        )),
        "the parent shows the completed sub-agent's report"
    );
    let sub = reference
        .chat
        .transcript(AgentId::Sub(1))
        .expect("sub transcript");
    assert!(
        sub.entries().iter().any(|entry| matches!(
            &entry.kind, EntryKind::Assistant(assistant)
                if assistant.finalized && assistant.message.content.iter().any(|content| matches!(
                    content, AssistantContent::Text(text) if text.text == "the sub found nothing"
                ))
        )),
        "the sub-agent has its own completed answer"
    );
    run
}

/// Drive one scripted turn whose first inference fails transiently and is
/// retried into a success, and return its tagged live frames plus the log.
///
/// Both attempts are persisted: the agent brackets each inference with a
/// `MessageStart`/`MessageEnd` pair, so the failed partial lands on disk
/// even though it never reaches the transcript. Only the successful one
/// reports usage.
async fn scripted_retried_turn() -> Recorded {
    let mut failed = finalized_text_message("");
    failed.stop_reason = StopReason::Error;
    failed.error = Some(AssistantError::new(
        ErrorCategory::Transient,
        "stream ended without a terminal event",
    ));
    let mut recovered = finalized_text_message("recovered");
    // Real usage on the attempt that completed, so the surviving
    // `UsageUpdate` is one carrying tokens rather than an empty default.
    recovered.usage.input = 40;
    recovered.usage.output = 7;
    recovered.usage.total_tokens = 47;
    recorded_turn(vec![failed, recovered], "try that").await
}

/// Drive `prompt` against a scripted agent replaying `messages`, and
/// return the tagged frames it emitted plus the resulting log.
async fn recorded_turn(
    messages: Vec<aj_models::types::AssistantMessage>,
    prompt: &str,
) -> Recorded {
    let dir = TempDir::new().expect("tempdir");
    let persistence = ConversationPersistence::new(dir.path().to_path_buf());
    let run_config = scripted_run_config(messages);
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);
    let seeded = log.lock().await.snapshot();

    agent
        .prompt(prompt.to_string(), CancellationToken::new())
        .await
        .expect("scripted turn");

    let mut recorded = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        recorded.push(frame);
    }
    let snapshot = log.lock().await.snapshot();
    Recorded {
        _dir: dir,
        seeded,
        frames: recorded,
        log: snapshot,
    }
}

/// Fold every frame with no interruption: the reference state.
fn uninterrupted(run: &Recorded) -> Client {
    let mut client = Client::attached(&run.seeded);
    for frame in &run.frames {
        client.live(frame);
    }
    client
}

/// The comparison needs a completed conversation, not two equally empty folds.
/// Check the user-visible fixture content independently of the projection.
fn assert_completed_turn(chat: &ChatState, prompt: &str, answer: &str) {
    let entries = chat
        .transcript(AgentId::Main)
        .expect("main transcript")
        .entries();
    let prompts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::User(user) => Some(user.joined_text()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, [prompt], "the prompt appears once");
    assert!(
        entries.iter().any(|entry| matches!(
            &entry.kind, EntryKind::Assistant(assistant)
                if assistant.finalized && assistant.message.content.iter().any(|content| matches!(
                    content, AssistantContent::Text(text) if text.text == answer
                ))
        )),
        "the concluding answer reached the transcript"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::TurnUsage(_)))
            .count(),
        2,
        "both successful inferences report usage, including zero-token usage"
    );
}

/// The sweep: for every pair `(cut, resume)` simulate a disconnect after
/// `cut` frames and a re-attach that resumes live delivery at `resume`,
/// losing everything in between, and require the same canonical state as
/// the uninterrupted fold.
fn sweep(run: &Recorded, expected: &CanonicalState) {
    let Recorded {
        seeded,
        frames,
        log,
        ..
    } = run;
    for cut in 0..=frames.len() {
        for resume in cut..=frames.len() {
            let mut client = Client::attached(seeded);
            for frame in &frames[..cut] {
                client.live(frame);
            }
            client.reattach(log, EPOCH);
            for frame in &frames[resume..] {
                // The server drops the lossy frames that were in flight
                // when an attach was served, because a
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
        }
    }
}

#[tokio::test]
async fn every_cut_and_resume_of_a_tool_turn_converges() {
    let run = scripted_tool_turn().await;
    let reference = uninterrupted(&run);
    let expected = reference.canonical();
    assert_no_dangling(&reference.chat);

    sweep(&run, &expected);
}

#[tokio::test]
async fn every_cut_and_resume_of_a_sub_agent_turn_converges() {
    let run = scripted_sub_agent_turn().await;
    let reference = uninterrupted(&run);
    let expected = reference.canonical();
    assert_no_dangling(&reference.chat);

    sweep(&run, &expected);
}

/// A host restart mints a fresh epoch, so the cursor the client offers is
/// stale and the whole log comes back under the new epoch. The client
/// drops everything it built under the old epoch and rebuilds from the
/// full backfill, which has to land on the same state.
#[tokio::test]
async fn an_attach_under_a_new_epoch_rebuilds_the_same_state() {
    for run in [scripted_tool_turn().await, scripted_sub_agent_turn().await] {
        let mut client = uninterrupted(&run);
        let expected = client.canonical();
        client.reattach(&run.log, "epoch-2");

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

#[tokio::test]
async fn a_reattach_across_a_retried_inference_gains_no_usage_row() {
    let run = scripted_retried_turn().await;
    let (frames, log) = (&run.frames, &run.log);

    // The fixture only measures something if the failed attempt really was
    // persisted: that entry is what a projection can over-derive from.
    let persisted_assistants = log
        .entries_in_order()
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.entry,
                aj_session::ConversationEntryKind::Message { ref message }
                    if matches!(message.as_stored_wire(), Some(aj_models::types::Message::Assistant(_)))
            )
        })
        .count();
    assert_eq!(
        persisted_assistants, 2,
        "the log has to hold the failed attempt beside the recovery, or a \
         projection deriving usage per assistant message cannot go wrong here",
    );

    // The live agent reports usage once: after the inference that completed.
    let live_usage = frames
        .iter()
        .filter(|tagged| matches!(tagged.event, AgentEvent::UsageUpdate { .. }))
        .count();
    assert_eq!(
        live_usage, 1,
        "a retried turn reports usage once, for the attempt that completed",
    );

    // The retry emits a "Retrying inference" notice with no durable origin,
    // which no backfill can hand over, so the tier for a client
    // that was disconnected is the convergent one. It masks that notice and
    // nothing else: usage rows stay under comparison, which is the point here.
    let expected = uninterrupted(&run).canonical().convergent();
    let mut rebuilt = Client::attached(&run.seeded);
    rebuilt.reattach(log, EPOCH);
    let rebuilt_state = rebuilt.canonical();

    // The harm is a usage row for an attempt that reported no usage, so name
    // the usage rows before comparing whole states: this says which message
    // each one is attached to, where the state comparison would only say that
    // two long values differ.
    let usage_rows = |state: &CanonicalState| -> Vec<Option<String>> {
        state
            .agent(AgentId::Main)
            .expect("main transcript")
            .entries
            .iter()
            .filter_map(|entry| match entry {
                CanonicalEntry::TurnUsage { source_entry, .. } => Some(source_entry.clone()),
                _ => None,
            })
            .collect()
    };
    let live_rows = usage_rows(&uninterrupted(&run).canonical());
    assert_eq!(
        usage_rows(&rebuilt_state),
        live_rows,
        "a backfill across the retry derives one usage row, attached to the \
         message that reported it, and none for the attempt that failed",
    );

    assert_convergent_eq(
        &rebuilt_state.convergent(),
        &expected,
        "full backfill across a retried inference",
    );
    assert_no_dangling(&rebuilt.chat);
}

/// The degenerate re-application: every event of the real projection,
/// durable frames included, applied a second time onto complete state.
///
/// This folds through `reduce` rather than through [`SessionClient`]
/// deliberately. The client's cursor invariant drops the durable frames of
/// entries it already applied, and that invariant is a de-duplication
/// optimization rather than the correctness mechanism.
/// Idempotent application is, so this pins the property the invariant is
/// not allowed to stand in for.
#[tokio::test]
async fn reapplying_the_whole_projected_suffix_changes_nothing() {
    for run in [scripted_tool_turn().await, scripted_sub_agent_turn().await] {
        let Recorded {
            seeded,
            frames,
            log,
            ..
        } = &run;
        let mut chat = ChatState::new(scripted_settings(), 200_000, Arc::new(Vec::new()));
        let mut life = AgentLifecycle::default();
        // What a client attached at creation folded: the seeded log's block,
        // then the turn live.
        for tagged in &project_suffix(seeded, None, &BTreeSet::new()).events {
            fold(&mut chat, &mut life, tagged);
        }
        for tagged in frames {
            fold(&mut chat, &mut life, tagged);
        }
        let before = CanonicalState::of_reduced(&chat, &life);
        let backfill = project_suffix(log, None, &BTreeSet::new());
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
    let Frame::Event { durability, .. } = event_frame(EPOCH, tagged) else {
        unreachable!()
    };
    let _ = reduce(chat, life, tagged.event.clone(), durability.as_ref());
}
