//! What a cancelled turn does to a command's processes, driven through
//! the real seam.
//!
//! The driver (`aj_agent`) races a tool against the turn's cancellation
//! token and drops the losing future, so a test that calls
//! `BashTool::execute` directly never sees that drop and proves
//! nothing about it. These tests run the production path: a real
//! `Agent` over a scripted provider, the real `bash` tool, and the
//! turn's own token.
//!
//! Linux-only: liveness is read from `/proc`, which is also the table
//! the harm was measured at.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aj_agent::{Agent, AgentSeed, TurnError};
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ProviderScript, ScriptedProvider};
use aj_models::streaming::{AssistantMessageEvent, DoneReason};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent, ToolCall,
};
use aj_tools::BashTool;
use aj_tools::tools::bash::KILL_GRACE;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SCRIPTED: &str = "scripted";

/// How long a teardown may take: the guard's `SIGTERM`, its grace, the
/// `SIGKILL`, and room for a loaded machine.
const TEARDOWN_BOUND: Duration = Duration::from_secs(8);

fn model_info() -> ModelInfo {
    ModelInfo {
        id: SCRIPTED.to_string(),
        name: SCRIPTED.to_string(),
        family: None,
        api: SCRIPTED.to_string(),
        provider: SCRIPTED.to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
    }
}

fn message(content: Vec<AssistantContent>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: SCRIPTED.to_string(),
        provider: SCRIPTED.to_string(),
        model: SCRIPTED.to_string(),
        account: None,
        response_id: None,
        usage: Default::default(),
        stop_reason,
        error: None,
        timestamp: 0,
    }
}

fn script(message: AssistantMessage, reason: DoneReason) -> Vec<AssistantMessageEvent> {
    vec![
        AssistantMessageEvent::Start {
            partial: message.clone(),
        },
        AssistantMessageEvent::Done { reason, message },
    ]
}

/// One scripted inference that finalizes on a single `bash` call.
fn bash_call(command: &str, background: bool) -> Vec<AssistantMessageEvent> {
    script(
        message(
            vec![AssistantContent::ToolCall(ToolCall {
                id: "c-1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({
                    "command": command,
                    "description": "test command",
                    "timeout": 60,
                    "run_in_background": background,
                }),
            })],
            StopReason::ToolUse,
        ),
        DoneReason::ToolUse,
    )
}

/// One scripted inference that finalizes on text, after `delay` so a
/// turn can still be cancelled while it is in flight.
fn text_after(delay: Duration, body: &str) -> ProviderScript {
    script(
        message(
            vec![AssistantContent::Text(TextContent {
                text: body.to_string(),
                text_signature: None,
            })],
            StopReason::Stop,
        ),
        DoneReason::Stop,
    )
    .into_iter()
    .fold(ProviderScript::new(), |s, event| s.push(delay, event))
}

/// An agent running the real `bash` tool, spilling into a directory the
/// caller owns: a background task persists its spill by contract, so
/// the ambient temp directory would outlive the test.
fn agent_running(scripts: Vec<ProviderScript>, spill_dir: &Path) -> Agent {
    let provider: Arc<dyn Provider> =
        Arc::new(ScriptedProvider::new(scripts).on_exhausted(ExhaustedBehavior::Panic));
    let mut agent = Agent::with_provider(
        std::env::temp_dir(),
        vec![BashTool::new(false, Some(spill_dir.to_path_buf())).into()],
        Vec::new(),
        provider,
        Arc::new(model_info()),
        StreamOptions::default(),
        None,
    );
    agent.seed_session(AgentSeed {
        assembled_system_prompt: Some("test system prompt".to_string()),
        ..AgentSeed::default()
    });
    agent
}

/// Whether `pid` is a live process. Reads `/proc` rather than
/// signalling: signal 0 also succeeds for a zombie that nobody has
/// reaped yet, which would read as alive right after a kill.
fn process_is_live(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(after_comm.split_whitespace().next(), Some("Z") | None)
}

fn read_pid(path: &Path) -> i32 {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.trim()
        .parse()
        .unwrap_or_else(|_| panic!("fixture should have recorded a pid, file held {raw:?}"))
}

/// Spin until `cond` holds, bounded, yielding to the runtime so the
/// guard's spawned teardown can actually run.
async fn wait_until(mut cond: impl FnMut() -> bool, bound: Duration, what: &str) {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out after {bound:?} waiting for {what}");
}

async fn wait_for_file(path: &Path) {
    let owned = path.to_path_buf();
    wait_until(
        || {
            std::fs::metadata(&owned)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        },
        Duration::from_secs(10),
        "the command to record its pid",
    )
    .await;
}

/// A command that records the shell's pid and a forked descendant's,
/// then holds both alive until `dir` goes away. The descendant is what
/// the process group exists for: killing the immediate child alone
/// leaves it running, which is the leak this bead measured on two
/// seats.
fn holding_command(dir: &Path, shell_pid: &Path, descendant_pid: &Path) -> String {
    format!(
        "{{ echo $BASHPID > '{descendant}'; \
            while [ -d '{dir}' ] && [ $SECONDS -lt 60 ]; do sleep 0.05; done; }} & \
         until [ -s '{descendant}' ]; do sleep 0.01; done; \
         echo $$ > '{shell}'; \
         while [ -d '{dir}' ] && [ $SECONDS -lt 60 ]; do sleep 0.05; done",
        descendant = descendant_pid.display(),
        shell = shell_pid.display(),
        dir = dir.display(),
    )
}

struct Fixture {
    dir: TempDir,
    shell_pid: PathBuf,
    descendant_pid: PathBuf,
    command: String,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().expect("create temp dir");
    let shell_pid = dir.path().join("shell-pid");
    let descendant_pid = dir.path().join("descendant-pid");
    let command = holding_command(dir.path(), &shell_pid, &descendant_pid);
    Fixture {
        dir,
        shell_pid,
        descendant_pid,
        command,
    }
}

/// Cancelling a turn kills the whole process group of the foreground
/// command it was running, and the cancel itself stays immediate: the
/// teardown is spawned, never awaited by the turn.
#[tokio::test]
async fn cancelling_a_turn_kills_the_commands_process_group() {
    let fx = fixture();
    let mut agent = agent_running(
        vec![
            ProviderScript::from_events(bash_call(&fx.command, false)),
            text_after(Duration::ZERO, "done"),
        ],
        fx.dir.path(),
    );

    let cancel = CancellationToken::new();
    let fired = Arc::new(std::sync::Mutex::new(None));
    let canceller = {
        let cancel = cancel.clone();
        let fired = Arc::clone(&fired);
        let pid_path = fx.shell_pid.clone();
        tokio::spawn(async move {
            wait_for_file(&pid_path).await;
            *fired.lock().unwrap() = Some(Instant::now());
            cancel.cancel();
        })
    };

    let outcome = agent.prompt("run it".to_string(), cancel).await;
    let returned = Instant::now();
    canceller.await.expect("canceller");

    assert!(
        matches!(outcome, Err(TurnError::Aborted)),
        "a cancelled turn aborts: {outcome:?}"
    );
    let fired = fired.lock().unwrap().expect("the token was fired");
    assert!(
        returned.duration_since(fired) < KILL_GRACE,
        "cancel stays immediate, the turn must not wait out the teardown: took {:?}",
        returned.duration_since(fired)
    );

    // The harm this bead measured was at the process table, so that is
    // where it is asserted: both the shell and the descendant it forked
    // into the same group.
    let shell = read_pid(&fx.shell_pid);
    let descendant = read_pid(&fx.descendant_pid);
    assert_ne!(shell, descendant, "the fixture forked a real descendant");
    wait_until(
        || !process_is_live(shell) && !process_is_live(descendant),
        TEARDOWN_BOUND,
        "the cancelled command's process group to be killed",
    )
    .await;
}

/// The teardown gives the group a chance before it kills it: a command
/// that cleans up on `SIGTERM` gets to run its handler, and is killed
/// only if it is still holding on after the grace.
///
/// The grace is why the guard's escalation runs on a timer. Keyed on
/// `Child::wait` it would answer instantly whenever the child was
/// already reaped, and the `SIGKILL` would tread on the handler's
/// heels.
#[tokio::test]
async fn a_cancelled_command_gets_its_sigterm_handler() {
    let dir = TempDir::new().expect("create temp dir");
    let shell_pid = dir.path().join("shell-pid");
    let termed = dir.path().join("was-termed");
    let command = format!(
        "trap \"echo termed > '{termed}'; exit\" TERM; \
         echo $$ > '{shell}'; \
         while [ -d '{dir}' ] && [ $SECONDS -lt 60 ]; do sleep 0.05; done",
        termed = termed.display(),
        shell = shell_pid.display(),
        dir = dir.path().display(),
    );

    let mut agent = agent_running(
        vec![
            ProviderScript::from_events(bash_call(&command, false)),
            text_after(Duration::ZERO, "done"),
        ],
        dir.path(),
    );
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let pid_path = shell_pid.clone();
        tokio::spawn(async move {
            wait_for_file(&pid_path).await;
            cancel.cancel();
        })
    };

    let outcome = agent.prompt("run it".to_string(), cancel).await;
    canceller.await.expect("canceller");
    assert!(
        matches!(outcome, Err(TurnError::Aborted)),
        "a cancelled turn aborts: {outcome:?}"
    );

    let pid = read_pid(&shell_pid);
    wait_until(
        || !process_is_live(pid),
        TEARDOWN_BOUND,
        "the cancelled command to go away",
    )
    .await;
    assert!(
        termed.exists(),
        "the command should have run its SIGTERM handler before anything killed it: \
         without the grace this only measures the kill"
    );
}

/// The companion rule: a background task outlives the turn that started
/// it. Its lifetime moved to the task registry at the handoff, so a
/// cancel of that turn must not touch it.
#[tokio::test]
async fn cancelling_a_turn_leaves_its_background_task_running() {
    let fx = fixture();
    let mut agent = agent_running(
        vec![
            ProviderScript::from_events(bash_call(&fx.command, true)),
            // The launch returns at once, so the turn is cancelled
            // while this inference is still in flight.
            text_after(Duration::from_secs(30), "done"),
        ],
        fx.dir.path(),
    );

    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let pid_path = fx.shell_pid.clone();
        tokio::spawn(async move {
            wait_for_file(&pid_path).await;
            cancel.cancel();
        })
    };

    let outcome = agent.prompt("run it".to_string(), cancel).await;
    canceller.await.expect("canceller");
    assert!(
        matches!(outcome, Err(TurnError::Aborted)),
        "a cancelled turn aborts: {outcome:?}"
    );

    // Long enough that a teardown would have finished: the guard's
    // grace plus its kill, with margin.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let shell = read_pid(&fx.shell_pid);
    let descendant = read_pid(&fx.descendant_pid);
    assert!(
        process_is_live(shell) && process_is_live(descendant),
        "outliving the turn is background mode's whole point, so the guard must have \
         disarmed at the handoff: shell alive {}, descendant alive {}",
        process_is_live(shell),
        process_is_live(descendant),
    );
}
