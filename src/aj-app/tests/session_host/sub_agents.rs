//! Sub-agents across hosts and head switches: a sub-agent is its stored
//! thread, and a host that holds no live agent for it builds one from
//! that. `host.shutdown()` stands in for a process ending.
//!
//! The claims are about what the model is sent and what is stored, so they
//! are asserted on the provider's request and the stored entries.

use super::interruptions::{Inference, command, held, held_over, next, resume};
use super::tasks::eventually;
use super::*;
use aj_models::types::{Message, UserContent as Content};

/// The text of every user and assistant message of a request, in order.
pub(super) fn said(inference: &Inference) -> Vec<String> {
    let text = |message: &Message| match message {
        Message::User(user) => Some(
            user.content
                .iter()
                .filter_map(|block| match block {
                    Content::Text(text) => Some(text.text.as_str()),
                    Content::Image(_) => None,
                })
                .collect::<String>(),
        ),
        Message::Assistant(assistant) => Some(
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
        ),
        Message::ToolResult(_) => None,
    };
    inference.context.messages.iter().filter_map(text).collect()
}

/// How many stored entries of `session` have the entry type `kind`.
fn stored_count(harness: &Harness, session: &str, kind: &str) -> usize {
    let stored = harness.stored(session);
    let records = stored.as_array().expect("records");
    records
        .iter()
        .filter(|record| record["type"] == kind)
        .count()
}

/// The main transcript a fresh attach shows.
async fn main_transcript(harness: &Harness, session: &str) -> serde_json::Value {
    let client = Client::attach(&harness.host, session).await;
    let state = client.canonical();
    serde_json::to_value(&state.agent(AgentId::Main).expect("main transcript").entries)
        .expect("entries serialize")
}

/// Wait for sub-agent `child`'s turn to end and for the host to have
/// stored that it did.
async fn sub_idle(harness: &Harness, client: &mut Client, child: usize) {
    let frames = frames_until(&mut client.stream, "the sub-agent's turn to end", |frame| {
        matches!(frame,
            Frame::Event { event, .. } if matches!(event.known(),
                Some(AgentEvent::AgentEnd { agent_id, .. }) if *agent_id == AgentId::Sub(child))
        )
    })
    .await;
    for frame in frames {
        let _ = client.client.apply(&mut client.chat, frame);
    }
    let session = client.client.session().to_string();
    bounded("the sub-agent's turn to be reaped", async {
        loop {
            let list = harness.host.sessions().await.expect("sessions");
            let row = list.sessions.iter().find(|entry| entry.id == session);
            if row.is_some_and(|entry| !entry.working) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// The bodies of the task notifications in the main agent's stored transcript.
async fn main_notices(harness: &Harness, session: &str) -> Vec<String> {
    let client = Client::attach(&harness.host, session).await;
    let state = client.canonical();
    let entries = &state.agent(AgentId::Main).expect("main transcript").entries;
    let notices = entries.iter().filter_map(|entry| match entry {
        aj_app::test_support::CanonicalEntry::TaskNotification { body, .. } => Some(body.clone()),
        _ => None,
    });
    notices.collect()
}

/// A session whose first turn ran a blocking sub-agent to its end, followed
/// by one more turn.
async fn delegated() -> (Harness, String) {
    let mut script = sub_agent_turn();
    script.push(finalized_text_message("and that was that"));
    let harness = Harness::new(script);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    harness.prompt(&session, "anything else?").await;
    client.pump_until_idle().await;
    (harness, session)
}

/// Append a compaction and enough records after it that a reopen's window
/// starts after everything the log holds now.
fn later_compacted(log: &mut aj_session::SessionLog) {
    let later = aj_session::LogEntryKind::Message {
        message: aj_agent::message::AgentMessage::wire(Message::User(
            aj_models::types::UserMessage::text("much later"),
        )),
    };
    let head = log.head().cloned();
    let kept = log.append(head, aj_session::ThreadKind::Main, None, later);
    let kept = kept.expect("append a message").id;
    let usage = Default::default();
    log.append_compaction(
        ThreadFilter::MAIN,
        "SUMMARY".into(),
        kept,
        vec![],
        1,
        None,
        usage,
    )
    .expect("append a compaction");
    for index in 0..=aj_session::DISPLAY_TAIL {
        let level = if index % 2 == 0 { "low" } else { "off" };
        log.append_thinking_change(ThreadFilter::MAIN, level)
            .expect("append a settings record");
    }
}

/// A sub-agent that finished under one host is prompted under the next. It
/// answers from its own thread, read from the store even when the
/// reopen holds none of it, under its own cache key and the session's
/// prompt. Its box stays closed: the continuation writes no second spawn or
/// end and tells the parent nothing.
#[tokio::test]
async fn a_finished_sub_agent_is_prompted_after_a_reopen() {
    let (harness, session) = delegated().await;
    {
        let handles = harness.host.local_handles(&session).await.unwrap();
        later_compacted(&mut *handles.log.lock().await);
    }
    harness.host.shutdown().await;

    let (revived, mut requests) = held_over(&harness);
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert!(handles.registry.ids().is_empty(), "nothing is built yet");
    let system_prompt = {
        let log = handles.log.lock().await;
        let leaf = log.latest_leaf(ThreadFilter::sub(1)).expect("the run");
        assert!(!log.contains(&leaf), "the reopen holds none of the run");
        log.system_prompt().expect("a system prompt").to_string()
    };
    let before = main_transcript(&revived, &session).await;

    let mut client = Client::attach(&revived.host, &session).await;
    resume(&revived, &session, 1, "what did you find?").await;
    let inference = next(&mut requests).await;
    assert_eq!(
        said(&inference),
        [
            "look into it",
            "the sub found nothing",
            "what did you find?"
        ],
        "the request continues the sub-agent's own thread"
    );
    assert_eq!(
        inference.context.system_prompt.as_deref(),
        Some(system_prompt.as_str())
    );
    assert_eq!(inference.cache_key, Some(format!("{session}:sub:1")));
    let tools: Vec<_> = inference
        .context
        .tools
        .iter()
        .map(|tool| &tool.name)
        .collect();
    assert!(
        tools.iter().any(|name| *name == "bash") && !tools.iter().any(|name| *name == "agent"),
        "a sub-agent's tools: {tools:?}"
    );
    inference.answer(finalized_text_message("nothing, as I said"));
    sub_idle(&revived, &mut client, 1).await;
    assert_eq!(handles.registry.ids(), [1]);

    assert_eq!(stored_count(&harness, &session, "sub_agent_spawn"), 1);
    assert_eq!(stored_count(&harness, &session, "sub_agent_end"), 1);
    assert_eq!(main_transcript(&revived, &session).await, before);
    // The next prompt goes to the agent that was built.
    resume(&revived, &session, 1, "sure?").await;
    let inference = next(&mut requests).await;
    assert_eq!(said(&inference).len(), 5, "{:?}", said(&inference));
    inference.answer(finalized_text_message("sure"));
    sub_idle(&revived, &mut client, 1).await;
    revived.host.shutdown().await;
}

/// A head switch drops the live sub-agents. One the new branch does not
/// show is refused. Back on a branch that shows it, it is built from its
/// stored run and answers with its earlier context.
#[tokio::test]
async fn a_sub_agent_is_prompted_after_a_head_switch_away_and_back() {
    let (harness, session) = delegated().await;
    harness.host.shutdown().await;
    let (revived, mut requests) = held_over(&harness);
    let mut client = Client::attach(&revived.host, &session).await;
    // The agent is live before the switch, so the switch has one to drop.
    resume(&revived, &session, 1, "what did you find?").await;
    next(&mut requests)
        .await
        .answer(finalized_text_message("nothing, as I said"));
    sub_idle(&revived, &mut client, 1).await;
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1]);

    let (root, tip) = {
        let log = handles.log.lock().await;
        let root = log.system_prompt_id().cloned().expect("a root");
        (root, log.head().cloned().expect("a head"))
    };
    let head = |entry: &String| Command::Head {
        changes: Default::default(),
        target: HeadTarget::Entry(entry.clone()),
    };
    command(&revived, &session, head(&root)).await;
    assert!(handles.registry.ids().is_empty());
    let prompt = Command::Prompt {
        agent: AgentId::Sub(1),
        content: vec![UserContent::text("still there?")],
    };
    let refused = revived.host.command(&session, prompt).await;
    assert!(
        matches!(&refused, Err(HostError::Conflict { reason }) if reason.contains("no run on this branch")),
        "{refused:?}"
    );

    command(&revived, &session, head(&tip)).await;
    assert!(
        handles.registry.ids().is_empty(),
        "built on use, not on the switch"
    );
    let mut client = Client::attach(&revived.host, &session).await;
    resume(&revived, &session, 1, "still there?").await;
    let inference = next(&mut requests).await;
    assert_eq!(
        said(&inference),
        [
            "look into it",
            "the sub found nothing",
            "what did you find?",
            "nothing, as I said",
            "still there?"
        ]
    );
    inference.answer(finalized_text_message("yes"));
    sub_idle(&revived, &mut client, 1).await;
    assert_eq!(stored_count(&harness, &session, "sub_agent_end"), 1);
    revived.host.shutdown().await;
}

/// A settings change for a sub-agent of an earlier host is applied: the
/// sub-agent is built to take it.
#[tokio::test]
async fn a_setting_is_changed_for_a_sub_agent_of_an_earlier_host() {
    let (harness, session) = delegated().await;
    harness.host.shutdown().await;
    let revived = harness.revive(Vec::new());
    let change = Command::Settings(SettingsChange {
        agent: AgentId::Sub(1),
        persist: PersistAction::None,
        axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
    });
    command(&revived, &session, change).await;
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1]);
    revived.host.shutdown().await;
}

/// A message queued for a busy sub-agent is saved under the sub-agent's
/// thread. The next host puts it back without starting a turn, and
/// the sub-agent's next turn delivers it once.
#[tokio::test]
async fn a_sub_agents_queued_message_survives_a_reopen() {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let _client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    next(&mut requests).await.answer(calling(
        "delegating",
        "call-sub",
        "agent",
        serde_json::json!({"task": "look into it"}),
    ));
    // The sub-agent's first inference stays open, so it is busy.
    let _running = next(&mut requests).await;
    resume(&harness, &session, 1, "also check the logs").await;
    let queued = |handles: &aj_app::host::LocalHandles| handles.queues.snapshot(AgentId::Sub(1));
    let handles = harness.host.local_handles(&session).await.unwrap();
    assert_eq!(queued(&handles).text, "also check the logs");
    drop(handles);
    harness.host.shutdown().await;

    let (revived, mut requests) = held_over(&harness);
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(queued(&handles).text, "also check the logs");
    assert_eq!(handles.registry.ids(), [1], "built to receive it");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(requests.try_recv().is_err(), "the reopen starts no turn");

    let mut client = Client::attach(&revived.host, &session).await;
    resume(&revived, &session, 1, "go on").await;
    let inference = next(&mut requests).await;
    let said = said(&inference);
    // The prompt that starts the turn joins the restored message.
    let delivered = said
        .iter()
        .filter(|text| text.contains("also check the logs"));
    assert_eq!(delivered.count(), 1, "{said:?}");
    assert_eq!(
        said.last().map(String::as_str),
        Some("also check the logs\ngo on")
    );
    assert_eq!(said.first().map(String::as_str), Some("look into it"));
    inference.answer(finalized_text_message("checked"));
    sub_idle(&revived, &mut client, 1).await;
    assert_eq!(queued(&handles).text, "");
    drop(handles);
    revived.host.shutdown().await;

    let third = revived.revive(Vec::new());
    let handles = third.host.local_handles(&session).await.unwrap();
    assert_eq!(queued(&handles).text, "", "delivered once");
    drop(handles);
    third.host.shutdown().await;
}

/// End `harness`'s host as its process dying would: its drivers are aborted
/// where they stand and nothing is written afterwards. Returns a host over
/// the same store whose inferences the test answers, and what keeps the
/// store alive.
pub(super) async fn crashed(
    harness: Harness,
    session: &str,
) -> (
    Harness,
    tokio::sync::mpsc::UnboundedReceiver<Inference>,
    impl Sized,
) {
    let (revived, requests) = held_over(&harness);
    let Harness {
        host,
        persistence,
        _dir,
        _tasks,
        ..
    } = harness;
    drop(host);
    bounded("the dead host to let go of the session", async {
        loop {
            let lock = SessionLock::try_acquire(&persistence, session, "a-later-host");
            if lock.expect("try_acquire").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    (revived, requests, (_dir, _tasks))
}

/// The report on sub-agent `child`'s box in a fresh attach, with its status.
async fn concluded(harness: &Harness, session: &str, child: usize) -> (String, String) {
    let client = Client::attach(&harness.host, session).await;
    let state = client.canonical();
    let main = &state.agent(AgentId::Main).expect("main transcript").entries;
    let found = main.iter().find_map(|entry| match entry {
        aj_app::test_support::CanonicalEntry::SubAgent {
            child: n,
            status,
            report,
            ..
        } if *n == child => Some((format!("{status:?}"), report.clone().unwrap_or_default())),
        _ => None,
    });
    found.expect("the sub-agent's box")
}

/// A session whose main turn started a background sub-agent and ended. The
/// sub-agent's first inference, returned, is still open.
async fn background_run() -> (Harness, String, Inference) {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "kick it off").await;
    next(&mut requests).await.answer(calling(
        "kicking that off",
        "call-bg",
        "agent",
        serde_json::json!({"task": "look into it", "run_in_background": true}),
    ));
    // The parent's next inference and the sub-agent's first, in either
    // order. The parent's turn ends and the sub-agent's stays open.
    let (first, second) = (next(&mut requests).await, next(&mut requests).await);
    let parents = |inference: &Inference| said(inference).contains(&"kick it off".to_string());
    let (parent, running) = if parents(&first) {
        (first, second)
    } else {
        (second, first)
    };
    parent.answer(finalized_text_message("it is running"));
    client.pump_until_idle().await;
    assert_eq!(stored_count(&harness, &session, "sub_agent_end"), 0);
    (harness, session, running)
}

/// The sub-agents the store says an earlier process left work for, read
/// while no host holds the session.
fn left(harness: &Harness, session: &str) -> Vec<aj_session::LeftSubAgent> {
    let log = aj_session::SessionLog::resume(&harness.persistence, session);
    let left = log.expect("read the session").sub_agents_with_work();
    left.expect("read what was left")
}

/// Open `session` on `harness`'s host and assert that the open built no
/// sub-agent and left none with a turn stored as working or other work.
async fn opens_with_nothing_to_pick_up(harness: &Harness, session: &str) {
    let handles = harness.host.local_handles(session).await.unwrap();
    assert!(handles.registry.ids().is_empty(), "built on first use");
    let mut log = handles.log.lock().await;
    assert_eq!(log.sub_agents_with_work().unwrap(), []);
    assert_eq!(log.pick_up_turn(1).unwrap(), aj_session::TurnState::Idle);
}

/// Prompt sub-agent 1, which `harness`'s host holds no agent for, and
/// answer it. It is built to take the prompt and continues its run.
async fn takes_a_prompt(
    harness: &Harness,
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<Inference>,
    session: &str,
) {
    let mut client = Client::attach(&harness.host, session).await;
    resume(harness, session, 1, "carry on").await;
    let inference = next(requests).await;
    let said = said(&inference);
    assert_eq!(said.first().map(String::as_str), Some("look into it"));
    assert_eq!(said.last().map(String::as_str), Some("carry on"));
    inference.answer(finalized_text_message("found it"));
    sub_idle(harness, &mut client, 1).await;
    let handles = harness.host.local_handles(session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1]);
    assert_eq!(stored_count(harness, session, "sub_agent_end"), 1);
}

/// A background run whose process died is closed by the next host: one end
/// record says it was interrupted, and its parent gets one notice saying it
/// can be continued. Neither is repeated by a later open, nor by an open
/// that ends between saving the notice and appending the record.
#[tokio::test]
async fn a_background_run_left_working_is_closed_and_its_parent_told_once() {
    let (harness, session, _running) = background_run().await;
    let persistence = harness.persistence.clone();
    let pending = |harness: &Harness| {
        let (host, session) = (harness.host.clone(), session.clone());
        async move {
            let handles = host.local_handles(&session).await.expect("live session");
            handles.task_registry.pending_notices(AgentId::Main)
        }
    };

    // An open that ends after the notice is saved and before the run is
    // closed, then one that gets through.
    let (revived, mut requests, _store) = crashed(harness, &session).await;
    persistence.refuse_sqlite_entry_writes(true);
    let refused = revived.host.local_handles(&session).await;
    assert!(refused.is_err(), "the open could not close the run");
    persistence.refuse_sqlite_entry_writes(false);
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 0);

    assert_eq!(pending(&revived).await, 1);
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    let (status, report) = concluded(&revived, &session, 1).await;
    assert_eq!(status, "Failed");
    assert!(report.contains("interrupted"), "{report}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(requests.try_recv().is_err(), "the open starts no turn");
    revived.host.shutdown().await;

    // A second open closes nothing and has the same one notice.
    let (again, mut requests) = held_over(&revived);
    assert_eq!(pending(&again).await, 1);
    assert_eq!(stored_count(&again, &session, "sub_agent_end"), 1);
    let mut client = Client::attach(&again.host, &session).await;
    command(&again, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    let told: Vec<_> = said(&inference)
        .into_iter()
        .filter(|text| text.contains("Background agent 1 was interrupted"))
        .collect();
    assert_eq!(told.len(), 1, "{:?}", said(&inference));
    assert!(told[0].contains("can be continued by sending it a message"));
    inference.answer(finalized_text_message("noted"));
    client.pump_until_idle().await;
    assert_eq!(pending(&again).await, 0);
    again.host.shutdown().await;

    let third = again.revive(Vec::new());
    assert_eq!(pending(&third).await, 0, "delivered once");
    assert_eq!(stored_count(&third, &session, "sub_agent_end"), 1);
    let notices = main_notices(&third, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    third.host.shutdown().await;
}

/// How many notices wait for the main agent of `session` on `harness`'s
/// host, which opens the session if it has not.
async fn pending_notices(harness: &Harness, session: &str) -> usize {
    let handles = harness.host.local_handles(session).await;
    let handles = handles.expect("live session");
    handles.task_registry.pending_notices(AgentId::Main)
}

/// A background run that finishes just before its host dies has told its
/// parent once: the task notification is saved before the end record, so
/// the next host finds the report among the parent's pending work and
/// neither loses it nor adds an "interrupted" notice to it.
#[tokio::test]
async fn a_background_run_that_ended_as_its_host_died_tells_its_parent_once() {
    let (harness, session, running) = background_run().await;
    let announced = |harness: &Harness| {
        let rows = harness.persistence.stored_rows(&session);
        let queued = rows.into_iter().filter(|row| row.starts_with("pending"));
        queued.collect::<Vec<_>>()
    };
    let waiting = announced(&harness);
    assert_eq!(waiting.len(), 1, "the run announced its loss: {waiting:?}");
    assert!(waiting[0].contains("was interrupted"), "{waiting:?}");

    // The parent cannot take the notice: its wake waits for the agent. So
    // the host dies with the run ended and the notice undelivered.
    let handles = harness.host.local_handles(&session).await.unwrap();
    let parent = Arc::clone(&handles.agent).lock_owned().await;
    running.answer(finalized_text_message("the report"));
    eventually("the run's end to be stored", || {
        stored_count(&harness, &session, "sub_agent_end") == 1
    })
    .await;
    let waiting = announced(&harness);
    assert_eq!(waiting.len(), 1, "one notice under one id: {waiting:?}");
    assert!(waiting[0].contains("the report"), "{waiting:?}");
    let (revived, mut requests, _store) = crashed(harness, &session).await;
    drop((parent, handles));

    assert_eq!(pending_notices(&revived, &session).await, 1);
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    let mut client = Client::attach(&revived.host, &session).await;
    command(&revived, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    let told: Vec<_> = said(&inference)
        .into_iter()
        .filter(|text| text.contains("agent 1"))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("the report"), "{told:?}");
    assert!(!told[0].contains("interrupted"), "{told:?}");
    inference.answer(finalized_text_message("noted"));
    client.pump_until_idle().await;
    revived.host.shutdown().await;

    let third = revived.revive(Vec::new());
    assert_eq!(pending_notices(&third, &session).await, 0, "delivered once");
    assert_eq!(main_notices(&third, &session).await.len(), 1);
    assert_eq!(stored_count(&third, &session, "sub_agent_end"), 1);
    third.host.shutdown().await;
}

/// A host that dies between saving a run's task notification and appending
/// its end record leaves the report, not the announcement. The next open
/// closes the run without a second notice and keeps the report.
#[tokio::test]
async fn a_run_whose_completion_was_saved_without_its_end_keeps_the_report() {
    let (harness, session, _running) = background_run().await;
    let (revived, _requests, _store) = crashed(harness, &session).await;
    {
        // What the run's driver saves first when the run ends.
        let log = aj_session::SessionLog::resume(&revived.persistence, &session).unwrap();
        let mut report = aj_agent::message::AgentMessage::task_notification(
            aj_agent::message::TaskNotification::new(
                "agent 1".into(),
                aj_agent::message::TaskNotificationKind::Agent,
                aj_agent::message::TaskOutcome::Succeeded,
                "Background task #1 finished: agent 1 — the report".into(),
            ),
        );
        report.set_id(aj_agent::interrupted_run_notice(1).id().to_owned());
        let saved = log.pending().save_task_notification(AgentId::Main, &report);
        saved.expect("save the completion");
    }

    assert_eq!(pending_notices(&revived, &session).await, 1);
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    let (_, report) = concluded(&revived, &session, 1).await;
    assert!(report.contains("sub-agent finished"), "{report}");
    let rows = revived.persistence.stored_rows(&session);
    let queued: Vec<_> = rows
        .iter()
        .filter(|row| row.starts_with("pending"))
        .collect();
    assert_eq!(queued.len(), 1, "{rows:?}");
    assert!(queued[0].contains("the report"), "{rows:?}");
    revived.host.shutdown().await;
}

/// A foreground run whose process died is closed by the next host too. Its
/// parent's tool call is answered with what happened, and the sub-agent
/// takes a prompt and continues its thread.
#[tokio::test]
async fn a_foreground_run_left_working_answers_its_call_and_takes_a_prompt() {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let _client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    next(&mut requests).await.answer(calling(
        "delegating",
        "call-sub",
        "agent",
        serde_json::json!({"task": "look into it"}),
    ));
    // The sub-agent's first inference stays open.
    let _running = next(&mut requests).await;

    let (revived, mut requests, _store) = crashed(harness, &session).await;
    let handles = revived.host.local_handles(&session).await.unwrap();
    // Every stored answer to the parent's call.
    let answers = |harness: &Harness| -> Vec<String> {
        let stored = harness.stored(&session);
        let records = stored.as_array().expect("records").iter();
        records
            .map(|record| record.to_string())
            .filter(|record| record.contains(r#""tool_call_id":"call-sub""#))
            .collect()
    };
    let answer = answers(&revived);
    assert_eq!(answer.len(), 1, "{answer:?}");
    assert!(
        answer[0].contains("Sub-agent 1 was interrupted")
            && answer[0].contains("can be continued by sending it a message"),
        "{answer:?}"
    );
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 0);
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    let (status, report) = concluded(&revived, &session, 1).await;
    assert_eq!(status, "Failed");
    assert!(report.contains("interrupted"), "{report}");

    let mut client = Client::attach(&revived.host, &session).await;
    resume(&revived, &session, 1, "carry on").await;
    let inference = next(&mut requests).await;
    assert_eq!(said(&inference), ["look into it", "carry on"]);
    inference.answer(finalized_text_message("found it"));
    sub_idle(&revived, &mut client, 1).await;
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    drop(handles);
    revived.host.shutdown().await;

    // The turn the sub-agent ran since ended, so nothing is left to close.
    let third = revived.revive(Vec::new());
    let handles = third.host.local_handles(&session).await.unwrap();
    assert_eq!(answers(&third).len(), 1, "answered once");
    assert_eq!(stored_count(&third, &session, "sub_agent_end"), 1);
    assert!(handles.registry.ids().is_empty(), "nothing to pick up");
    drop(handles);
    third.host.shutdown().await;
}

/// A run an open closed is not picked up again. The open that closes it
/// stores its turn idle, so that open and every later one build no agent
/// for it, and the sub-agent is built when it is prompted.
#[tokio::test]
async fn a_closed_run_is_stored_idle_and_built_on_first_use() {
    let (harness, session, _running) = background_run().await;
    let (revived, mut requests, _store) = crashed(harness, &session).await;
    assert_eq!(
        left(&revived, &session),
        [aj_session::LeftSubAgent {
            agent_id: 1,
            working: true
        }],
        "the dead process left the run working"
    );
    opens_with_nothing_to_pick_up(&revived, &session).await;
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    // The parent is told, so nothing of the close is left to deliver.
    let mut client = Client::attach(&revived.host, &session).await;
    command(&revived, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    assert!(
        said(&inference)
            .iter()
            .any(|text| text.contains("Background agent 1 was interrupted")),
        "{:?}",
        said(&inference)
    );
    inference.answer(finalized_text_message("noted"));
    client.pump_until_idle().await;
    revived.host.shutdown().await;

    assert_eq!(left(&revived, &session), []);
    let (again, mut requests) = held_over(&revived);
    opens_with_nothing_to_pick_up(&again, &session).await;
    takes_a_prompt(&again, &mut requests, &session).await;
    again.host.shutdown().await;
}

/// A host that shuts down ends a running background run as a process that
/// died does: the run records no end and no notice and stays stored as
/// working. The next open closes it as interrupted and tells its parent
/// once, and no open builds its sub-agent before it is prompted.
#[tokio::test]
async fn a_run_a_shutdown_stopped_is_closed_as_interrupted_by_the_next_open() {
    let (harness, session, _running) = background_run().await;
    harness.host.shutdown().await;
    assert_eq!(stored_count(&harness, &session, "sub_agent_end"), 0);
    assert_eq!(
        left(&harness, &session),
        [aj_session::LeftSubAgent {
            agent_id: 1,
            working: true
        }],
        "the shutdown left the run working"
    );

    let (revived, mut requests) = held_over(&harness);
    opens_with_nothing_to_pick_up(&revived, &session).await;
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 1);
    let (status, report) = concluded(&revived, &session, 1).await;
    assert_eq!(status, "Failed");
    assert!(report.contains("interrupted"), "{report}");
    let mut client = Client::attach(&revived.host, &session).await;
    command(&revived, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    // Every notice about the run, a `killed` one included.
    let told: Vec<_> = said(&inference)
        .into_iter()
        .filter(|text| text.contains("agent 1"))
        .collect();
    assert_eq!(told.len(), 1, "{:?}", said(&inference));
    assert!(
        told[0].contains("Background agent 1 was interrupted")
            && told[0].contains("can be continued by sending it a message"),
        "{told:?}"
    );
    inference.answer(finalized_text_message("noted"));
    client.pump_until_idle().await;
    revived.host.shutdown().await;

    let (again, mut requests) = held_over(&revived);
    opens_with_nothing_to_pick_up(&again, &session).await;
    assert_eq!(stored_count(&again, &session, "sub_agent_end"), 1);
    let notices = main_notices(&again, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    takes_a_prompt(&again, &mut requests, &session).await;
    again.host.shutdown().await;
}

/// A sub-agent whose stored run cannot be read does not fail the open. The
/// session opens without it, its first client is told once, and a healthy
/// sub-agent that left a recorded task is restored with it.
#[tokio::test]
async fn an_open_skips_a_damaged_sub_agent_and_restores_the_others() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let harness = Harness::new(vec![
        calling(
            "delegating",
            "call-first",
            "agent",
            serde_json::json!({"task": "look into it"}),
        ),
        finalized_text_message("the first found nothing"),
        calling(
            "delegating again",
            "call-second",
            "agent",
            serde_json::json!({"task": "run it"}),
        ),
        calling(
            "backgrounding it",
            "call-bash",
            "bash",
            serde_json::json!({
                "command": super::tasks::gated(&gate),
                "run_in_background": true,
                "description": "background"
            }),
        ),
        finalized_text_message("the second started it"),
        finalized_text_message("both delegated"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it twice").await;
    client.pump_until_idle().await;
    let recorded = harness.persistence.recorded_tasks(&session);
    let [(task, _)] = recorded[..] else {
        panic!("one task: {recorded:?}");
    };
    // A reopen's window starts after the runs: a damaged record the window
    // holds fails the read of the session itself.
    {
        let handles = harness.host.local_handles(&session).await.unwrap();
        later_compacted(&mut *handles.log.lock().await);
    }
    harness.host.shutdown().await;
    harness.persistence.damage_sub_agent(&session, 1);
    let working = |left: &aj_session::LeftSubAgent| (left.agent_id, left.working);
    assert_eq!(
        left(&harness, &session)
            .iter()
            .map(working)
            .collect::<Vec<_>>(),
        [(1, true), (2, false)],
        "the open has both sub-agents to look at"
    );

    // The wake the task's end starts on the second sub-agent.
    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let mut stream = revived
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("the session opens");
    let block = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert!(
        !block
            .iter()
            .any(|frame| matches!(frame, Frame::Error { .. })),
        "{block:?}"
    );
    // The notice is released after the block.
    let unrestored = |frame: &Frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(),
            Some(AgentEvent::Notice { text, .. })
                if text.contains("Sub-agent 1 could not be restored")))
    };
    frames_until(&mut stream, "the notice", unrestored).await;

    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [2], "the healthy one is live");
    let tasks = super::tasks::registry(&revived, &session).await;
    let summary = tasks.summary(task).expect("its task is tracked again");
    assert_eq!(summary.status, TaskStatus::Running);
    assert_eq!(summary.owner, AgentId::Sub(2));
    // A prompt to the damaged one is refused and the session stays up.
    let prompt = Command::Prompt {
        agent: AgentId::Sub(1),
        content: vec![UserContent::text("still there?")],
    };
    let refused = revived.host.command(&session, prompt).await;
    assert!(
        matches!(refused, Err(HostError::Internal(_))),
        "{refused:?}"
    );

    std::fs::write(&gate, "").unwrap();
    let ended = bounded("the task to end", tasks.wait_terminal(task)).await;
    assert_eq!(ended, Some(TaskStatus::Exited(Some(0))));
    super::tasks::quiet(&revived, &session).await;
    drop(handles);
    revived.host.shutdown().await;

    // Told once: the rest of the stream, up to the shutdown, repeats nothing.
    while let Some(frame) = bounded("the stream to close", stream.recv()).await {
        assert!(!unrestored(&frame), "{frame:?}");
    }
}

/// A run dropped with its parent's cancelled turn has no end record, and
/// its turn ended in its process. The next open picks up the work it left
/// and does not close it as interrupted or tell its parent so.
#[tokio::test]
async fn a_run_dropped_with_its_parents_turn_is_not_closed_as_interrupted() {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    next(&mut requests).await.answer(calling(
        "delegating",
        "call-sub",
        "agent",
        serde_json::json!({"task": "look into it"}),
    ));
    let _running = next(&mut requests).await;
    // Pending work is what has the next open look at the run at all.
    resume(&harness, &session, 1, "also check the logs").await;
    let cancel = Command::Cancel {
        agent: AgentId::Main,
    };
    command(&harness, &session, cancel).await;
    client.pump_until_idle().await;
    harness.host.shutdown().await;
    assert_eq!(stored_count(&harness, &session, "sub_agent_end"), 0);
    assert_eq!(
        left(&harness, &session),
        [aj_session::LeftSubAgent {
            agent_id: 1,
            working: false
        }],
        "the run left work, and its turn ended in its process"
    );

    let revived = harness.revive(Vec::new());
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1], "built to receive its work");
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 0);
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 0);
    drop(handles);
    revived.host.shutdown().await;
}

/// A turn the user starts on a sub-agent whose run ended is stored as
/// working while it runs, so a host that dies during it leaves the
/// sub-agent's thread working. The resume policy treats it as it
/// treats the main agent's: the next open builds the sub-agent, tells the
/// user, starts nothing, and `continue` resumes the turn. No second end
/// record is written and the parent hears nothing.
#[tokio::test]
async fn a_sub_agents_interrupted_turn_waits_for_continue_as_the_main_agents_does() {
    let (harness, session) = delegated().await;
    harness.host.shutdown().await;
    assert_eq!(left(&harness, &session), []);
    let (revived, mut requests) = held_over(&harness);
    resume(&revived, &session, 1, "what did you find?").await;
    let _running = next(&mut requests).await;

    let (third, mut requests, _store) = crashed(revived, &session).await;
    let working = [aj_session::LeftSubAgent {
        agent_id: 1,
        working: true,
    }];
    assert_eq!(left(&third, &session), working);
    let mut stream = third
        .host
        .attach(&[attach_request(&session)])
        .await
        .unwrap();
    let waits = |frame: &Frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(),
            Some(AgentEvent::Notice { text, .. })
                if text.contains("Sub-agent 1's last turn did not finish")))
    };
    frames_until(&mut stream, "the notice", waits).await;
    let handles = third.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1], "built to be continued");
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 0);
    drop(handles);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(requests.try_recv().is_err(), "the open starts no turn");
    // The turn stays stored as working, which is what an open reads.
    third.host.shutdown().await;
    assert_eq!(left(&third, &session), working);

    let (fourth, mut requests) = held_over(&third);
    let mut client = Client::attach(&fourth.host, &session).await;
    command(&fourth, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    let said = said(&inference);
    assert_eq!(said.first().map(String::as_str), Some("look into it"));
    assert!(said.contains(&"what did you find?".to_string()), "{said:?}");
    inference.answer(finalized_text_message("found it"));
    sub_idle(&fourth, &mut client, 1).await;
    let again = fourth.host.command(&session, Command::Continue).await;
    assert!(again.is_err(), "nothing is left to continue");
    assert_eq!(stored_count(&fourth, &session, "sub_agent_end"), 1);
    assert!(main_notices(&fourth, &session).await.is_empty());
    fourth.host.shutdown().await;
    assert_eq!(left(&fourth, &session), []);
}

/// A tool call the dead host left open on a sub-agent's thread is
/// answered by the next open, so the sub-agent it builds sends the model
/// the call with a result.
#[tokio::test]
async fn a_run_stored_idle_has_its_open_tool_call_answered() {
    let (harness, session) = delegated().await;
    harness.host.shutdown().await;
    let (revived, mut requests) = held_over(&harness);
    resume(&revived, &session, 1, "what did you find?").await;
    let _running = next(&mut requests).await;
    // The process dies after the model's call is stored and before its
    // result is.
    {
        let handles = revived.host.local_handles(&session).await.unwrap();
        let mut log = handles.log.lock().await;
        let leaf = log.latest_leaf(ThreadFilter::sub(1));
        let call = calling("reading", "call-open", "read_file", serde_json::json!({}));
        let call = aj_session::LogEntryKind::Message {
            message: aj_agent::message::AgentMessage::wire(Message::Assistant(call)),
        };
        log.append(leaf, aj_session::ThreadKind::Sub, Some(1), call)
            .expect("append the call");
    }
    let (third, mut requests, _store) = crashed(revived, &session).await;
    let handles = third.host.local_handles(&session).await.unwrap();
    assert_eq!(
        handles.registry.ids(),
        [1],
        "its turn waits to be continued"
    );
    drop(handles);

    let mut client = Client::attach(&third.host, &session).await;
    resume(&third, &session, 1, "carry on").await;
    let inference = next(&mut requests).await;
    let answered = inference.context.messages.iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.tool_call_id == "call-open")
    });
    assert!(answered, "{:?}", inference.context.messages);
    inference.answer(finalized_text_message("found it"));
    sub_idle(&third, &mut client, 1).await;
    third.host.shutdown().await;
}

/// A wake that finds nothing to deliver starts no turn, so it stores
/// nothing: not that a turn started, and not that one ended. The same wake
/// with a notice to deliver runs a turn and stores both.
#[tokio::test]
async fn a_wake_with_nothing_to_deliver_writes_nothing() {
    let mut script = sub_agent_turn();
    script.push(finalized_text_message("noted"));
    let harness = Harness::new(script);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    let handles = harness.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1], "a live sub-agent to wake");

    // A task's end wakes its owner, whether or not it left a notice.
    let wake = |owner: AgentId| {
        let (agent, host, session) = (Arc::clone(&handles.agent), &harness.host, &session);
        async move {
            let ended = AgentEvent::TaskEnd {
                agent_id: owner,
                task_id: 99,
                call_id: "call-none".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "nothing".into(),
            };
            agent.lock().await.emit_event(ended).await.expect("emit");
            // A command is answered after the events before it are folded.
            let folded = host.command(session, Command::Queue(QueueOp::Clear)).await;
            folded.expect("a command");
        }
    };
    let writes = || harness.persistence.committed_writes();
    let before = writes();
    for owner in [AgentId::Main, AgentId::Sub(1)] {
        wake(owner).await;
        super::tasks::quiet(&harness, &session).await;
    }
    assert_eq!(writes(), before, "a wake that started no turn wrote");

    let notice = aj_agent::message::AgentMessage::task_notification(
        aj_agent::message::TaskNotification::new(
            "make".into(),
            aj_agent::message::TaskNotificationKind::Bash,
            aj_agent::message::TaskOutcome::Succeeded,
            "exit 0".into(),
        ),
    );
    handles.task_registry.restore_notice(AgentId::Main, notice);
    wake(AgentId::Main).await;
    super::tasks::quiet(&harness, &session).await;
    // The turn's start, the notice, the answer and the turn's end.
    assert_eq!(writes(), before + 4);
    assert_eq!(main_notices(&harness, &session).await.len(), 1);
    drop(handles);
    harness.host.shutdown().await;
}

/// Two parallel `agent` calls with the same task spawn two sub-agents. The
/// second finishes while the first still runs, and the process dies before
/// either call has its result. The next host answers each call for the
/// sub-agent that call spawned: the one whose run ended gets its report,
/// and the other is told its sub-agent was interrupted.
#[tokio::test]
async fn a_spawning_call_is_answered_for_the_sub_agent_it_spawned() {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it twice").await;
    let task = serde_json::json!({"task": "look into it"});
    let mut twice = calling("delegating", "call-a", "agent", task.clone());
    let again = calling("", "call-b", "agent", task);
    let calls = again.content.into_iter();
    twice
        .content
        .extend(calls.filter(|block| matches!(block, AssistantContent::ToolCall(_))));
    next(&mut requests).await.answer(twice);

    // Both sub-agents' first inferences, told apart by their cache keys.
    let (one, other) = (next(&mut requests).await, next(&mut requests).await);
    let of = |n: usize| Some(format!("{session}:sub:{n}"));
    let (first, second) = if one.cache_key == of(1) {
        (one, other)
    } else {
        (other, one)
    };
    assert_eq!((&first.cache_key, &second.cache_key), (&of(1), &of(2)));
    second.answer(finalized_text_message("the second found it"));
    let ended = |frame: &Frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(),
            Some(AgentEvent::SubAgentEnd { child: AgentId::Sub(2), .. })))
    };
    let frames = frames_until(&mut client.stream, "the second run to end", ended).await;
    // Which call spawned which sub-agent, as the spawning calls said live.
    let spawned_by: BTreeMap<usize, String> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event { event, .. } => match event.known() {
                Some(AgentEvent::SubAgentStart {
                    child: AgentId::Sub(n),
                    call_id: Some(call),
                    ..
                }) => Some((*n, call.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let mut calls: Vec<&str> = spawned_by.values().map(String::as_str).collect();
    calls.sort_unstable();
    assert_eq!(calls, ["call-a", "call-b"], "{spawned_by:?}");

    let (revived, _requests, _store) = crashed(harness, &session).await;
    let handles = revived.host.local_handles(&session).await.unwrap();
    let answer = |call: &str| -> String {
        let stored = revived.stored(&session);
        let records = stored.as_array().expect("records").iter();
        let answers: Vec<String> = records
            .map(|record| record.to_string())
            .filter(|record| record.contains(&format!(r#""tool_call_id":"{call}""#)))
            .collect();
        assert_eq!(answers.len(), 1, "{answers:?}");
        answers[0].clone()
    };
    let finished = answer(&spawned_by[&2]);
    assert!(
        finished.contains("Sub-agent 2 ended its run")
            && finished.contains("the second found it")
            && !finished.contains("interrupted")
            && !finished.contains("Sub-agent 1"),
        "{finished}"
    );
    let interrupted = answer(&spawned_by[&1]);
    assert!(
        interrupted.contains("Sub-agent 1 was interrupted")
            && interrupted.contains("can be continued by sending it a message")
            && !interrupted.contains("Sub-agent 2"),
        "{interrupted}"
    );
    assert_eq!(stored_count(&revived, &session, "sub_agent_end"), 2);
    drop((handles, first));
    revived.host.shutdown().await;
}
