//! What a reopened SQLite session leaves in the store and what it saves for
//! a restart, observed through the host.

use super::*;
use aj_agent::message::AgentMessage;
use aj_models::types::{AssistantContent, Message, ToolCall, ToolResultMessage, UserMessage};

const TURNS: usize = 300;

/// A saved session whose last compaction is older than its display tail.
/// Each turn is a prompt, a tool call and its output. Reopened from SQLite,
/// the turns before the compaction's first kept one stay in the store.
/// Returns the session id and the id of each turn's prompt.
fn compacted_long_ago(persistence: &ProjectSessions) -> (String, Vec<String>) {
    let mut log = SessionLog::create(persistence).unwrap();
    let add = |log: &mut SessionLog, message: Message| {
        let entry = aj_session::LogEntryKind::Message {
            message: AgentMessage::wire(message),
        };
        let head = log.head().cloned();
        let appended = log.append(head, aj_session::ThreadKind::Main, None, entry);
        appended.unwrap().id
    };
    let mut prompts = Vec::new();
    for turn in 0..TURNS {
        let id = format!("call-{turn}");
        let mut call = finalized_text_message("");
        call.content = vec![AssistantContent::ToolCall(ToolCall {
            is_raw: false,
            id: id.clone(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        })];
        let prompt = Message::User(UserMessage::text(format!("prompt {turn}")));
        prompts.push(add(&mut log, prompt));
        add(&mut log, Message::Assistant(call));
        let output = ToolResultMessage::text(id, "read", format!("output {turn}"), false);
        add(&mut log, Message::ToolResult(output));
        if turn == 250 {
            let (kept, retained) = (prompts[100].clone(), vec![prompts[3].clone()]);
            let filter = aj_session::ThreadFilter::MAIN;
            let usage = Default::default();
            log.append_compaction(filter, "SUMMARY".into(), kept, retained, 1, None, usage)
                .unwrap();
        }
    }
    (log.session_id().to_string(), prompts)
}

/// Every reader that goes past what a reopened log holds reads the rest: a
/// history page, a branch whose context reaches before the compaction, and
/// an export of the whole session.
#[tokio::test]
async fn readers_of_old_history_get_what_a_reopen_left_in_the_store() {
    let answer = vec![finalized_text_message("answer")];
    let (harness, provider) = goals::recorded(answer);
    let (session, prompts) = compacted_long_ago(&harness.persistence);
    {
        let log = SessionLog::resume(&harness.persistence, &session).unwrap();
        let start = log.snapshot().window_start();
        assert_eq!(start, 301, "the fixture leaves turns 0 to 99 in the store");
    }

    // Paging back to the start shows an old call with its output.
    let mut client = Client::attach(&harness.host, &session).await;
    while client.chat.history_start(AgentId::Main) > 1 {
        let before = client.chat.history_start(AgentId::Main);
        let page = harness.host.history(&session, AgentId::Main, before, 200);
        assert!(
            client
                .client
                .apply_history(&mut client.chat, page.await.unwrap())
        );
    }
    let shown = main_tool_content(&client.canonical(), "call-5").to_string();
    assert!(shown.contains("output 5"), "{shown}");

    // A branch off turn 20 has no compaction behind it, so its context is
    // every turn up to there, outputs included. It is named as a reader
    // does, by the prompt it replaces, and the output before that prompt is
    // not in memory.
    let head = Command::Head {
        changes: Default::default(),
        target: HeadTarget::Before(prompts[21].clone()),
    };
    harness.host.command(&session, head).await.expect("switch");
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "next").await;
    client.pump_until_idle().await;
    let contexts = provider.contexts.lock().unwrap().clone();
    let results: Vec<String> = contexts
        .last()
        .expect("a request")
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(format!("{:?}", result.content)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 21, "turns 0 to 20");
    assert!(results[5].contains("output 5"), "{}", results[5]);

    // The export is the whole session, the other branch's old turns too.
    let html = harness.host.export_html(&session).await.unwrap().html;
    let (_, island) = html.split_once("id=\"session-data\">").unwrap();
    let (island, _) = island.split_once("</script>").unwrap();
    let standard = base64::engine::general_purpose::STANDARD;
    let gzipped = base64::Engine::decode(&standard, island.trim()).unwrap();
    let mut exported = String::new();
    let mut decoder = flate2::read::GzDecoder::new(&gzipped[..]);
    std::io::Read::read_to_string(&mut decoder, &mut exported).unwrap();
    assert!(exported.contains("output 50") && exported.contains("output 299"));
    harness.host.shutdown().await;
}

/// The queue a restart restores is the queue as the session left it:
/// steering is saved like a follow-up and a withdrawn message stays
/// withdrawn. What was queued on a branch holds the head there until it is
/// delivered or withdrawn.
#[tokio::test]
async fn a_restart_restores_the_queue_as_the_session_left_it() {
    let slow = || {
        let answer = vec![finalized_text_message("slow enough to queue behind")];
        Harness::with_provider(scripted(answer, 1, Duration::from_millis(30)))
    };
    let steer = |text: &str| Command::Steer {
        agent: AgentId::Main,
        text: text.to_string(),
    };
    let queued = |harness: Harness, session: String| async move {
        let handles = harness.host.local_handles(&session).await.unwrap();
        let snapshot = handles.queues.snapshot(AgentId::Main);
        drop(handles);
        (harness, snapshot.kind, snapshot.text)
    };

    // Steering queued behind a busy turn comes back as steering.
    let harness = slow();
    let session = harness.create().await;
    harness.prompt(&session, "first").await;
    let steered = harness.host.command(&session, steer("steer me")).await;
    steered.expect("steering queues behind the turn");
    harness.host.shutdown().await;
    let revived = harness.revive(vec![finalized_text_message("unused")]);
    let (revived, kind, text) = queued(revived, session.clone()).await;
    assert_eq!(
        (kind, text.as_str()),
        (Some(aj_agent::queue::PendingKind::Steering), "steer me")
    );

    // It belongs to its branch, so the branch cannot be left while it
    // waits: a switch is refused and changes nothing, here and after a
    // restart.
    let (root, tip) = {
        let handles = revived.host.local_handles(&session).await.unwrap();
        let log = handles.log.lock().await;
        let root = log.system_prompt_id().cloned().expect("a root");
        (root, log.head().cloned().expect("a head"))
    };
    let head = |entry: &String| Command::Head {
        changes: Default::default(),
        target: HeadTarget::Entry(entry.clone()),
    };
    let steering = Some(aj_agent::queue::PendingKind::Steering);
    let refused = revived.host.command(&session, head(&root)).await;
    let said = refused.expect_err("pending work waits").to_string();
    assert!(
        said.contains("a queued message is waiting") && said.contains("withdraw it"),
        "{said}"
    );
    let (revived, kind, text) = queued(revived, session.clone()).await;
    assert_eq!((kind, text.as_str()), (steering, "steer me"));
    revived.host.shutdown().await;
    let again = revived.revive(vec![finalized_text_message("steered")]);
    {
        let handles = again.host.local_handles(&session).await.unwrap();
        assert_eq!(handles.log.lock().await.head(), Some(&tip));
    }
    let (again, kind, text) = queued(again, session.clone()).await;
    assert_eq!((kind, text.as_str()), (steering, "steer me"));
    // Continue delivers it, and the branch can then be left.
    let mut client = Client::attach(&again.host, &session).await;
    again
        .host
        .command(&session, Command::Continue)
        .await
        .expect("continue");
    client.pump_until_idle().await;
    let (again, kind, _) = queued(again, session.clone()).await;
    assert_eq!(kind, None, "the message was delivered");
    again
        .host
        .command(&session, head(&root))
        .await
        .expect("nothing waits on the branch");
    again.host.shutdown().await;

    // A message withdrawn from the queue does not come back either.
    let harness = slow();
    let session = harness.create().await;
    harness.prompt(&session, "first").await;
    harness.prompt(&session, "changed my mind").await;
    let remove = Command::Queue(QueueOp::Remove {
        agent: AgentId::Main,
    });
    let withdrawn = harness.host.command(&session, remove).await.unwrap();
    assert!(matches!(withdrawn, CommandOutcome::Withdrawn(Some(_))));
    harness.host.shutdown().await;
    let revived = harness.revive(Vec::new());
    let (revived, kind, _) = queued(revived, session).await;
    assert_eq!(kind, None, "the withdrawn message stayed withdrawn");
    revived.host.shutdown().await;
}

/// Accepting a queued message, withdrawing one and accepting the first
/// prompt of a branch are saved before they take effect: a refused save
/// refuses the command and leaves the session as it was.
#[tokio::test]
async fn a_change_that_cannot_be_saved_is_refused_and_changes_nothing() {
    let answer = vec![finalized_text_message("slow enough to queue behind")];
    let harness = Harness::with_provider(scripted(answer, 1, Duration::from_millis(30)));
    let session = harness.create().await;
    harness.prompt(&session, "first").await;
    harness.prompt(&session, "queued").await;
    let handles = harness.host.local_handles(&session).await.unwrap();
    let queued = || handles.queues.snapshot(AgentId::Main).text;

    harness.persistence.refuse_sqlite_pending_writes();
    let refused = harness.host.command(&session, prompt("more")).await;
    let said = refused.expect_err("the save was refused").to_string();
    assert!(
        said.contains("the queued message could not be saved"),
        "{said}"
    );
    assert_eq!(queued(), "queued", "the refused text was not queued");
    let remove = Command::Queue(QueueOp::Remove {
        agent: AgentId::Main,
    });
    let refused = harness.host.command(&session, remove).await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(queued(), "queued", "the message is still queued");
    drop(handles);
    harness.host.shutdown().await;

    // A restart agrees with what the commands answered.
    let revived = harness.revive(Vec::new());
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.queues.snapshot(AgentId::Main).text, "queued");
    drop(handles);
    revived.host.shutdown().await;

    // No head switch is saved, so nothing refuses one. The first prompt at
    // a fork point is: it creates the branch, and a refused save refuses the
    // prompt and creates nothing.
    let harness = Harness::new(vec![
        finalized_text_message("done"),
        finalized_text_message("on the fork"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    client.pump_until_idle().await;
    let handles = harness.host.local_handles(&session).await.unwrap();
    let (root, tip) = {
        let log = handles.log.lock().await;
        let root = log.system_prompt_id().cloned().expect("a root");
        (root, log.head().cloned().expect("a head"))
    };
    let head = |entry: &String| Command::Head {
        changes: Default::default(),
        target: HeadTarget::Entry(entry.clone()),
    };
    harness.persistence.refuse_sqlite_pending_writes();
    for entry in [&root, &tip, &root] {
        let moved = harness.host.command(&session, head(entry)).await;
        assert!(moved.is_ok(), "{moved:?}");
    }
    let stored = harness.stored(&session);
    let refused = harness.host.command(&session, prompt("forked")).await;
    let said = refused.expect_err("the save was refused").to_string();
    assert!(said.contains("the prompt could not be saved"), "{said}");
    assert_eq!(harness.stored(&session), stored);
    assert_eq!(handles.queues.snapshot(AgentId::Main).kind, None);
    {
        let log = handles.log.lock().await;
        assert!(log.is_draft(), "no fork was created");
        assert_eq!(log.head(), Some(&root));
    }
    // The fork point is still there for a prompt that can be saved.
    harness.persistence.allow_sqlite_pending_writes();
    harness.prompt(&session, "forked").await;
    client.pump_until_idle().await;
    assert!(!handles.log.lock().await.is_draft());
    let stored = harness.stored(&session);
    let records = stored.as_array().unwrap().iter();
    let forked: Vec<_> = records
        .filter(|record| record["message"]["content"][0]["text"] == "forked")
        .collect();
    assert_eq!(forked.len(), 1);
    assert_eq!(forked[0]["parent_id"], root.as_str());
    drop(handles);
    harness.host.shutdown().await;
}

/// A settings change is recorded before it is staged, so one the session
/// cannot record is refused and the next turn runs on the old setting.
#[tokio::test]
async fn a_setting_that_cannot_be_recorded_is_not_applied() {
    let harness = Harness::new(vec![finalized_text_message("done")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    client.pump_until_idle().await;
    let handles = harness.host.local_handles(&session).await.unwrap();
    let thinking = || handles.run_config.lock().unwrap().main.thinking.clone();
    let before = thinking();
    assert_ne!(before, Some(aj_models::ThinkingConfig::High));

    fail_sqlite_commits(&mut *handles.log.lock().await);
    let change = Command::Settings(SettingsChange {
        agent: AgentId::Main,
        persist: PersistAction::None,
        axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
    });
    let refused = harness.host.command(&session, change).await;
    let said = refused.expect_err("the change was refused").to_string();
    assert!(said.contains("not changed"), "{said}");
    assert_eq!(thinking(), before);
}

/// Whether a fresh attach opens on an interrupted turn, and whether the
/// client it is folded into shows the notice.
async fn opens_interrupted(harness: &Harness, session: &str) -> (bool, bool) {
    let mut client = Client::attach(&harness.host, session).await;
    let handles = harness.host.local_handles(session).await.unwrap();
    let turn = handles.log.lock().await.turn_state().get(AgentId::Main);
    let stored = turn == aj_session::TurnState::Interrupted;
    drop(handles);
    client.drain_into_fold();
    let transcript = client.chat.transcript(AgentId::Main).expect("main");
    let shown = transcript.entries().iter().any(|entry| {
        matches!(&entry.kind, aj_app::chat::EntryKind::Notice(notice)
            if notice.text.contains("The last turn did not finish"))
    });
    (stored, shown)
}

/// A turn is interrupted when its process ended before it did. One that
/// was cancelled, or that a later turn followed, is not.
#[tokio::test]
async fn only_a_turn_its_process_abandoned_reopens_interrupted() {
    let slow = || {
        let answer = vec![finalized_text_message("slow enough to be cut short")];
        Harness::with_provider(scripted(answer, 1, Duration::from_millis(30)))
    };

    // The host stops while the turn runs.
    let harness = slow();
    let session = harness.create().await;
    harness.prompt(&session, "first").await;
    harness.host.shutdown().await;
    let revived = harness.revive(vec![finalized_text_message("answered")]);
    assert_eq!(opens_interrupted(&revived, &session).await, (true, true));
    // It stays so across another restart, until a turn runs.
    revived.host.shutdown().await;
    let revived = revived.revive(vec![finalized_text_message("answered")]);
    assert_eq!(opens_interrupted(&revived, &session).await, (true, true));
    let mut client = Client::attach(&revived.host, &session).await;
    revived
        .host
        .command(&session, Command::Continue)
        .await
        .unwrap();
    client.pump_until_idle().await;
    revived.host.shutdown().await;
    let revived = revived.revive(Vec::new());
    assert_eq!(opens_interrupted(&revived, &session).await, (false, false));
    revived.host.shutdown().await;

    // The user cancels the turn, then the host stops.
    let harness = slow();
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    let cancel = Command::Cancel {
        agent: AgentId::Main,
    };
    harness.host.command(&session, cancel).await.unwrap();
    client.pump_until_idle().await;
    harness.host.shutdown().await;
    let revived = harness.revive(Vec::new());
    assert_eq!(opens_interrupted(&revived, &session).await, (false, false));
    revived.host.shutdown().await;
}

/// An interrupted turn stays with its branch across a head switch.
#[tokio::test]
async fn an_interrupted_turn_belongs_to_its_branch() {
    let answer = vec![finalized_text_message("slow enough to be cut short")];
    let harness = Harness::with_provider(scripted(answer, 1, Duration::from_millis(30)));
    let session = harness.create().await;
    harness.prompt(&session, "first").await;
    harness.host.shutdown().await;
    let revived = harness.revive(Vec::new());
    assert_eq!(opens_interrupted(&revived, &session).await, (true, true));

    let (root, tip) = {
        let handles = revived.host.local_handles(&session).await.unwrap();
        let log = handles.log.lock().await;
        let root = log.system_prompt_id().cloned().expect("a root");
        (root, log.head().cloned().expect("a head"))
    };
    let head = |entry: String| Command::Head {
        changes: Default::default(),
        target: HeadTarget::Entry(entry),
    };
    revived.host.command(&session, head(root)).await.unwrap();
    assert_eq!(opens_interrupted(&revived, &session).await, (false, false));
    revived.host.command(&session, head(tip)).await.unwrap();
    assert_eq!(opens_interrupted(&revived, &session).await, (true, true));
    revived.host.shutdown().await;
}
