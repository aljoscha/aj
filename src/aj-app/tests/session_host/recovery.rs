//! What an open does with what the last host left, and that every exit of
//! a host leaves the same thing.
//!
//! The claims are about stored state, so they are asserted on the stored
//! records, on the rows beside them and on what the next host's model is
//! told.

use super::interruptions::{Inference, command, held, next};
use super::sub_agents::{crashed, said};
use super::tasks::{delivered, eventually, gated};
use super::*;
use aj_app::recovery::recover;
use aj_session::SessionLog;
use tokio::sync::mpsc::UnboundedReceiver;

/// Everything the store holds for `session`: its entries and the rows
/// beside them.
pub(super) fn stored_state(harness: &Harness, session: &str) -> (serde_json::Value, Vec<String>) {
    (
        harness.stored(session),
        harness.persistence.stored_rows(session),
    )
}

/// A host in the middle of a turn that has everything an open settles:
/// the main agent waits for a foreground command, a background sub-agent
/// is in its run, a recorded background command runs until `gate` exists,
/// and a prompt is queued behind the turn. Returns the pid of the
/// task record.
async fn mid_turn(gate: &std::path::Path) -> (Harness, UnboundedReceiver<Inference>, String, u32) {
    let (harness, mut requests) = held();
    let session = harness.create().await;
    harness.prompt(&session, "kick it off").await;
    next(&mut requests).await.answer(calling(
        "delegating",
        "call-bg",
        "agent",
        serde_json::json!({"task": "look into it", "run_in_background": true}),
    ));
    // The parent's next inference and the sub-agent's first, in either
    // order. The sub-agent's stays open.
    let (first, second) = (next(&mut requests).await, next(&mut requests).await);
    let parents = |inference: &Inference| said(inference).contains(&"kick it off".to_string());
    let parent = if parents(&first) { first } else { second };
    parent.answer(calling(
        "backgrounding",
        "call-task",
        "bash",
        serde_json::json!({
            "command": gated(gate), "run_in_background": true, "description": "background"
        }),
    ));
    let never = gate.with_file_name("never");
    next(&mut requests).await.answer(calling(
        "waiting",
        "call-fg",
        "bash",
        serde_json::json!({"command": gated(&never), "timeout": 300, "description": "wait"}),
    ));
    eventually("the foreground call to be stored", || {
        harness.stored(&session).to_string().contains("call-fg")
    })
    .await;
    harness.prompt(&session, "queued behind the turn").await;
    let recorded = harness.persistence.recorded_tasks(&session);
    assert_eq!(recorded.len(), 1, "the background command is recorded");
    (harness, requests, session, recorded[0].1)
}

/// Recovery is a function of stored state and can be repeated: a host
/// that dies after its open settled the session, before anything was
/// installed, leaves a session the next open recovers to the same result.
/// Nothing is answered, closed or announced twice.
#[tokio::test]
async fn a_second_recovery_equals_the_first() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let (harness, _requests, session, pid) = mid_turn(&gate).await;
    let (revived, mut requests, _store) = crashed(harness, &session).await;
    std::fs::write(&gate, "").unwrap();
    eventually("the recorded task to finish", || !process_running(pid)).await;

    // An open that settles the session and dies before it installs.
    let settle = || {
        let mut log = SessionLog::resume(&revived.persistence, &session).unwrap();
        let recovered = recover(&mut log).expect("recover");
        drop(log);
        (recovered, stored_state(&revived, &session))
    };
    let (first, stored_first) = settle();
    assert_eq!(first.interrupted, [AgentId::Main]);
    assert_eq!(first.main.inputs.len(), 1, "the queued prompt");
    assert_eq!(
        first.main.task_notifications.len(),
        2,
        "the command and the run"
    );
    assert!(first.main.tasks.is_empty(), "the command ended unwatched");
    assert!(
        first.sub_agents.is_empty(),
        "the run left nothing to install"
    );
    assert!(first.notices.is_empty(), "{:?}", first.notices);

    let (second, stored_second) = settle();
    assert_eq!(format!("{second:?}"), format!("{first:?}"));
    assert_eq!(stored_second, stored_first, "the second open wrote nothing");

    // A host that opens it now holds the same, and delivers each thing once.
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(stored_state(&revived, &session), stored_first);
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 2);
    assert!(handles.task_registry.snapshot().is_empty());
    let mut client = Client::attach(&revived.host, &session).await;
    command(&revived, &session, Command::Continue).await;
    let inference = next(&mut requests).await;
    let told = said(&inference);
    let count = |needle: &str| told.iter().filter(|text| text.contains(needle)).count();
    assert_eq!(count("Background agent 1 was interrupted"), 1, "{told:?}");
    assert_eq!(count("Background task #"), 1, "{told:?}");
    assert_eq!(count("queued behind the turn"), 1, "{told:?}");
    inference.answer(finalized_text_message("carrying on"));
    client.pump_until_idle().await;
    drop(handles);

    let stored = revived.stored(&session).to_string();
    assert_eq!(stored.matches("sub_agent_end").count(), 1);
    assert_eq!(delivered(&revived, &session).await.len(), 2);
    let interrupted = "Previous session was interrupted before this tool call completed";
    assert_eq!(stored.matches(interrupted).count(), 1, "one call was open");
    revived.host.shutdown().await;
}

/// A Code Mode cell that was open when its host died is gone, and the agent
/// that started it is told once. The cell announces its loss when it
/// becomes a background task, so the next open finds the notice among the
/// owner's pending work.
#[tokio::test]
async fn an_open_cell_lost_with_its_host_is_reported_once() {
    let scratch = TempDir::new().unwrap();
    let never = scratch.path().join("never");
    let source = format!(
        "yield_control(); await tools.bash({});",
        serde_json::json!({"command": gated(&never), "description": "wait", "timeout": 300})
    );
    let mut exec = finalized_text_message("");
    exec.stop_reason = StopReason::ToolUse;
    exec.content = vec![AssistantContent::ToolCall(ToolCall {
        id: "call-exec".into(),
        name: "exec".into(),
        is_raw: true,
        arguments: serde_json::json!(source),
    })];
    let script = vec![exec, finalized_text_message("the cell is open")];
    let mut run = snapshot(scripted(script, 0, Duration::ZERO));
    let model = Arc::make_mut(&mut run.main.model_info);
    model.id = "gpt-6-astra".into();
    model.api = "openai-responses".into();
    run.main.model_key = (model.provider.clone(), model.id.clone());
    let harness = Harness::with_run_config(run, vec![], None, None);
    harness.config.lock().unwrap().code_mode = true;
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "open a cell").await;
    client.pump_until_idle().await;

    let handles = harness.host.local_handles(&session).await.unwrap();
    let tasks = handles.task_registry.snapshot();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert!(matches!(tasks[0].kind, TaskKind::CodeMode { .. }));
    assert_eq!(tasks[0].status, TaskStatus::Running);
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 0);
    let rows = harness.persistence.stored_rows(&session);
    let queued: Vec<_> = rows
        .iter()
        .filter(|row| row.starts_with("pending"))
        .collect();
    assert_eq!(queued.len(), 1, "the cell announced its loss: {rows:?}");
    assert!(queued[0].contains("was lost"), "{rows:?}");
    drop((handles, client));

    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let Harness { host, .. } = harness;
    drop(host);
    let tasks = bounded("the next host to open the session", async {
        loop {
            if let Ok(handles) = revived.host.local_handles(&session).await {
                return handles.task_registry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(tasks.snapshot().is_empty(), "the cell is not restored");
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    let mut client = Client::attach(&revived.host, &session).await;
    command(&revived, &session, Command::Continue).await;
    client.pump_until_idle().await;
    let notices = delivered(&revived, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("was lost") && notices[0].contains("Its state is gone"),
        "{notices:?}"
    );
    revived.host.shutdown().await;

    let third = revived.revive(Vec::new());
    let handles = third.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.task_registry.pending_notices(AgentId::Main), 0);
    assert_eq!(delivered(&third, &session).await.len(), 1, "told once");
    drop(handles);
    third.host.shutdown().await;
}

/// Quit is a crash: a host that shuts down in the middle of a turn leaves
/// exactly the stored session a host that died there leaves. No aborted
/// message, no cancelled tool result, no turn end and no task end is
/// written on the way out. Both sessions then reopen to the same state.
#[tokio::test]
async fn a_shutdown_mid_turn_leaves_what_a_dead_host_leaves() {
    /// The stored session with what differs between any two runs taken
    /// out: ids, pids, times and paths.
    fn shape(state: &(serde_json::Value, Vec<String>)) -> (Vec<String>, Vec<String>) {
        let records = state.0.as_array().expect("records").iter();
        let records = records.map(|record| {
            let role = record["message"]["role"].as_str().unwrap_or("-");
            let call = record["message"]["tool_call_id"].as_str().unwrap_or("-");
            let error = &record["message"]["is_error"];
            let stop = record["message"]["stop_reason"].as_str().unwrap_or("-");
            format!("{} {role} {call} {error} {stop}", record["type"])
        });
        let rows = state.1.iter().map(|row| {
            let mut words = row.split(' ');
            match words.next() {
                // The thread, its kind, its agent and its turn.
                Some("thread") => row.clone(),
                // The thread and the kind of what waits in it.
                Some("pending") => {
                    let (thread, _id, kind) = (words.next(), words.next(), words.next());
                    format!("pending {thread:?} {kind:?}")
                }
                // The thread, the task id and its call.
                _ => format!("task {:?}", words.take(3).collect::<Vec<_>>()),
            }
        });
        (records.collect(), rows.collect())
    }

    let scratch = TempDir::new().unwrap();
    let (stopped, _requests, stopped_session, stopped_pid) =
        mid_turn(&scratch.path().join("a")).await;
    let (died, _requests_too, died_session, died_pid) = mid_turn(&scratch.path().join("b")).await;

    let before = shape(&stored_state(&stopped, &stopped_session));
    stopped.host.shutdown().await;
    let after_shutdown = stored_state(&stopped, &stopped_session);
    assert_eq!(shape(&after_shutdown), before, "the shutdown wrote nothing");
    assert!(process_running(stopped_pid), "a recorded task runs on");

    let (revived_died, _held, _store) = crashed(died, &died_session).await;
    let after_death = stored_state(&revived_died, &died_session);
    assert_eq!(shape(&after_shutdown), shape(&after_death));
    assert!(process_running(died_pid));
    let working = |state: &(serde_json::Value, Vec<String>)| {
        let rows = state.1.iter();
        rows.filter(|row| row.ends_with("working")).count()
    };
    assert_eq!(
        working(&after_shutdown),
        2,
        "the turn and the run: {:?}",
        after_shutdown.1
    );

    // Both reopen to the same settled session, with the command watched.
    let revived_stopped = stopped.revive(Vec::new());
    let mut reopened = Vec::new();
    for (harness, session) in [
        (&revived_stopped, &stopped_session),
        (&revived_died, &died_session),
    ] {
        let handles = harness.host.local_handles(session).await.unwrap();
        let tasks = handles.task_registry.snapshot();
        let tasks: Vec<_> = tasks.iter().map(|task| (task.id, task.status)).collect();
        let notices = handles.task_registry.pending_notices(AgentId::Main);
        let queued = handles.queues.snapshot(AgentId::Main).text;
        let stored = shape(&stored_state(harness, session));
        reopened.push((tasks, notices, queued, stored));
    }
    assert_eq!(reopened[0], reopened[1]);
    let (tasks, notices, queued, _) = &reopened[0];
    assert_eq!(tasks.len(), 1, "the recorded task is watched again");
    assert_eq!(*notices, 1, "the interrupted run");
    assert_eq!(queued, "queued behind the turn");
    revived_stopped.host.shutdown().await;
    revived_died.host.shutdown().await;
}

/// A head switch happens whole or not at all. One that fails after the
/// log's head has moved, here because the entry it appends to the target
/// branch is refused, ends the session instead of leaving the host on one
/// branch and its clients on another. The session opens again on what is
/// stored, and the switch can be made again.
#[tokio::test]
async fn a_head_switch_that_fails_after_the_head_moved_ends_the_session() {
    let harness = Harness::new(vec![
        finalized_text_message("one"),
        finalized_text_message("two"),
        finalized_text_message("three"),
        finalized_text_message("four"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    let head = || {
        let (host, session) = (harness.host.clone(), session.clone());
        async move {
            let handles = host.local_handles(&session).await.expect("live session");
            let log = handles.log.lock().await;
            log.head().cloned().expect("a head")
        }
    };
    let switch = |entry: &str, env: Option<&str>| {
        let env = env.map(|value| ("AJ_BRANCH".to_string(), Some(value.to_string())));
        let changes = aj_wire::BranchChanges {
            env: env.into_iter().collect(),
            ..Default::default()
        };
        let target = HeadTarget::Entry(entry.to_string());
        harness
            .host
            .command(&session, Command::Head { target, changes })
    };

    // Two branches: one ends on "two", the other forks before it and ends
    // on "three". The session is on the second.
    harness.prompt(&session, "first").await;
    client.pump_until_idle().await;
    let fork = head().await;
    harness.prompt(&session, "second").await;
    client.pump_until_idle().await;
    let first_tip = head().await;
    switch(&fork, None).await.expect("fork");
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "third").await;
    client.pump_until_idle().await;
    let second_tip = head().await;
    let before = stored_state(&harness, &session);

    // Back to the first branch with a change to record there, which the
    // store refuses. The head has moved by then.
    harness.persistence.refuse_sqlite_entry_writes(true);
    let refused = switch(&first_tip, Some("refused")).await;
    harness.persistence.refuse_sqlite_entry_writes(false);
    let reason = refused.expect_err("the switch failed").to_string();
    assert!(reason.contains("Switching branches failed"), "{reason}");
    let frames = frames_until(&mut client.stream, "the session to end", |frame| {
        matches!(frame, Frame::Error { .. })
    })
    .await;
    let ended =
        |frame: &Frame| matches!(frame, Frame::Error { code, .. } if code == "persistence_failed");
    assert_eq!(frames.iter().filter(|frame| ended(frame)).count(), 1);
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, Frame::Reset { .. })),
        "no client was moved to the other branch"
    );
    bounded("the ended session to release its lock", async {
        while SessionLock::is_held(&harness.persistence, &session).expect("probe lock") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert_eq!(
        stored_state(&harness, &session),
        before,
        "nothing was stored"
    );

    // The session opens again where the store has it, for host and client
    // alike, and the switch goes through.
    assert_eq!(head().await, second_tip);
    let mut client = Client::attach(&harness.host, &session).await;
    let shown = assistant_rows(&client.chat, AgentId::Main);
    assert_eq!(shown, ["one", "three"], "the stored branch");
    switch(&first_tip, Some("recorded")).await.expect("switch");
    let mut client_again = Client::attach(&harness.host, &session).await;
    assert_eq!(
        assistant_rows(&client_again.chat, AgentId::Main),
        ["one", "two"]
    );
    harness.prompt(&session, "fourth").await;
    client_again.pump_until_idle().await;
    client.drain_into_fold();
    harness.host.shutdown().await;
}
