//! How a prompt is accepted: saved as its agent's pending input before the
//! command is answered, then taken by a turn, whether the agent was idle or
//! busy.
//!
//! The claims are about what the store holds when the command answers and
//! what a later host does with it, so they are asserted on the store, on
//! the provider's requests and on the frames a client gets.

use super::interruptions::{Inference, command, held, held_over, next};
use super::sub_agents::{crashed, said};
use super::*;
use aj_agent::queue::PendingKind;
use aj_models::types::Message;
use aj_session::Pending;
use tokio::sync::mpsc::UnboundedReceiver;

/// A held host with one finished turn, so the session is stored and its
/// main agent is idle on a thread.
async fn idle_after_a_turn() -> (Harness, UnboundedReceiver<Inference>, String) {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    next(&mut requests)
        .await
        .answer(finalized_text_message("done"));
    client.pump_until_idle().await;
    (harness, requests, session)
}

/// The main agent's input as the store holds it, read from the database
/// and not from the queue in memory.
async fn saved_input(harness: &Harness, session: &str) -> Vec<Vec<UserContent>> {
    let handles = harness.host.local_handles(session).await.unwrap();
    let log = handles.log.lock().await;
    let saved = log.restore_pending(&log.pending(), AgentId::Main);
    let input = |work| match work {
        Pending::Input(_, _, input) => Some(input.content),
        Pending::TaskNotification(..) => None,
    };
    saved.unwrap().into_iter().filter_map(input).collect()
}

/// The text of every stored user message of the main thread.
fn stored_prompts(harness: &Harness, session: &str) -> Vec<String> {
    let stored = harness.stored(session);
    let records = stored.as_array().expect("records").iter();
    records
        .filter(|record| record["message"]["role"] == "user")
        .map(|record| {
            record["message"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn queue_updates(frames: &[Frame]) -> usize {
    let update = |event: &&&AgentEvent| matches!(event, AgentEvent::QueueUpdate { .. });
    events(frames).iter().filter(update).count()
}

async fn no_turn_starts(requests: &mut UnboundedReceiver<Inference>) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(requests.try_recv().is_err(), "no turn starts");
}

/// A prompt to an idle agent is acknowledged only once it is saved. A save
/// the store refuses refuses the command, and nothing starts: no turn, no
/// queued message, no entry.
#[tokio::test]
async fn an_idle_prompt_is_saved_before_it_is_acknowledged() {
    let (harness, mut requests, session) = idle_after_a_turn().await;
    let mut client = Client::attach(&harness.host, &session).await;

    harness.persistence.refuse_sqlite_pending_writes();
    let refused = harness.host.command(&session, prompt("not saved")).await;
    let reason = refused.expect_err("the save was refused").to_string();
    assert!(reason.contains("the prompt could not be saved"), "{reason}");
    no_turn_starts(&mut requests).await;
    harness.persistence.allow_sqlite_pending_writes();
    assert!(saved_input(&harness, &session).await.is_empty());
    assert_eq!(stored_prompts(&harness, &session), ["first"]);
    let handles = harness.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.queues.snapshot(AgentId::Main).kind, None);
    let frames = client.drain_into_fold();
    assert!(events(&frames).is_empty(), "nothing happened: {frames:?}");
    assert!(!client.client.working());

    // An accepted one is in the store when the command answers. Its turn
    // is held back here, so the store is read before the turn can append.
    let turn = Arc::clone(&handles.agent).lock_owned().await;
    harness.prompt(&session, "saved").await;
    assert_eq!(
        saved_input(&harness, &session).await,
        [vec![UserContent::text("saved")]]
    );
    assert_eq!(stored_prompts(&harness, &session), ["first"]);

    // The turn takes it, and its message removes the saved copy. A client
    // sees a transcript message and never a queued one.
    drop(turn);
    let inference = next(&mut requests).await;
    assert_eq!(said(&inference).last().map(String::as_str), Some("saved"));
    inference.answer(finalized_text_message("done"));
    let frames = client.pump_until_idle().await;
    let queued = |event: &&&AgentEvent| {
        matches!(event, AgentEvent::QueueUpdate { steering, follow_up, .. }
            if !steering.is_empty() || !follow_up.is_empty())
    };
    assert_eq!(events(&frames).iter().filter(queued).count(), 0);
    assert!(saved_input(&harness, &session).await.is_empty());
    assert_eq!(stored_prompts(&harness, &session), ["first", "saved"]);
    drop(handles);
    harness.host.shutdown().await;
}

/// A host that dies after it acknowledged a prompt to an idle agent, and
/// before the turn appended anything, leaves the prompt pending. The next
/// open restores it without starting a turn, Continue delivers it, and it
/// is delivered once.
async fn an_acknowledged_idle_prompt_survives_a_crash(content: Vec<UserContent>) {
    let (harness, _requests, session) = idle_after_a_turn().await;
    let handles = harness.host.local_handles(&session).await.unwrap();
    // The turn waits for the agent, so it has appended nothing when the
    // host dies.
    let turn = Arc::clone(&handles.agent).lock_owned().await;
    let sent = Command::Prompt {
        agent: AgentId::Main,
        content: content.clone(),
    };
    command(&harness, &session, sent).await;
    assert_eq!(
        saved_input(&harness, &session).await,
        std::slice::from_ref(&content)
    );
    let (revived, mut requests, _store) = crashed(harness, &session).await;
    drop(turn);
    drop(handles);
    assert_eq!(stored_prompts(&revived, &session), ["first"]);

    // The open restores it for every client and starts nothing.
    let mut client = Client::attach(&revived.host, &session).await;
    let pending = {
        let handles = revived.host.local_handles(&session).await.unwrap();
        handles.queues.pending(AgentId::Main)
    };
    let (kind, input) = pending.expect("the prompt is pending");
    assert_eq!((kind, &input.content), (PendingKind::FollowUp, &content));
    let shown = &client.chat.queue().queues;
    assert_eq!(shown.len(), 1, "the client shows it as pending");
    no_turn_starts(&mut requests).await;

    // A withdrawal hands back text, so it cannot take an attachment.
    if input.has_attachments() {
        let remove = Command::Queue(QueueOp::Remove {
            agent: AgentId::Main,
        });
        let refused = revived.host.command(&session, remove).await;
        let reason = refused.expect_err("an attachment stays").to_string();
        assert!(reason.contains("has an attachment"), "{reason}");
        assert_eq!(
            saved_input(&revived, &session).await,
            std::slice::from_ref(&content)
        );
    }

    // Continue delivers it as the message it was saved to become.
    command(&revived, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    let delivered: Vec<_> = inference
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(&user.content),
            _ => None,
        })
        .collect();
    assert_eq!(delivered.len(), 2, "the first prompt and this one");
    assert_eq!(delivered[1], &content);
    inference.answer(finalized_text_message("done"));
    let frames = client.pump_until_idle().await;
    assert_eq!(queue_updates(&frames), 1, "the drain clears the box");
    assert!(
        client
            .chat
            .queue()
            .queues
            .iter()
            .all(|queue| { queue.steering.is_empty() && queue.follow_up.is_empty() })
    );
    assert!(saved_input(&revived, &session).await.is_empty());
    assert_eq!(stored_prompts(&revived, &session).len(), 2);
    let stored = revived.stored(&session);
    let last_prompt = stored
        .as_array()
        .unwrap()
        .iter()
        .rfind(|record| record["message"]["role"] == "user");
    assert_eq!(
        last_prompt.unwrap()["message"]["content"],
        serde_json::to_value(&content).unwrap()
    );
    assert_eq!(last_prompt.unwrap()["id"], input.id.as_str());
    drop(client);
    revived.host.shutdown().await;

    // It is not delivered a second time.
    let third = revived.revive(Vec::new());
    assert!(saved_input(&third, &session).await.is_empty());
    let nothing = third.host.command(&session, Command::Continue).await;
    assert!(nothing.is_err(), "{nothing:?}");
    assert_eq!(stored_prompts(&third, &session).len(), 2);
    third.host.shutdown().await;
}

#[tokio::test]
async fn an_acknowledged_idle_prompt_is_delivered_once_after_a_crash() {
    an_acknowledged_idle_prompt_survives_a_crash(vec![UserContent::text("survives")]).await;
}

#[tokio::test]
async fn an_acknowledged_idle_prompt_keeps_its_attachment_across_a_crash() {
    let content = vec![
        UserContent::text("what is this"),
        UserContent::image("aGVsbG8=", "image/png"),
    ];
    an_acknowledged_idle_prompt_survives_a_crash(content).await;
}

/// The system prompt's entry and the main agent's head, as the host of
/// `session` holds them.
async fn root_and_head(harness: &Harness, session: &str) -> (String, String) {
    let handles = harness.host.local_handles(session).await.unwrap();
    let log = handles.log.lock().await;
    let root = log.system_prompt_id().cloned().expect("a root");
    (root, log.head().cloned().expect("a head"))
}

/// Switch the head of `session` to `entry`, setting `BRANCH` in the
/// environment when `change` says so. That is an entry the switch writes.
async fn switch_to(harness: &Harness, session: &str, entry: &str, change: bool) {
    let mut changes = aj_wire::BranchChanges::default();
    if change {
        changes.env = BTreeMap::from([("BRANCH".into(), Some("set".into()))]);
    }
    let target = HeadTarget::Entry(entry.to_owned());
    command(harness, session, Command::Head { changes, target }).await;
}

/// What a later host does with a prompt that was acknowledged and whose
/// turn appended nothing: it restores the prompt without starting a turn,
/// Continue delivers it in a context of `context` user messages, and a
/// third host has nothing left to deliver.
async fn delivered_once_by(
    revived: Harness,
    mut requests: UnboundedReceiver<Inference>,
    session: &str,
    context: &[&str],
) {
    let mut client = Client::attach(&revived.host, session).await;
    let pending = {
        let handles = revived.host.local_handles(session).await.unwrap();
        handles.queues.pending(AgentId::Main)
    };
    let (kind, input) = pending.expect("the prompt is pending");
    let last = *context.last().expect("the prompt");
    assert_eq!(kind, PendingKind::FollowUp);
    assert_eq!(input.content, [UserContent::text(last)]);
    assert_eq!(client.chat.queue().queues.len(), 1, "a client shows it");
    no_turn_starts(&mut requests).await;

    command(&revived, session, Command::Continue).await;
    let inference = next(&mut requests).await;
    assert_eq!(said(&inference), context);
    inference.answer(finalized_text_message("done"));
    client.pump_until_idle().await;
    assert!(saved_input(&revived, session).await.is_empty());
    let stored = revived.stored(session);
    let message = stored
        .as_array()
        .unwrap()
        .iter()
        .rfind(|record| record["message"]["role"] == "user");
    assert_eq!(message.unwrap()["id"], input.id.as_str());
    let prompts = stored_prompts(&revived, session);
    drop(client);
    revived.host.shutdown().await;

    let third = revived.revive(Vec::new());
    assert!(saved_input(&third, session).await.is_empty());
    let nothing = third.host.command(session, Command::Continue).await;
    assert!(nothing.is_err(), "{nothing:?}");
    assert_eq!(stored_prompts(&third, session), prompts);
    third.host.shutdown().await;
}

/// The first prompt of a new session creates the session and is saved with
/// it before it is acknowledged. A host that dies before the turn appended
/// anything leaves a session that reopens with the prompt pending, and the
/// prompt is delivered once.
#[tokio::test]
async fn the_first_prompt_of_a_new_session_survives_a_crash() {
    let (harness, _requests) = held();
    let session = harness.create().await;
    assert!(!harness.persistence.in_database(&session).unwrap());
    let handles = harness.host.local_handles(&session).await.unwrap();
    // The turn waits for the agent, so it has appended nothing when the
    // host dies.
    let turn = Arc::clone(&handles.agent).lock_owned().await;
    harness.prompt(&session, "first").await;
    assert!(harness.persistence.in_database(&session).unwrap());
    assert_eq!(
        saved_input(&harness, &session).await,
        [vec![UserContent::text("first")]]
    );
    let created = harness.stored(&session);
    let kinds: Vec<&str> = created
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|record| record["type"].as_str())
        .collect();
    assert_eq!(kinds.first(), Some(&"system_prompt"), "{kinds:?}");
    assert!(kinds.contains(&"model_change"), "{kinds:?}");
    assert!(stored_prompts(&harness, &session).is_empty());

    let (revived, requests, _store) = crashed(harness, &session).await;
    drop(turn);
    drop(handles);
    assert_eq!(revived.stored(&session), created, "the turn appended");
    delivered_once_by(revived, requests, &session, &["first"]).await;
}

/// The first prompt at a fork point creates the branch, with the entry the
/// switch produced, and is saved with it. A host that dies before the turn
/// appended anything leaves a session that reopens on the branch with the
/// prompt pending, and the prompt is delivered once, on the branch.
#[tokio::test]
async fn the_first_prompt_at_a_fork_point_survives_a_crash() {
    let (harness, _requests, session) = idle_after_a_turn().await;
    let (root, tip) = root_and_head(&harness, &session).await;
    let before = harness.stored(&session);
    switch_to(&harness, &session, &root, true).await;
    let (_, draft) = root_and_head(&harness, &session).await;
    assert_ne!(draft, root, "the switch's record is read at the fork point");
    assert_eq!(harness.stored(&session), before, "and is not stored");

    let handles = harness.host.local_handles(&session).await.unwrap();
    let turn = Arc::clone(&handles.agent).lock_owned().await;
    harness.prompt(&session, "forked").await;
    assert_eq!(
        saved_input(&harness, &session).await,
        [vec![UserContent::text("forked")]]
    );
    let created = harness.stored(&session);
    let records = created.as_array().unwrap();
    assert_eq!(records.len(), before.as_array().unwrap().len() + 1);
    let change = records.last().unwrap();
    assert_eq!(
        (&change["type"], &change["id"]),
        (&"env_change".into(), &draft.clone().into())
    );
    assert_eq!(change["parent_id"], root.as_str());

    let (revived, requests, _store) = crashed(harness, &session).await;
    drop(turn);
    drop(handles);
    assert_eq!(revived.stored(&session), created, "the turn appended");
    let (_, head) = root_and_head(&revived, &session).await;
    assert_eq!(head, draft, "the reopen is on the fork, not on {tip}");
    // The branch's context does not hold the other thread's prompt.
    delivered_once_by(revived, requests, &session, &["forked"]).await;
}

/// A fork point that is picked and left without input leaves nothing in
/// the store, whatever the switch produced: left by another switch, or by
/// the host ending.
#[tokio::test]
async fn an_abandoned_fork_point_leaves_nothing_in_the_store() {
    let (harness, _requests, session) = idle_after_a_turn().await;
    let (root, tip) = root_and_head(&harness, &session).await;
    let before = (
        harness.stored(&session),
        harness.persistence.committed_writes(),
    );
    let unchanged = |harness: &Harness| {
        (
            harness.stored(&session),
            harness.persistence.committed_writes(),
        ) == before
    };

    switch_to(&harness, &session, &root, true).await;
    assert!(unchanged(&harness), "the switch wrote");
    switch_to(&harness, &session, &tip, false).await;
    assert!(unchanged(&harness), "leaving the fork point wrote");
    assert_eq!(root_and_head(&harness, &session).await.1, tip);
    switch_to(&harness, &session, &root, true).await;
    assert!(unchanged(&harness));
    let stored = harness.stored(&session);
    harness.host.shutdown().await;

    let revived = harness.revive(Vec::new());
    assert_eq!(root_and_head(&revived, &session).await.1, tip);
    assert_eq!(revived.stored(&session), stored);
    revived.host.shutdown().await;
}

/// Which thread a reopen is on is read off what was done, not off
/// what was looked at: a switch to another thread's end stores
/// nothing, and a prompt there makes that thread the last active.
#[tokio::test]
async fn a_reopen_lands_on_the_thread_last_worked_on() {
    let (harness, mut requests, session) = idle_after_a_turn().await;
    let (root, first) = root_and_head(&harness, &session).await;
    let turn = |harness: &Harness, text: &'static str| {
        let (host, session) = (harness.host.clone(), session.clone());
        async move {
            let client = Client::attach(&host, &session).await;
            host.command(&session, prompt(text))
                .await
                .expect("accepted");
            client
        }
    };
    // A branch from the root, worked on last.
    switch_to(&harness, &session, &root, false).await;
    let mut client = turn(&harness, "second").await;
    next(&mut requests)
        .await
        .answer(finalized_text_message("done"));
    client.pump_until_idle().await;
    let (_, second) = root_and_head(&harness, &session).await;
    assert_ne!(second, first);

    // A switch to the first thread's end with nothing done there is
    // not remembered.
    let writes = harness.persistence.committed_writes();
    switch_to(&harness, &session, &first, false).await;
    assert_eq!(root_and_head(&harness, &session).await.1, first);
    assert_eq!(harness.persistence.committed_writes(), writes);
    drop(client);
    harness.host.shutdown().await;
    let (revived, mut requests) = held_over(&harness);
    assert_eq!(root_and_head(&revived, &session).await.1, second);

    // A prompt after the switch is.
    switch_to(&revived, &session, &first, false).await;
    let mut client = turn(&revived, "third").await;
    let inference = next(&mut requests).await;
    assert_eq!(said(&inference), ["first", "done", "third"]);
    inference.answer(finalized_text_message("done"));
    client.pump_until_idle().await;
    let (_, third) = root_and_head(&revived, &session).await;
    drop(client);
    revived.host.shutdown().await;
    let last = revived.revive(Vec::new());
    assert_eq!(root_and_head(&last, &session).await.1, third);
    assert_eq!(
        stored_prompts(&last, &session),
        ["first", "second", "third"]
    );
    last.host.shutdown().await;
}
