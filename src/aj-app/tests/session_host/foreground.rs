//! Foreground bash commands and their host: the command a tool call waits
//! for is a real process that is never recorded and never outlives the
//! host. A host that ended under one leaves its call unanswered, and the
//! next host answers it as interrupted.
//!
//! The claims are about processes and stored state, so they are asserted on
//! the pid, the task table and the stored transcript. What ends the command
//! when the host's process really dies is the wrapper's lifeline, which
//! `aj-tools` tests against a real pipe. Here the host is one of several in
//! the test's process, so dropping it stands in for that.

use super::tasks::{delivered, eventually, gated, quiet, registry};
use super::*;

const CALL: &str = "call-fg";

/// An assistant message that runs `command` in the foreground as [`CALL`].
fn running(command: &str) -> AssistantMessage {
    calling(
        "running it",
        CALL,
        "bash",
        serde_json::json!({"command": command, "timeout": 300, "description": "foreground"}),
    )
}

/// A host whose first turn is waiting for a command that never ends by
/// itself, and the pid of the command's shell.
async fn waiting() -> (Harness, String, Client, u32) {
    let scratch = TempDir::new().unwrap();
    let pid_file = scratch.path().join("pid");
    let command = format!(
        "echo $$ > '{}'; {}",
        pid_file.display(),
        gated(&scratch.path().join("never"))
    );
    let harness = Harness::new(vec![running(&command), finalized_text_message("ran it")]);
    let session = harness.create().await;
    let client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "run it").await;
    let pid = || std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok();
    eventually("the command to run", || pid().is_some()).await;
    let pid: u32 = pid().unwrap();
    assert!(process_running(pid), "the command runs");
    assert!(
        harness.persistence.recorded_tasks(&session).is_empty(),
        "a foreground command is not recorded"
    );
    (harness, session, client, pid)
}

/// The files bash commands of `harness` left.
fn spill(harness: &Harness) -> Vec<PathBuf> {
    std::fs::read_dir(harness._dir.path().join("spill"))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// Every stored tool result for [`CALL`], as text.
fn answers(harness: &Harness, session: &str) -> Vec<String> {
    fn walk(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("tool_call_id").and_then(|id| id.as_str()) == Some(CALL) {
                    found.push(value.to_string());
                    return;
                }
                map.values().for_each(|value| walk(value, found));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|value| walk(value, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(&harness.stored(session), &mut found);
    found
}

async fn turn_interrupted(harness: &Harness, session: &str) -> bool {
    let stream = harness.host.attach(&[attach_request(session)]).await;
    let mut stream = stream.expect("attach");
    let frames = frames_until(&mut stream, "the session's state", |frame| {
        matches!(frame, Frame::State { .. })
    })
    .await;
    frames.iter().any(|frame| {
        matches!(
            frame,
            Frame::State {
                turn_interrupted: true,
                ..
            }
        )
    })
}

/// A host that ends in the middle of a foreground command, without its
/// turn being cancelled, leaves the call unanswered and no command running.
/// The next host answers the call as interrupted, once, and has no task
/// for it. The turn reads as interrupted, and Continue resumes it.
#[tokio::test]
async fn a_host_that_ended_under_a_command_leaves_a_call_answered_as_interrupted() {
    let (harness, session, client, pid) = waiting().await;
    let revived = harness.revive(vec![finalized_text_message("it was interrupted")]);
    drop(client);
    // Dropping the host aborts the turn where it stands.
    drop(harness.host);
    eventually("the command to end with its host", || !process_running(pid)).await;
    // The session is released once the command's teardown is done.
    let tasks = bounded("the next host to open the session", async {
        loop {
            if let Ok(handles) = revived.host.local_handles(&session).await {
                return handles.task_registry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    let found = answers(&revived, &session);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("Previous session was interrupted before this tool call completed")
            && found[0].contains(r#""is_error":true"#),
        "{found:?}"
    );
    assert!(revived.persistence.recorded_tasks(&session).is_empty());
    assert!(tasks.snapshot().is_empty(), "the command is no task");
    assert_eq!(tasks.pending_notices(AgentId::Main), 0);
    let left = std::fs::read_dir(harness._dir.path().join("spill"));
    let left: Vec<_> = left.map(|dir| dir.flatten().collect()).unwrap_or_default();
    assert!(left.is_empty(), "files left behind: {left:?}");

    // The reopen started no turn. The turn reads as interrupted, and
    // Continue resumes it with the call answered.
    assert!(turn_interrupted(&revived, &session).await);
    let mut client = Client::attach(&revived.host, &session).await;
    revived
        .host
        .command(&session, Command::Continue)
        .await
        .expect("the interrupted turn continues");
    assert_eq!(
        assistant_text(&client.pump_until_idle().await),
        "it was interrupted"
    );
    revived.host.shutdown().await;

    let third = revived.revive(Vec::new());
    registry(&third, &session).await;
    assert_eq!(answers(&third, &session).len(), 1, "answered once");
    third.host.shutdown().await;
}

/// A host that shuts down under a foreground command stops it. The call
/// is answered once across the shutdown and the next open, and nothing is
/// recorded for the command.
#[tokio::test]
async fn a_host_that_shuts_down_stops_its_foreground_command() {
    let (harness, session, _client, pid) = waiting().await;
    harness.host.shutdown().await;
    eventually("the command to end with its host", || !process_running(pid)).await;
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    assert_eq!(spill(&harness), Vec::<PathBuf>::new());

    let revived = harness.revive(Vec::new());
    let tasks = registry(&revived, &session).await;
    assert_eq!(answers(&harness, &session).len(), 1, "answered once");
    assert!(tasks.snapshot().is_empty());
    revived.host.shutdown().await;
}

/// A cancel by the user stops the real process, and the call gets the
/// result a cancelled call gets. The command's files are removed.
#[tokio::test]
async fn a_user_cancel_stops_the_command() {
    let (harness, session, mut client, pid) = waiting().await;

    let cancel = Command::Cancel {
        agent: AgentId::Main,
    };
    harness
        .host
        .command(&session, cancel)
        .await
        .expect("cancel");
    client.pump_until_idle().await;
    eventually("the command to be gone", || !process_running(pid)).await;
    let result = main_tool_content(&client.canonical(), CALL).to_string();
    assert!(result.contains("bash: cancelled by user"), "{result}");
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    eventually("the command's files to be removed", || {
        spill(&harness).is_empty()
    })
    .await;
    harness.host.shutdown().await;

    let revived = harness.revive(Vec::new());
    registry(&revived, &session).await;
    assert_eq!(answers(&harness, &session).len(), 1, "answered once");
    revived.host.shutdown().await;
}

/// A foreground call that runs to its end leaves no row and no file, takes
/// no task id and brings no notice. Its output is one stream.
#[tokio::test]
async fn a_finished_foreground_call_leaves_nothing_behind() {
    let harness = Harness::new(vec![
        running("echo out; echo err >&2; exit 4"),
        calling(
            "backgrounding one",
            "call-bg",
            "bash",
            serde_json::json!({
                "command": "true", "run_in_background": true, "description": "background"
            }),
        ),
        finalized_text_message("done"),
        finalized_text_message("noted"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "run it").await;
    client.pump_until_idle().await;
    quiet(&harness, &session).await;

    let result = main_tool_content(&client.canonical(), CALL).to_string();
    assert!(
        result.contains(r"out\nerr\nCommand failed with exit code: 4")
            && !result.contains("STDERR"),
        "{result}"
    );
    let started = main_tool_content(&client.canonical(), "call-bg").to_string();
    assert!(
        started.contains("Started background task #1:"),
        "the foreground call took no task id: {started}"
    );
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    let tasks = registry(&harness, &session).await;
    let listed: Vec<_> = tasks.snapshot().into_iter().map(|task| task.id).collect();
    assert_eq!(listed, [1], "only the background task is a task");
    let notices = delivered(&harness, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("Background task #1"), "{notices:?}");
    // The background task's output file is all that stays.
    let left = spill(&harness);
    assert_eq!(left.len(), 1, "{left:?}");
    harness.host.shutdown().await;
}
