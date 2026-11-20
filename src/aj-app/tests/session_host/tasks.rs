//! Background bash tasks across hosts: a task is a real process that
//! outlives the host that started it, and its record is what the next host
//! finds it by. `host.shutdown()` stands in for a process ending.
//!
//! The claims are about processes, so they are asserted on the pid.

use super::*;

/// A command that prints `first`, waits for `gate` to exist and prints
/// `second`. It gives up by itself, so a failing test leaves nothing behind
/// for long.
pub(super) fn gated(gate: &std::path::Path) -> String {
    format!(
        "echo first; while [ ! -e '{}' ] && [ $SECONDS -lt 120 ]; do sleep 0.05; done; echo second",
        gate.display()
    )
}

/// A turn that starts each of `commands` in the background.
fn starting(commands: &[String]) -> Vec<AssistantMessage> {
    let mut script: Vec<_> = commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            calling(
                "backgrounding it",
                &format!("call-bash-{index}"),
                "bash",
                serde_json::json!({
                    "command": command, "run_in_background": true, "description": "background"
                }),
            )
        })
        .collect();
    script.push(finalized_text_message("started"));
    script
}

/// A host with a session whose first turn started `commands` in the
/// background, and the id and pid of each recorded task.
async fn started(commands: &[String]) -> (Harness, String, Vec<(usize, u32)>) {
    let harness = Harness::new(starting(commands));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "start it").await;
    client.pump_until_idle().await;
    let recorded = harness.persistence.recorded_tasks(&session);
    assert_eq!(recorded.len(), commands.len(), "every launch is recorded");
    for (_, pid) in &recorded {
        assert!(process_running(*pid), "the recorded process runs");
    }
    (harness, session, recorded)
}

pub(super) async fn eventually(what: &str, mut holds: impl FnMut() -> bool) {
    bounded(what, async {
        while !holds() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// Wait until nothing runs and nothing waits to be delivered: no turn, no
/// task, and no notice that a wake has yet to deliver.
pub(super) async fn quiet(harness: &Harness, session: &str) {
    let tasks = registry(harness, session).await;
    bounded("the session to go quiet", async {
        loop {
            let list = harness.host.sessions().await.expect("sessions");
            let working = list
                .sessions
                .iter()
                .find(|entry| entry.id == session)
                .is_none_or(|entry| entry.working);
            if !working
                && tasks.pending_notices(AgentId::Main) == 0
                && running_tasks(&harness.host, session).await == 0
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Whether any process is left in the group `pid` leads.
pub(super) fn group_running(pid: u32) -> bool {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a pid"));
    nix::sys::signal::killpg(pid, None).is_ok()
}

pub(super) async fn registry(harness: &Harness, session: &str) -> aj_agent::TaskRegistry {
    let handles = harness.host.local_handles(session).await;
    handles.expect("live session").task_registry
}

fn tail(registry: &aj_agent::TaskRegistry, task: usize) -> String {
    registry.read(task).expect("a tracked task").1.stdout_tail
}

fn output_path(registry: &aj_agent::TaskRegistry, task: usize) -> PathBuf {
    let read = registry.read(task).expect("a tracked task").1;
    read.spill_path.expect("an output path")
}

/// The bodies of the task notifications in the main agent's stored transcript.
pub(super) async fn delivered(harness: &Harness, session: &str) -> Vec<String> {
    delivered_to(harness, session, AgentId::Main).await
}

/// The bodies of the task notifications in `agent`'s stored transcript.
pub(super) async fn delivered_to(harness: &Harness, session: &str, agent: AgentId) -> Vec<String> {
    let client = Client::attach(&harness.host, session).await;
    let state = client.canonical();
    let entries = &state.agent(agent).expect("its transcript").entries;
    entries
        .iter()
        .filter_map(|entry| match entry {
            aj_app::test_support::CanonicalEntry::TaskNotification { body, .. } => {
                Some(body.clone())
            }
            _ => None,
        })
        .collect()
}

async fn switch(
    harness: &Harness,
    session: &str,
    entry: &str,
) -> Result<CommandOutcome, HostError> {
    let head = Command::Head {
        changes: Default::default(),
        target: HeadTarget::Entry(entry.to_string()),
    };
    harness.host.command(session, head).await
}

async fn root_and_tip(harness: &Harness, session: &str) -> (String, String) {
    let handles = harness.host.local_handles(session).await.unwrap();
    let log = handles.log.lock().await;
    let root = log.system_prompt_id().cloned().expect("a root");
    (root, log.head().cloned().expect("a head"))
}

/// A turn has a thread from its first moment, so a command it starts
/// in the background is recorded: on the first turn of a new session (see
/// [`started`]) and on the first turn of a branch.
#[tokio::test]
async fn a_task_started_on_a_forks_first_turn_is_recorded() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let mut script = vec![finalized_text_message("done")];
    script.extend(starting(&[gated(&gate)]));
    script.push(finalized_text_message("noted"));
    let harness = Harness::new(script);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    client.pump_until_idle().await;
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    let (root, _) = root_and_tip(&harness, &session).await;
    switch(&harness, &session, &root)
        .await
        .expect("a fork point");

    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "start it").await;
    client.pump_until_idle().await;
    let recorded = harness.persistence.recorded_tasks(&session);
    assert_eq!(recorded.len(), 1, "the launch is recorded");
    let (task, pid) = recorded[0];
    assert!(process_running(pid), "the recorded process runs");
    let tasks = registry(&harness, &session).await;
    assert_eq!(tasks.status(task), Some(TaskStatus::Running));
    // It belongs to the branch: the thread it was started from has none.
    let transcript = harness.stored(&session).to_string();
    assert!(
        !transcript.contains("could not be recorded"),
        "{transcript}"
    );

    std::fs::write(&gate, "").unwrap();
    eventually("the process to be gone", || !process_running(pid)).await;
    quiet(&harness, &session).await;
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    harness.host.shutdown().await;
}

/// A task started in one host runs on when that host ends. The next host
/// tracks it under the id it had, reads what it writes afterwards, and
/// delivers its task notification once.
#[tokio::test]
async fn a_task_outlives_its_host_and_reports_to_the_next() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let (harness, session, recorded) = started(&[gated(&gate)]).await;
    let (task, pid) = recorded[0];
    let first = registry(&harness, &session).await;
    eventually("the first output", || tail(&first, task).contains("first")).await;
    let output = output_path(&first, task);

    harness.host.shutdown().await;
    assert!(process_running(pid), "the command outlives its host");
    assert_eq!(harness.persistence.recorded_tasks(&session), recorded);

    // The task's end wakes its owner, as it does in the host that started it.
    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let tasks = registry(&revived, &session).await;
    let summary = tasks.summary(task).expect("the task is tracked again");
    assert_eq!(summary.status, TaskStatus::Running);
    assert_eq!(summary.owner, AgentId::Main);
    assert_eq!(summary.call_id, "call-bash-0");
    assert_eq!(output_path(&tasks, task), output);
    eventually("the output so far", || tail(&tasks, task).contains("first")).await;
    assert!(!tail(&tasks, task).contains("second"));
    assert!(delivered(&revived, &session).await.is_empty());
    {
        // A client that attaches now is told of the running task.
        let client = Client::attach(&revived.host, &session).await;
        let shown = client.chat.tasks().get(&task).expect("in the task table");
        assert_eq!(shown.status, TaskStatus::Running);
    }

    std::fs::write(&gate, "").unwrap();
    let ended = bounded("the task to end", tasks.wait_terminal(task)).await;
    assert_eq!(ended, Some(TaskStatus::Exited(Some(0))));
    assert_eq!(tail(&tasks, task), "first\nsecond\n");
    eventually("the process to be gone", || !process_running(pid)).await;
    quiet(&revived, &session).await;
    let notices = delivered(&revived, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("Background task #1 finished:") && notices[0].contains("second"),
        "{notices:?}"
    );
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    // The output stays for the model to read. The exit file has no reader.
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "first\nsecond\n");
    assert!(!output.with_extension("exit").exists());
    revived.host.shutdown().await;

    // Delivered once: a third host has nothing left to deliver or track.
    let third = revived.revive(Vec::new());
    let tasks = registry(&third, &session).await;
    assert_eq!(tasks.pending_notices(AgentId::Main), 0);
    assert!(tasks.snapshot().is_empty());
    assert_eq!(delivered(&third, &session).await.len(), 1);
    third.host.shutdown().await;
}

/// A task that finished while no host was up has its notice saved by the
/// next host, which starts no turn for it. The notice is delivered once, by
/// the next turn.
#[tokio::test]
async fn a_task_that_finished_with_no_host_up_is_reported_on_reopen() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let (harness, session, recorded) = started(&[gated(&gate)]).await;
    let (task, pid) = recorded[0];
    harness.host.shutdown().await;
    std::fs::write(&gate, "").unwrap();
    eventually("the command to finish", || !process_running(pid)).await;
    assert_eq!(
        harness.persistence.recorded_tasks(&session),
        recorded,
        "nobody was there to see it end"
    );

    // An empty script: a turn started by the reopen would panic.
    let revived = harness.revive(Vec::new());
    let tasks = registry(&revived, &session).await;
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    assert_eq!(tasks.summary(task).map(|task| task.status), None);
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(delivered(&revived, &session).await.is_empty());
    revived.host.shutdown().await;

    // The notice was saved, not only queued: the next host has it too.
    let again = revived.revive(vec![finalized_text_message("noted")]);
    let tasks = registry(&again, &session).await;
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    let mut client = Client::attach(&again.host, &session).await;
    again
        .host
        .command(&session, Command::Continue)
        .await
        .expect("continue");
    client.pump_until_idle().await;
    let notices = delivered(&again, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("exit code 0") && notices[0].contains("first\nsecond"),
        "{notices:?}"
    );
    assert_eq!(tasks.pending_notices(AgentId::Main), 0);
    again.host.shutdown().await;

    let third = again.revive(Vec::new());
    assert_eq!(
        registry(&third, &session)
            .await
            .pending_notices(AgentId::Main),
        0
    );
    assert_eq!(delivered(&third, &session).await.len(), 1);
    third.host.shutdown().await;
}

/// A task whose process is gone without an exit status is reported lost,
/// with the output its file holds.
#[tokio::test]
async fn a_task_killed_without_an_exit_status_is_reported_lost() {
    let scratch = TempDir::new().unwrap();
    let (harness, session, recorded) = started(&[gated(&scratch.path().join("never"))]).await;
    let (task, pid) = recorded[0];
    let first = registry(&harness, &session).await;
    eventually("the first output", || tail(&first, task).contains("first")).await;
    let exit = output_path(&first, task).with_extension("exit");
    harness.host.shutdown().await;

    kill_task_group(pid);
    eventually("the process to be gone", || !group_running(pid)).await;
    // The host that started the command is this very process, which still
    // reaps it and writes the status it saw. A host that had really ended
    // would have written nothing, so that is the state put back here.
    eventually("the reaper's exit file", || exit.exists()).await;
    std::fs::remove_file(&exit).unwrap();

    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let tasks = registry(&revived, &session).await;
    assert_eq!(tasks.summary(task).map(|task| task.status), None);
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    let mut client = Client::attach(&revived.host, &session).await;
    revived
        .host
        .command(&session, Command::Continue)
        .await
        .expect("continue");
    client.pump_until_idle().await;
    let notices = delivered(&revived, &session).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("Background task #1 lost:")
            && notices[0].contains("status unknown")
            && notices[0].contains("first\n")
            && notices[0].contains("Full output: "),
        "{notices:?}"
    );
    revived.host.shutdown().await;
}

/// The next host stops a task it did not start: the user's kill and the
/// model's `task_stop` both end the real process. Ids go on from where the
/// first host left them.
#[tokio::test]
async fn a_reopened_host_stops_the_real_process() {
    let scratch = TempDir::new().unwrap();
    let never = scratch.path().join("never");
    let (harness, session, recorded) = started(&[gated(&never), gated(&never)]).await;
    let [(killed, killed_pid), (stopped, stopped_pid)] = recorded[..] else {
        panic!("two tasks: {recorded:?}");
    };
    assert_eq!((killed, stopped), (1, 2));
    harness.host.shutdown().await;

    let revived = harness.revive(vec![
        calling(
            "stopping it",
            "call-stop",
            "task_stop",
            serde_json::json!({"id": stopped}),
        ),
        calling(
            "one more",
            "call-more",
            "bash",
            serde_json::json!({
                "command": "true", "run_in_background": true, "description": "background"
            }),
        ),
        finalized_text_message("stopped"),
        // The wakes for the ended tasks.
        finalized_text_message("noted"),
        finalized_text_message("noted"),
        finalized_text_message("noted"),
    ]);
    let tasks = registry(&revived, &session).await;
    assert_eq!(tasks.snapshot().len(), 2);

    // The model's stop, by the agent that owns the task.
    let mut client = Client::attach(&revived.host, &session).await;
    revived.prompt(&session, "stop the second").await;
    client.pump_until_idle().await;
    let result = main_tool_content(&client.canonical(), "call-stop").to_string();
    assert!(result.contains("killed"), "{result}");
    assert!(result.contains("first"), "its output is read: {result}");
    assert_eq!(tasks.status(stopped), Some(TaskStatus::Killed));
    eventually("the stopped group to be gone", || {
        !group_running(stopped_pid)
    })
    .await;
    assert!(process_running(killed_pid), "the other task is untouched");
    let started = main_tool_content(&client.canonical(), "call-more").to_string();
    assert!(
        started.contains("Started background task #3"),
        "ids are not used twice: {started}"
    );

    // The user's kill.
    revived
        .host
        .command(&session, Command::KillTask { task: killed })
        .await
        .expect("kill");
    let ended = bounded("the task to end", tasks.wait_terminal(killed)).await;
    assert_eq!(ended, Some(TaskStatus::Killed));
    assert!(!group_running(killed_pid), "the kill reached the process");

    quiet(&revived, &session).await;
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    let notices = delivered(&revived, &session).await;
    assert_eq!(notices.len(), 3, "one notice a task: {notices:?}");
    revived.host.shutdown().await;
}

/// A task's end that cannot be saved has not happened as far as anyone is
/// told: the task stays recorded and running, and nothing is delivered from
/// memory. The save is tried again, and the notice is then delivered once.
#[tokio::test]
#[cfg_attr(
    not(feature = "slow-tests"),
    ignore = "slow: waits for the driver to try the save again"
)]
async fn a_task_end_that_cannot_be_saved_leaves_the_task_recorded() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let harness = Harness::new(vec![
        starting(&[gated(&gate)]).remove(0),
        finalized_text_message("started"),
        finalized_text_message("noted"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "start it").await;
    client.pump_until_idle().await;
    let recorded = harness.persistence.recorded_tasks(&session);
    let [(task, pid)] = recorded[..] else {
        panic!("one task: {recorded:?}");
    };
    let tasks = registry(&harness, &session).await;

    harness.persistence.refuse_sqlite_pending_writes();
    std::fs::write(&gate, "").unwrap();
    eventually("the command to finish", || !process_running(pid)).await;
    // Long enough for the driver to see the end and be refused once.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        harness.persistence.recorded_tasks(&session),
        recorded,
        "the record outlives a notice that could not be saved"
    );
    assert_eq!(tasks.status(task), Some(TaskStatus::Running));
    assert_eq!(tasks.pending_notices(AgentId::Main), 0);

    harness.persistence.allow_sqlite_pending_writes();
    let ended = bounded("the save to be tried again", tasks.wait_terminal(task)).await;
    assert_eq!(ended, Some(TaskStatus::Exited(Some(0))));
    quiet(&harness, &session).await;
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    assert_eq!(delivered(&harness, &session).await.len(), 1);
    harness.host.shutdown().await;
}

/// A launch whose record is refused fails, and no command runs: nothing is
/// left that no record accounts for.
#[tokio::test]
async fn a_launch_that_cannot_be_recorded_fails_and_runs_nothing() {
    let scratch = TempDir::new().unwrap();
    let marker = scratch.path().join("ran");
    let command = format!("touch '{}'; sleep 120", marker.display());
    let harness = Harness::new(starting(std::slice::from_ref(&command)));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.persistence.refuse_sqlite_task_writes(true, false);
    harness.prompt(&session, "start it").await;
    client.pump_until_idle().await;

    let result = main_tool_content(&client.canonical(), "call-bash-0").to_string();
    assert!(result.contains("was not run"), "{result}");
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    assert!(registry(&harness, &session).await.snapshot().is_empty());
    // The wrapper names the marker in its arguments, so it is found by it.
    let mentions = |needle: &str| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|entry| {
                std::fs::read(entry.path().join("cmdline"))
                    .is_ok_and(|line| String::from_utf8_lossy(&line).contains(needle))
            })
            .count()
    };
    let needle = marker.display().to_string();
    eventually("the wrapper to be gone", || mentions(&needle) == 0).await;
    assert!(!marker.exists(), "the command ran without a record");
    let spill = harness._dir.path().join("spill");
    let left: Vec<_> = std::fs::read_dir(&spill)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    harness.host.shutdown().await;
}

/// A head switch waits for a branch's tasks and their notices: it is
/// refused while a task runs and while its notice is undelivered, and it
/// lets go of nothing. Once the notice is in the transcript the branch can
/// be left.
#[tokio::test]
async fn a_head_switch_waits_for_a_branchs_tasks_and_their_notices() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let (harness, session, recorded) = started(&[gated(&gate)]).await;
    let (task, pid) = recorded[0];
    let (root, tip) = root_and_tip(&harness, &session).await;
    let tasks = registry(&harness, &session).await;
    let refusal = |refused: Result<CommandOutcome, HostError>| match refused {
        Err(HostError::Conflict { reason }) => reason,
        other => panic!("the switch was not refused: {other:?}"),
    };

    let said = refusal(switch(&harness, &session, &root).await);
    assert!(
        said.contains("1 background tasks are still running") && said.contains("stop them"),
        "{said}"
    );
    assert!(process_running(pid), "a refused switch stops nothing");
    assert_eq!(harness.persistence.recorded_tasks(&session), recorded);
    assert_eq!(tasks.status(task), Some(TaskStatus::Running));
    assert_eq!(root_and_tip(&harness, &session).await.1, tip);

    // The task ends with no host up, so the next one restores its notice
    // and starts no turn for it. The notice holds the head as well.
    harness.host.shutdown().await;
    std::fs::write(&gate, "").unwrap();
    eventually("the command to finish", || !process_running(pid)).await;
    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let tasks = registry(&revived, &session).await;
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    let said = refusal(switch(&revived, &session, &root).await);
    assert!(
        said.contains("notification is waiting") && said.contains("with Continue"),
        "{said}"
    );
    assert_eq!(tasks.pending_notices(AgentId::Main), 1);
    assert_eq!(root_and_tip(&revived, &session).await.1, tip);

    let mut client = Client::attach(&revived.host, &session).await;
    revived
        .host
        .command(&session, Command::Continue)
        .await
        .expect("continue");
    client.pump_until_idle().await;
    assert_eq!(delivered(&revived, &session).await.len(), 1);
    switch(&revived, &session, &root)
        .await
        .expect("nothing is left on the branch");
    assert_eq!(root_and_tip(&revived, &session).await.1, root);
    assert!(tasks.snapshot().is_empty(), "the other branch has no tasks");
    revived.host.shutdown().await;
}

/// A sub-agent's task is recorded under the sub-agent's thread, and
/// the next host takes it up for that sub-agent: it is tracked under its
/// id, and its end wakes the sub-agent, which is told once. The main agent
/// is told nothing. In the host that started it, it holds the head, as any
/// running task does.
#[tokio::test]
async fn a_sub_agents_task_outlives_its_host_and_reports_to_the_sub_agent() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let harness = Harness::new(vec![
        calling(
            "delegating that",
            "call-sub",
            "agent",
            serde_json::json!({"task": "run it"}),
        ),
        calling(
            "backgrounding it",
            "call-bash",
            "bash",
            serde_json::json!({
                "command": gated(&gate), "run_in_background": true, "description": "background"
            }),
        ),
        finalized_text_message("the sub started it"),
        finalized_text_message("delegated"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    let recorded = harness.persistence.recorded_tasks(&session);
    let [(task, pid)] = recorded[..] else {
        panic!("one task: {recorded:?}");
    };
    let tasks = registry(&harness, &session).await;
    assert_eq!(
        tasks.summary(task).map(|task| task.owner),
        Some(AgentId::Sub(1))
    );

    let (root, _) = root_and_tip(&harness, &session).await;
    let refused = switch(&harness, &session, &root).await;
    assert!(
        matches!(refused, Err(HostError::Conflict { .. })),
        "{refused:?}"
    );
    harness.host.shutdown().await;
    assert!(process_running(pid));

    // The wake the task's end starts on the sub-agent.
    let revived = harness.revive(vec![finalized_text_message("noted")]);
    let tasks = registry(&revived, &session).await;
    let summary = tasks.summary(task).expect("the task is tracked again");
    assert_eq!(summary.status, TaskStatus::Running);
    assert_eq!(summary.owner, AgentId::Sub(1));
    assert_eq!(summary.call_id, "call-bash");
    let handles = revived.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.registry.ids(), [1], "its owner is live again");
    assert_eq!(harness.persistence.recorded_tasks(&session), recorded);

    std::fs::write(&gate, "").unwrap();
    let ended = bounded("the task to end", tasks.wait_terminal(task)).await;
    assert_eq!(ended, Some(TaskStatus::Exited(Some(0))));
    bounded("the sub-agent to be told", async {
        while delivered_to(&revived, &session, AgentId::Sub(1))
            .await
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    quiet(&revived, &session).await;
    let notices = delivered_to(&revived, &session, AgentId::Sub(1)).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("Background task #1 finished:") && notices[0].contains("second"),
        "{notices:?}"
    );
    assert!(delivered(&revived, &session).await.is_empty());
    assert!(harness.persistence.recorded_tasks(&session).is_empty());
    revived.host.shutdown().await;

    // Delivered once: a third host has nothing left to deliver or track.
    let third = revived.revive(Vec::new());
    let tasks = registry(&third, &session).await;
    assert_eq!(tasks.pending_notices(AgentId::Sub(1)), 0);
    assert!(tasks.snapshot().is_empty());
    let notices = delivered_to(&third, &session, AgentId::Sub(1)).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    third.host.shutdown().await;
}

/// `aj doctor` lists a task record with its session, the agent that
/// started it and its task id, and says whether its process still runs. It
/// changes nothing.
#[tokio::test]
async fn doctor_lists_a_recorded_command_and_whether_it_still_runs() {
    let scratch = TempDir::new().unwrap();
    let gate = scratch.path().join("gate");
    let (harness, session, recorded) = started(&[gated(&gate)]).await;
    let (task, pid) = recorded[0];
    // No host is up from here on, so the row stays whatever the command does.
    harness.host.shutdown().await;
    let report = || aj_app::task_records_report(&harness.persistence).expect("a report");

    let lines = report();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "task records: 1");
    let row = format!("/{session}  main  task #{task}  pid {pid}  started ");
    assert!(
        lines[1].contains(&row) && lines[1].contains("Z  running  echo first; while"),
        "{lines:?}"
    );

    std::fs::write(&gate, "").unwrap();
    eventually("the command to finish", || !process_running(pid)).await;
    let lines = report();
    assert!(
        lines[1].contains(&row) && lines[1].contains("Z  not running  echo first; while"),
        "{lines:?}"
    );
    assert_eq!(
        harness.persistence.recorded_tasks(&session),
        recorded,
        "the report changed nothing"
    );
}
