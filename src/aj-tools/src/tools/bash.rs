//! `bash` builtin — execute a command in the system shell.
//!
//! Implements [`aj_agent::tool::ToolDefinition`]. Returns a
//! [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Bash`] on completion, carrying a
//! bounded rolling tail of `stdout` / `stderr`, the process exit code,
//! a `truncated` flag, an optional `full_output_path` pointing at a
//! temp file with the complete (un-truncated) capture, and per-stream
//! [`BashStreamTruncation`] summaries with the line/byte totals
//! renderers use to compose `[Showing lines X-Y of TOTAL ...]`
//! markers. The wire `content` keeps the text shape
//! (`<stdout>` then `STDERR:` then `Command failed with exit code: N`)
//! so the model reads the same transcript it always has, with each
//! affected stream's marker inserted right after its content when
//! truncation occurred.
//!
//! Output handling:
//!
//! - **Bounded tail.** Each stream is capped at
//!   [`crate::truncate::BASH_MAX_BYTES`] / [`crate::truncate::BASH_MAX_LINES`]
//!   for what the model sees; in memory we keep a rolling window up to
//!   [`ROLLING_CAP_BYTES`] (2× the byte cap) and lazily trim it back
//!   whenever it crosses [`TRIM_TRIGGER_BYTES`] (4× the byte cap).
//!   Bytes beyond that get evicted from the in-memory tail but still
//!   land in the spill file. The first time the source overflows
//!   either cap, `truncated` flips to `true` and the spill path is
//!   surfaced in the structured payload so the user (and the TUI) can
//!   open the full transcript on demand.
//! - **Spill file.** A [`tempfile::NamedTempFile`] is created up-front
//!   with prefix `aj-bash-` and suffix `.log`; both reader tasks tee
//!   into it as bytes flow. If no truncation occurred at completion the
//!   `NamedTempFile` is dropped (cleaning up the file). Otherwise
//!   `keep()` persists it and we surface the resulting path. Background
//!   tasks persist it unconditionally — the spill is the canonical
//!   full output named in the started result and the completion
//!   notice.
//! - **Progress updates.** While the child runs the implementation
//!   self-throttles `ToolContext::emit_update` to one snapshot per
//!   [`UPDATE_DEBOUNCE`] (~10/s) using a leading-edge fire so the first
//!   byte of output lights up the UI without waiting for the next
//!   tick. Live snapshots carry the boolean `truncated` flag but not
//!   the structured per-stream summary (that's only meaningful once
//!   the stream has closed).
//! - **Cancellation / timeout.** The child is launched in a fresh
//!   process group (`process_group(0)`) so we can signal every
//!   descendant the shell forked, which a plain `Child::kill()` would
//!   leak. On Unix we `SIGTERM` the process group, give it a grace
//!   period, then escalate to `SIGKILL`. The timeout path falls back
//!   to killing just the immediate child on non-Unix. The drop path
//!   has no such fallback, so there a cancelled command's processes
//!   survive and only the tool's own descriptors are released. A
//!   descendant that left the group (`setsid`) is out of reach of all
//!   of this, which is what the capture drain below bounds.
//!   Every post-`SIGKILL` wait has its own [`KILL_GRACE`] bound. If the
//!   kernel cannot make the leader reapable within it, the guard releases
//!   the child handle, capture readers, and session cleanup lease together.
//!
//!   Timeout runs through the `select!` loop's own arm. Cancellation
//!   mostly does not: the driver drops this future instead of polling
//!   it again, so the loop's cancel arm only runs in the narrow window
//!   where the token fires while a poll is in flight. What actually
//!   tears a cancelled command down is [`ProcessGuard`], on drop.
//! - **Capture drain.** Capture ends when the command ends. The reader
//!   tasks stop at EOF, EOF needs every write end of the pipes closed,
//!   and a process that outlives the command holds one unless it
//!   redirected the tool's stdout and stderr away. So once the command
//!   has ended, for any reason, the drain waits
//!   [`CAPTURE_DRAIN_GRACE`] for the pipes to close and then takes
//!   them back: `SIGTERM` to the group, the same grace again,
//!   `SIGKILL`, and finally aborting the reader tasks, which drops the
//!   read ends whatever still holds the far side. A run that got that
//!   far says so in a trailer, see [`capture_cut_trailer`].
//! - **`Sequential` execution.** `bash` runs arbitrary commands, so it
//!   runs in `Sequential` mode: a batch containing it serializes
//!   around any other in-flight tool calls.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::tool::{
    BashStreamTruncation, ExecutionMode, StartedTask, TaskEventSink, TaskId, TaskKind, TaskNotice,
    TaskOutputSource, TaskRead, TaskStatus, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
};
use aj_agent::{TaskCleanupGuard, TaskRegistry};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::truncate::{BASH_MAX_BYTES, BASH_MAX_LINES, TruncatedBy, format_size, truncate_tail};

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash). The command will be run in the
working directory of the agent session.

- There are no permissions checks or sandboxing. You are free to run any command
  you consider reasonable and safe.
- Commands have a configurable timeout to prevent hanging (default: 30s). It
  bounds the command itself: a process the command leaves behind still holding
  its stdout or stderr gets a couple of seconds to let go, after which its
  process group is killed and the reported output may be incomplete.
- Output is truncated to the last 2000 lines or 50KB per stream (whichever
  fires first). When truncated, the full output is saved to a temp file and
  the marker points at it.
- The command is passed to `bash -c`, so pipes, redirects, and shell features work.
- For file search, prefer `rg` (ripgrep) over `grep`/`find` — it's faster and
  respects `.gitignore` by default. Use `read_file` for reading file contents
  rather than `cat`.
- Set `run_in_background: true` for long-running work: the call returns
  immediately with a task id and the path the output is written to, which you
  can read with read_file (supports offset/limit). `timeout` is ignored in
  background mode: the task runs until it exits or is stopped, and you are
  notified when it completes.
- For "wait until X is ready", background a command that exits when the
  condition holds (e.g. `until grep -q "Ready" dev.log; do sleep 0.5; done`)
  — one task, one completion notice — instead of polling in the foreground.
- You do not need to wait for a background task. Keep working and the
  completion notice reaches you once it finishes. Never wait by sleeping in
  the foreground: no notice can arrive while a foreground command is running,
  so sleeping only delays the report by the length of the sleep.
- Double-forking daemons escape the process group, so stopping the task can't
  kill them. Prefer supervising the process in the foreground of a background
  task over nohup-style detachment.
"#;

/// Maximum bytes preserved per stream in the in-memory rolling tail
/// after a trim. Twice the byte cap so the post-trim window always
/// contains the full last `BASH_MAX_BYTES` of the stream plus a
/// buffer for the next chunk, which keeps the truncate-tail finaliser
/// free to drop a leading partial line without losing visible bytes.
const ROLLING_CAP_BYTES: usize = BASH_MAX_BYTES * 2;
/// Trim trigger. We trim the rolling tail back to [`ROLLING_CAP_BYTES`]
/// once its size crosses this threshold; in between trims the tail is
/// allowed to grow up to this size, amortising the cost of shifting
/// bytes out of the front of a `Vec<u8>`.
const TRIM_TRIGGER_BYTES: usize = ROLLING_CAP_BYTES * 2;

/// Minimum spacing between `emit_update` snapshots. ~10 events per
/// second, with a leading-edge fire so the very first chunk of output
/// reaches a renderer without waiting for the next tick.
const UPDATE_DEBOUNCE: Duration = Duration::from_millis(100);

/// Maximum time the host-side rtk hook may spend deciding whether to rewrite.
const RTK_HOOK_TIMEOUT: Duration = Duration::from_millis(500);

/// Grace period a terminated command's process group gets to exit on
/// `SIGTERM` before we escalate to `SIGKILL`.
///
/// Kept comfortably below `task_stop`'s own `STOP_GRACE` (5s) so that a
/// `task_stop` blocking on the status flip still observes the kill
/// within its budget, even for a command that ignores `SIGTERM`.
///
/// Public because it is the teardown's whole budget, and a caller (or a
/// test) that wants to say "this returned without waiting out the
/// teardown" has to say it in terms of this.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long the capture pipes get to close once the command has ended.
///
/// Deliberately a constant rather than a slice of the command's
/// `timeout`: this is not "how long may the command run" but "how long
/// do we wait for its pipes to close afterwards", and a slice would
/// make the bound depend on when the child exited (an exit one second
/// into a 30s budget would leave a 29s wait, which is most of the
/// hang it is meant to prevent).
const CAPTURE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Window for the pipes to close after the escalation `SIGKILL`. The
/// kernel closes a killed process's descriptors, so this covers
/// scheduling the readers, not any cleanup of theirs.
const CAPTURE_CLOSE_GRACE: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct BashTool {
    /// When true, eligible commands are dispatched through `rtk`
    /// (https://github.com/rtk-ai/rtk) to compress their output before
    /// it reaches the model. See [`rtk_rewrite`] for the eligibility
    /// rules and [`find_rtk_on_path`] for the PATH probe.
    rtk: bool,
    /// Where spill files are written. `None` uses the ambient temp
    /// directory, which is what an unset `spill_dir` config means.
    spill_dir: Option<PathBuf>,
}

impl BashTool {
    /// Construct with the given rtk passthrough setting and spill directory.
    pub fn new(rtk: bool, spill_dir: Option<PathBuf>) -> Self {
        Self { rtk, spill_dir }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            rtk: false,
            spill_dir: None,
        }
    }
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct BashInput {
    /// The command to execute in the shell.
    pub command: String,
    /// Timeout in seconds after which the command will be cancelled (default: 30).
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// A description explaining what the command does and why you want to run it.
    pub description: String,
    /// Run the command in the background. The call returns immediately
    /// with a task id and the output path, and a completion notice
    /// arrives when the task finishes. `timeout` is ignored in
    /// background mode, the task runs until it exits or is stopped.
    #[serde(default)]
    pub run_in_background: bool,
}

fn default_timeout() -> u64 {
    30
}

impl BashTool {
    /// Return the `rtk`-rewritten command if passthrough is enabled,
    /// or `None` to run the command verbatim. Delegates the actual
    /// rewriting to `rtk hook check`, the dry-run form of the same
    /// engine the Claude Code / Cursor / Gemini PreToolUse hooks use,
    /// so we inherit rtk's shell-aware handling of compounds, pipes,
    /// and `env`/`sudo` prefixes instead of maintaining our own
    /// approximation of rtk's subcommand catalog. The helper belongs to the
    /// host: selection and hook checking use plain process inheritance, while
    /// the accepted rewrite binds that selected absolute executable before the
    /// session overlay reaches Bash.
    async fn rtk_rewrite(
        &self,
        command: &str,
        working_dir: &Path,
        tasks: &TaskRegistry,
    ) -> Option<String> {
        let host_path = std::env::var_os("PATH");
        self.rtk_rewrite_with_host_path(command, host_path.as_deref(), working_dir, tasks)
            .await
    }

    async fn rtk_rewrite_with_host_path(
        &self,
        command: &str,
        host_path: Option<&OsStr>,
        working_dir: &Path,
        tasks: &TaskRegistry,
    ) -> Option<String> {
        if !self.rtk {
            return None;
        }
        // An existing shell name can define, alias, or directly invoke rtk.
        // In that case there is no unambiguous way to distinguish the hook's
        // insertion from the caller's command, so passthrough stays off.
        if contains_shell_identifier(command, "rtk") {
            return None;
        }
        let executable = find_rtk_on_path(host_path)?;
        let rewritten =
            rtk_hook_check(&executable, working_dir, command, tasks.track_cleanup()).await?;
        bind_rtk_rewrite(&rewritten, &executable)
    }
}

/// Find the host-owned `rtk` executable on the inherited process PATH.
///
/// Relative and empty entries cannot name one stable absolute helper after the
/// hook changes directory, so passthrough declines when the host PATH contains
/// one. Non-executable regular files are skipped just as shell PATH lookup skips
/// them in favor of a later executable. The selected absolute path is bound into
/// every accepted rewrite.
fn find_rtk_on_path(paths: Option<&OsStr>) -> Option<PathBuf> {
    let Some(paths) = paths else {
        return None;
    };
    let directories: Vec<PathBuf> = std::env::split_paths(paths).collect();
    if directories.iter().any(|dir| !dir.is_absolute()) {
        return None;
    }
    directories
        .into_iter()
        .map(|dir| dir.join("rtk"))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(candidate: &Path) -> bool {
    let Ok(metadata) = candidate.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use nix::unistd::{AccessFlags, access};

        access(candidate, AccessFlags::X_OK).is_ok()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Bind every helper command introduced by the hook to the executable selected
/// by the PATH probe. Callers reject an original command containing standalone
/// `rtk`, so every standalone `rtk ` in the hook's answer belongs to the hook
/// even when it canonicalizes another command (`rg` to `grep`, for example).
fn bind_rtk_rewrite(rewritten: &str, executable: &Path) -> Option<String> {
    let executable = executable.to_str()?;
    let quoted = format!("'{}'", executable.replace('\'', "'\"'\"'"));
    let bytes = rewritten.as_bytes();
    let mut bound = String::with_capacity(rewritten.len() + quoted.len());
    let mut copied_to = 0;
    let mut replacements = 0;
    for (start, _) in rewritten.match_indices("rtk") {
        let before_is_name = start
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        let after = start + "rtk".len();
        let after_is_space = bytes.get(after).is_some_and(u8::is_ascii_whitespace);
        if before_is_name || !after_is_space {
            continue;
        }
        bound.push_str(&rewritten[copied_to..start]);
        bound.push_str(&quoted);
        copied_to = after;
        replacements += 1;
    }
    if replacements == 0 {
        return None;
    }
    bound.push_str(&rewritten[copied_to..]);
    Some(bound)
}

fn contains_shell_identifier(command: &str, identifier: &str) -> bool {
    let bytes = command.as_bytes();
    command.match_indices(identifier).any(|(start, _)| {
        let before = start
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        let after = bytes
            .get(start + identifier.len())
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        !before && !after
    })
}

/// Ask `rtk` whether it would rewrite `command`, via the dry-run form
/// of its hook engine (`rtk hook check <command>`). On a rewrite this
/// returns the rewritten string (e.g. `git status` -> `rtk git
/// status`, `cargo fmt && cargo check` -> `rtk cargo fmt && rtk cargo
/// check`). Returns `None` on no-rewrite, on rtk being missing or
/// misbehaving, or on timeout. In every case the caller falls back to
/// running the original command verbatim.
///
/// `command` is passed as a single argv element so shell
/// metacharacters in it are never interpreted by a shell at this
/// layer. rtk parses the string itself. The 500ms timeout guards
/// against a wedged rtk blocking the tool call. The hook inherits the host
/// process environment unchanged. Its own process group and cleanup lease keep
/// timeout or outer-future cancellation from detaching the helper tree. A
/// discarded formatter probe gets no graceful-shutdown window: both paths kill
/// its group immediately, make a bounded attempt to reap the owned leader, then
/// release capture and cleanup ownership even if the kernel cannot reap it yet.
async fn rtk_hook_check(
    executable: &Path,
    working_dir: &Path,
    command: &str,
    cleanup: TaskCleanupGuard,
) -> Option<String> {
    rtk_hook_check_with_timeout(executable, working_dir, command, cleanup, RTK_HOOK_TIMEOUT).await
}

async fn rtk_hook_check_with_timeout(
    executable: &Path,
    working_dir: &Path,
    command: &str,
    cleanup: TaskCleanupGuard,
    timeout: Duration,
) -> Option<String> {
    let mut check = tokio::process::Command::new(executable);
    check
        .arg("hook")
        .arg("check")
        .arg(command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        check.process_group(0);
    }
    let child = check.spawn().ok()?;
    let mut guard = ProcessGuard::arm_host_helper(child, cleanup).ok()?;
    let mut stdout = guard.child_mut().stdout.take()?;
    let mut stderr = guard.child_mut().stderr.take()?;
    let stdout_reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    guard.watch_readers([stdout_reader.abort_handle(), stderr_reader.abort_handle()]);

    let output = tokio::time::timeout(timeout, async {
        let status = guard.child_mut().wait().await?;
        let stdout = stdout_reader.await.map_err(std::io::Error::other)??;
        let stderr = stderr_reader.await.map_err(std::io::Error::other)??;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    })
    .await;
    let (status, stdout, _stderr) = match output {
        Ok(Ok(output)) => output,
        // Timeout, capture failure, or a malformed helper lifecycle all use
        // the original command, after the complete hook group is owned down.
        _ => {
            guard.terminate().await;
            return None;
        }
    };
    guard.release();
    if !status.success() {
        return None;
    }
    let rewritten = String::from_utf8(stdout).ok()?;
    if rewritten.contains('\0') {
        return None;
    }
    let rewritten = rewritten.trim();
    if rewritten.is_empty() || rewritten == command {
        return None;
    }
    Some(rewritten.to_string())
}

impl ToolDefinition for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    /// `bash` runs arbitrary commands; serialize a batch containing it
    /// so two shell calls never trample each other or interleave their
    /// captured output.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let working_dir = ctx.working_directory();
        let cancellation = ctx.cancellation();
        let timeout = Duration::from_secs(input.timeout);
        let command = input.command.clone();
        let session_env = ctx.session_env();
        let tasks = ctx.task_registry();

        // Optionally dispatch through `rtk` to compress output. The
        // model-facing `command` stays the original. Only the string
        // handed to `bash -c` is rewritten, so snapshots, the wire
        // trailer, and `ToolDetails::Bash` keep showing what the model
        // asked for.
        let executed = match self.rtk_rewrite(&command, &working_dir, &tasks).await {
            Some(rewritten) => {
                tracing::debug!(original = %command, rewritten = %rewritten, "rtk passthrough");
                rewritten
            }
            None => command.clone(),
        };

        // Build the child. `process_group(0)` makes the child the
        // leader of a new process group so signaling the group reaches
        // every descendant the shell may have spawned (a `Child::kill`
        // alone only signals the immediate child).
        //
        // `stdin: null` keeps any command that reads from stdin from
        // hanging on the agent's terminal — the child gets EOF
        // immediately rather than waiting for input that will never
        // come.
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&executed)
            .current_dir(&working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // `Command` inherits the process environment by default. Apply the
        // session map afterward so it shadows inherited values, then apply
        // the fixed overrides below so deterministic output still wins.
        cmd.envs(session_env);
        // Overlay a fixed set of environment overrides on top of the
        // inherited and session environment. We capture output rather than
        // attach a terminal, so we force programs into a deterministic,
        // uncolored, non-interactive mode: unstable output (colors, spinners,
        // prompts) is noise the model has to parse, and a prompt with no
        // attached tty would hang until timeout.
        // `GIT_OPTIONAL_LOCKS=0` keeps the agent's read-only git calls
        // from contending with the user's concurrent git over the index
        // lock. Non-git processes ignore it, so we set it unconditionally.
        cmd.env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CLICOLOR_FORCE", "0")
            .env("FORCE_COLOR", "0")
            .env("NONINTERACTIVE", "1")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .env("AGENT", "aj")
            .env("GIT_OPTIONAL_LOCKS", "0");
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let cleanup = ctx.task_registry().track_cleanup();
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Ok(spawn_error_outcome(
                    &command,
                    format!("Failed to start command '{}': {}", command, e),
                ));
            }
        };

        // Armed on the line after the spawn, before anything that can
        // fail: every `?` below this point would otherwise return with
        // the command running and nothing left to reach it.
        let mut guard = ProcessGuard::arm(child, cleanup)?;
        let child_pid = guard.pgid();
        let stdout = guard
            .child_mut()
            .stdout
            .take()
            .expect("stdout was piped above");
        let stderr = guard
            .child_mut()
            .stderr
            .take()
            .expect("stderr was piped above");

        // The spill file is created eagerly so both reader tasks can
        // tee into it without coordinating creation. If no truncation
        // happens we drop the `NamedTempFile` at the end and the file
        // gets unlinked; if truncation does happen we `keep()` it and
        // surface the path through the structured payload.
        let spill = Arc::new(Mutex::new(SpillState::new(self.spill_dir.as_deref())?));

        let stdout_state = Arc::new(Mutex::new(StreamState::new()));
        let stderr_state = Arc::new(Mutex::new(StreamState::new()));
        let capture_error = Arc::new(Mutex::new(None));

        let stdout_reader = tokio::spawn(read_stream(
            stdout,
            Arc::clone(&stdout_state),
            Arc::clone(&spill),
            Arc::clone(&capture_error),
            "stdout",
        ));
        let stderr_reader = tokio::spawn(read_stream(
            stderr,
            Arc::clone(&stderr_state),
            Arc::clone(&spill),
            Arc::clone(&capture_error),
            "stderr",
        ));

        // The readers own the pipe read ends, so the teardown drops them
        // too. They cannot be handed over at arming time because they do
        // not exist until the child's pipes do.
        guard.watch_readers([stdout_reader.abort_handle(), stderr_reader.abort_handle()]);

        if input.run_in_background {
            // Background mode: the spill file is the canonical full
            // output — it must stay reachable for `read_file`, task
            // reads, and the completion notice even when nothing ever
            // overflows the rolling tails — so it is persisted
            // up-front, not just on truncation. Writes keep flowing
            // to the same fd after `persist`.
            let persisted = spill.lock().unwrap().persist();
            let spill_path = match persisted {
                Ok(path) => path,
                Err(err) => {
                    // The child is already running but has no registry
                    // entry yet, so nothing here can ever reach it
                    // again. Returning drops the still-armed guard,
                    // which tears the group down.
                    return Err(err.into());
                }
            };
            let output = Arc::new(BashTaskOutput {
                stdout: Arc::clone(&stdout_state),
                stderr: Arc::clone(&stderr_state),
                spill_path: spill_path.clone(),
            });
            let kind = TaskKind::Bash {
                command: command.clone(),
            };
            let StartedTask {
                id,
                cancel,
                events,
                driver,
            } = ctx.start_background_task(kind, command.clone(), output);
            // Registration and driver spawn stay one synchronous ownership
            // handoff. A drop before the spawn settles the registration and
            // leaves the process guard armed in this future. Once spawned, the
            // driver owns all task lifecycle emissions: `TaskStart` first, then
            // `TaskOutput` / `TaskEnd`. That first emit races this future's own
            // return, so `TaskStart` may land before or after the launch's
            // `ToolExecutionEnd`.
            //
            // The armed process guard moves with the background future. It no
            // longer belongs to this turn, but remains armed so a forced task-
            // driver abort still terminates the process group and readers.
            driver.spawn(drive_background_bash(BackgroundBash {
                process: guard,
                child_pid,
                stdout_reader,
                stderr_reader,
                stdout_state,
                stderr_state,
                capture_error,
                spill_path: spill_path.clone(),
                command: command.clone(),
                task_id: id,
                cancel,
                events,
            }));

            let wire = format!(
                "Started background task #{id}: {command}\n\
                 Output is being written to {path}, read it with read_file \
                 (supports offset/limit). You will be notified when it \
                 completes.",
                path = spill_path.display(),
            );
            return Ok(ToolOutcome {
                content: vec![UserContent::text(wire)],
                details: ToolDetails::Bash {
                    command,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    truncated: false,
                    full_output_path: Some(spill_path),
                    stdout_truncation: None,
                    stderr_truncation: None,
                    task_id: Some(id),
                },
                is_error: false,
            });
        }

        let timeout_at = Instant::now() + timeout;
        // `last_update - UPDATE_DEBOUNCE` triggers a leading-edge fire
        // on the first iteration, so a renderer can show the running
        // command label as soon as we enter the loop.
        let mut last_update = Instant::now() - UPDATE_DEBOUNCE;

        let outcome_kind = loop {
            let now = Instant::now();
            if now.duration_since(last_update) >= UPDATE_DEBOUNCE {
                let snapshot = snapshot_partial(&command, &stdout_state, &stderr_state);
                ctx.emit_update(snapshot).await;
                last_update = now;
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    break ChildExit::Cancelled;
                }
                _ = tokio::time::sleep_until(timeout_at) => {
                    break ChildExit::TimedOut;
                }
                res = guard.child_mut().wait() => {
                    let status = res?;
                    break ChildExit::Exited(status.code());
                }
                // Wake periodically so a long-running command without
                // child-exit / cancel / timeout activity still gets
                // its progress snapshot through the loop above.
                _ = tokio::time::sleep(UPDATE_DEBOUNCE) => {}
            }
        };

        // Cancel/timeout paths: signal the whole process group so any
        // shell-spawned grandchildren die with the parent.
        let capture_released = if matches!(outcome_kind, ChildExit::Cancelled | ChildExit::TimedOut)
        {
            guard.terminate_user_command().await == ProcessTermination::OwnershipReleased
        } else {
            false
        };

        // The reader tasks own the pipe read ends, and their streams end
        // only at EOF, which needs every write end closed. The kill
        // above does not reach a descendant that left the group, so
        // every exit reason drains under the same bound.
        let capture_end = drain_capture(
            stdout_reader,
            stderr_reader,
            &capture_error,
            child_pid,
            capture_released,
        )
        .await;
        let capture_end = if capture_released {
            CaptureEnd::ReapReleased
        } else {
            capture_end
        };
        let capture_error = capture_error.lock().unwrap().clone();

        // Finalize per-stream: apply truncate_tail to the rolling tail
        // (after dropping any leading partial line) and produce the
        // model-facing stdout/stderr strings plus optional structured
        // truncation summaries.
        let (stdout_str, stdout_truncation) = {
            let s = stdout_state.lock().unwrap();
            finalize_stream(&s)
        };
        let (stderr_str, stderr_truncation) = {
            let s = stderr_state.lock().unwrap();
            finalize_stream(&s)
        };

        let truncated = stdout_truncation.is_some() || stderr_truncation.is_some();

        // Persist the spill file iff we actually truncated; otherwise
        // drop it (NamedTempFile's Drop unlinks the file).
        let full_output_path = {
            let mut spill = spill.lock().unwrap();
            if truncated {
                Some(spill.persist()?)
            } else {
                None
            }
        };

        let exit_code = match outcome_kind {
            ChildExit::Exited(code) => code,
            ChildExit::Cancelled | ChildExit::TimedOut => None,
        };

        // The command has ended and its capture is closed, so there is
        // nothing left for a drop to tear down.
        guard.release();

        let mut wire = build_wire_content(
            &stdout_str,
            &stderr_str,
            stdout_truncation.as_ref(),
            stderr_truncation.as_ref(),
            &outcome_kind,
            exit_code,
            input.timeout,
            full_output_path.as_deref(),
            capture_end,
        );
        if let Some(error) = &capture_error {
            wire.push_str(&format!("\nOutput capture failed: {error}"));
        }

        // Cancellation and timeout are exceptional outcomes the model
        // should know to recover from; a non-zero exit code from a
        // command that ran to completion is a normal "the command
        // failed" signal that the wire content already conveys.
        let is_error = capture_error.is_some()
            || matches!(outcome_kind, ChildExit::Cancelled | ChildExit::TimedOut);

        Ok(ToolOutcome {
            content: vec![UserContent::text(wire)],
            details: ToolDetails::Bash {
                command,
                stdout: stdout_str,
                stderr: stderr_str,
                exit_code,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                task_id: None,
            },
            is_error,
        })
    }
}

/// Why the child stopped. Drives both the wire content's trailer and
/// the `is_error` flag.
#[derive(Clone, Copy, Debug)]
enum ChildExit {
    /// Child ran to completion. `Some(code)` for normal exit, `None`
    /// when the child was killed by a signal.
    Exited(Option<i32>),
    /// `ToolContext::cancellation` fired during execution.
    Cancelled,
    /// The configured timeout elapsed before the child returned.
    TimedOut,
}

/// Per-stream rolling-tail state shared with the reader task.
///
/// Tracks both the in-memory rolling tail and the source-stream
/// totals (line and byte counts) needed to build the truncation
/// markers. The rolling tail is allowed to grow up to
/// [`TRIM_TRIGGER_BYTES`] between trims and is shrunk back to
/// [`ROLLING_CAP_BYTES`] whenever it crosses that threshold.
struct StreamState {
    /// Rolling buffer of recent source bytes.
    tail: Vec<u8>,
    /// True iff `tail[0]` sits at a line boundary in the source — i.e.
    /// the byte preceding it in the original stream was `\n`, or
    /// `tail[0]` is the first byte of the source. Used at snapshot
    /// time to decide whether to drop a leading partial line before
    /// running [`truncate_tail`].
    tail_starts_at_boundary: bool,
    /// Total bytes that flowed through this stream (including any
    /// that have been trimmed out of `tail`).
    total_bytes_seen: u64,
    /// Number of `\n` bytes seen in the source so far.
    newlines_seen: u64,
    /// Bytes since the most recent `\n` (or since stream start). Equals
    /// the size of the source's trailing partial line at end-of-stream.
    current_line_bytes: u64,
    /// True iff the most recent source byte was `\n`. Initialised to
    /// `true` so an empty stream is treated as ending on a (vacuous)
    /// boundary.
    ends_with_newline: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            tail: Vec::new(),
            tail_starts_at_boundary: true,
            total_bytes_seen: 0,
            newlines_seen: 0,
            current_line_bytes: 0,
            ends_with_newline: true,
        }
    }

    /// Source line count. The empty stream has zero lines; a stream
    /// ending in `\n` does not get a phantom trailing empty line;
    /// otherwise we add one for the unterminated trailing line.
    fn total_lines(&self) -> u64 {
        if self.total_bytes_seen == 0 {
            return 0;
        }
        self.newlines_seen + u64::from(!self.ends_with_newline)
    }

    /// Apply a chunk: update the rolling tail and the source-totals
    /// bookkeeping. The chunk is appended verbatim; we trim back to
    /// [`ROLLING_CAP_BYTES`] once the tail crosses
    /// [`TRIM_TRIGGER_BYTES`].
    #[allow(clippy::as_conversions)]
    fn append_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes_seen += chunk.len() as u64;
        for &b in chunk {
            if b == b'\n' {
                self.newlines_seen += 1;
                self.current_line_bytes = 0;
                self.ends_with_newline = true;
            } else {
                self.current_line_bytes += 1;
                self.ends_with_newline = false;
            }
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TRIM_TRIGGER_BYTES {
            self.trim_to(ROLLING_CAP_BYTES);
        }
    }

    fn trim_to(&mut self, target: usize) {
        if self.tail.len() <= target {
            return;
        }
        let drop_n = self.tail.len() - target;
        // `drop_n > 0` here, so `tail[drop_n - 1]` is the last byte
        // we're about to evict. Whether it's `\n` decides whether the
        // new tail starts on a fresh line.
        let preceding = self.tail[drop_n - 1];
        self.tail.drain(..drop_n);
        self.tail_starts_at_boundary = preceding == b'\n';
    }
}

/// Spill-file state: a temp file we tee both streams into.
///
/// Foreground runs persist it only when truncation occurred
/// (otherwise dropping `Self` unlinks it); background runs persist it
/// up-front — the spill is the canonical full output — and keep
/// writing to the same fd afterwards.
struct SpillState {
    /// `None` only transiently inside `persist` (and after a failed
    /// `keep`, in which case further writes are dropped — the caller
    /// already surfaced the error).
    file: Option<SpillFile>,
}

enum SpillFile {
    /// Unlinked on drop unless persisted.
    Temp(NamedTempFile),
    /// Persisted at `path`; writes keep flowing to `file`.
    Kept { file: std::fs::File, path: PathBuf },
}

impl SpillState {
    /// A fresh spill file in `dir`, or in the ambient temp directory when
    /// `dir` is `None`.
    ///
    /// The directory is created if missing, so a configured path does not have
    /// to exist before the first command runs.
    fn new(dir: Option<&Path>) -> std::io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("aj-bash-").suffix(".log");
        let file = match dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                builder.tempfile_in(dir)?
            }
            None => builder.tempfile()?,
        };
        Ok(Self {
            file: Some(SpillFile::Temp(file)),
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        match self.file.as_mut() {
            Some(SpillFile::Temp(f)) => f.as_file_mut().write_all(bytes),
            Some(SpillFile::Kept { file, .. }) => file.write_all(bytes),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "spill file is unavailable",
            )),
        }
    }

    /// Persist the spill file at its current path, returning that path
    /// for the caller to surface. Idempotent; the open handle is kept
    /// so reader tasks can continue teeing into the persisted file.
    fn persist(&mut self) -> std::io::Result<PathBuf> {
        match self.file.take() {
            Some(SpillFile::Temp(tmp)) => {
                let (file, path) = tmp.keep().map_err(|e| e.error)?;
                self.file = Some(SpillFile::Kept {
                    file,
                    path: path.clone(),
                });
                Ok(path)
            }
            Some(SpillFile::Kept { file, path }) => {
                let out = path.clone();
                self.file = Some(SpillFile::Kept { file, path });
                Ok(out)
            }
            None => unreachable!("spill file present unless a prior persist failed"),
        }
    }
}

/// Drain `reader` into the shared stream state, teeing every byte into
/// the spill file as it arrives. Terminates when the pipe closes.
async fn read_stream<R>(
    mut reader: R,
    state: Arc<Mutex<StreamState>>,
    spill: Arc<Mutex<SpillState>>,
    capture_error: Arc<Mutex<Option<String>>>,
    stream_name: &'static str,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            // EOF — the child has closed this pipe.
            Ok(0) => return,
            Ok(n) => {
                let chunk = &buf[..n];
                // The spill file always sees every byte; the rolling
                // tail is what gets surfaced to the model and the UI.
                {
                    let mut spill = spill.lock().unwrap();
                    if let Err(error) = spill.write_all(chunk) {
                        record_capture_error(
                            &capture_error,
                            format!("{stream_name} spill write: {error}"),
                        );
                        return;
                    }
                }
                {
                    let mut s = state.lock().unwrap();
                    s.append_chunk(chunk);
                }
            }
            Err(error) => {
                record_capture_error(&capture_error, format!("{stream_name} pipe read: {error}"));
                return;
            }
        }
    }
}

fn record_capture_error(error_slot: &Mutex<Option<String>>, error: String) {
    let mut slot = error_slot.lock().unwrap();
    if slot.is_none() {
        *slot = Some(error);
    }
}

async fn await_reader(
    reader: &mut Option<tokio::task::JoinHandle<()>>,
    capture_error: &Mutex<Option<String>>,
    cancellation_expected: bool,
) {
    if let Some(handle) = reader.as_mut() {
        if let Err(error) = handle.await {
            if !(cancellation_expected && error.is_cancelled()) {
                record_capture_error(capture_error, format!("capture reader task: {error}"));
            }
        }
        // Clearing the slot is what keeps a later drain round from
        // polling a handle that already returned, which panics.
        *reader = None;
    }
}

/// How a command's output capture ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureEnd {
    /// Both pipes closed on their own: the capture is complete.
    Closed,
    /// A straggler still held a pipe when the drain expired, so the
    /// pipes were taken back and the output may be incomplete.
    Cut,
    /// A killed command remained unavailable to reap, so its process and
    /// capture ownership were released at the reap boundary.
    ReapReleased,
}

/// Escalation step for signalling a command's process group.
#[derive(Clone, Copy, Debug)]
enum GroupSignal {
    Term,
    Kill,
}

/// Wait for the capture pipes to close now the command has ended, and
/// take them back if something still holds them.
///
/// Returns [`CaptureEnd::Cut`] when the wait had to be cut short, which
/// means the output is possibly incomplete and the command's process
/// group has been killed. Callers report that; `pgid` is the child's
/// pid, which is its group id because it was spawned with
/// `process_group(0)`.
///
/// Every exit reason comes through here, including the paths that
/// already killed the group: [`read_stream`] returns only on EOF, EOF
/// needs every write end closed, and a descendant that left the group
/// holds one just as well as one that stayed.
///
/// The escalation keys on EOF rather than on the child, because the caller has
/// either reaped it or exhausted the bounded reap and released its handle.
async fn drain_capture(
    stdout_reader: tokio::task::JoinHandle<()>,
    stderr_reader: tokio::task::JoinHandle<()>,
    capture_error: &Mutex<Option<String>>,
    pgid: i32,
    reader_cancellation_expected: bool,
) -> CaptureEnd {
    let mut stdout_reader = Some(stdout_reader);
    let mut stderr_reader = Some(stderr_reader);

    if drain_round(
        &mut stdout_reader,
        &mut stderr_reader,
        capture_error,
        CAPTURE_DRAIN_GRACE,
        reader_cancellation_expected,
    )
    .await
    {
        return CaptureEnd::Closed;
    }

    // Something the command started outlived it holding a write end.
    // Signalling a reaped leader's group is a theoretical stray kill if
    // the pid were recycled, but the drain only expires while a member
    // of that group is alive and holding our pipe.
    signal_process_group(pgid, GroupSignal::Term);
    if !drain_round(
        &mut stdout_reader,
        &mut stderr_reader,
        capture_error,
        CAPTURE_DRAIN_GRACE,
        reader_cancellation_expected,
    )
    .await
    {
        signal_process_group(pgid, GroupSignal::Kill);
        drain_round(
            &mut stdout_reader,
            &mut stderr_reader,
            capture_error,
            CAPTURE_CLOSE_GRACE,
            reader_cancellation_expected,
        )
        .await;
    }

    // A holder outside the group survives both signals, so the last
    // word is dropping the read ends: the reader tasks own them, the
    // turn returns, the host keeps no descriptors, and the holder's
    // next write fails.
    for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
        reader.abort();
    }
    CaptureEnd::Cut
}

/// One drain window: both readers, bounded by `budget`, concurrently so
/// a stalled stream cannot spend the other's budget. True iff both are
/// finished.
async fn drain_round(
    stdout_reader: &mut Option<tokio::task::JoinHandle<()>>,
    stderr_reader: &mut Option<tokio::task::JoinHandle<()>>,
    capture_error: &Mutex<Option<String>>,
    budget: Duration,
    reader_cancellation_expected: bool,
) -> bool {
    let _ = tokio::time::timeout(budget, async {
        tokio::join!(
            await_reader(stdout_reader, capture_error, reader_cancellation_expected),
            await_reader(stderr_reader, capture_error, reader_cancellation_expected),
        );
    })
    .await;
    stdout_reader.is_none() && stderr_reader.is_none()
}

/// Trailer for a run whose capture was cut short, in
/// [`CaptureEnd::Cut`]'s terms: what happened, what it costs the
/// reader, and the supported way to avoid it.
fn capture_cut_trailer() -> String {
    format!(
        "Output capture was cut short: a process this command started still held \
         stdout/stderr {}s after the command ended, so its process group was killed \
         and the pipes were closed. The output above may be incomplete. Use \
         run_in_background for work that should outlive the call.",
        CAPTURE_DRAIN_GRACE.as_secs(),
    )
}

/// Explain capture released with a command that remained unavailable to reap.
fn reap_release_trailer() -> String {
    format!(
        "Output capture was cut short: the command leader was still unavailable to reap {}s \
         after its process group was killed, so its process and pipes were released. The output \
         above may be incomplete.",
        KILL_GRACE.as_secs(),
    )
}

fn capture_end_trailer(capture_end: CaptureEnd) -> Option<String> {
    match capture_end {
        CaptureEnd::Closed => None,
        CaptureEnd::Cut => Some(capture_cut_trailer()),
        CaptureEnd::ReapReleased => Some(reap_release_trailer()),
    }
}

/// [`TaskOutputSource`] over a background bash task's shared stream
/// states. Snapshots are stateless reads of the rolling tails; the
/// always-persisted spill file is the canonical full output.
struct BashTaskOutput {
    stdout: Arc<Mutex<StreamState>>,
    stderr: Arc<Mutex<StreamState>>,
    spill_path: PathBuf,
}

impl TaskOutputSource for BashTaskOutput {
    fn snapshot(&self) -> TaskRead {
        let (stdout_tail, stdout_total_bytes) = tail_snapshot(&self.stdout);
        let (stderr_tail, stderr_total_bytes) = tail_snapshot(&self.stderr);
        TaskRead {
            stdout_tail,
            stderr_tail,
            stdout_total_bytes,
            stderr_total_bytes,
            spill_path: Some(self.spill_path.clone()),
            report: None,
        }
    }
}

/// Decode a stream's rolling tail plus its exact byte total. Mirrors
/// `finalize_stream`'s whole-line policy: a leading partial line left
/// by a mid-line trim is dropped, except when the tail has no newline
/// at all (a single huge line stays visible).
fn tail_snapshot(state: &Arc<Mutex<StreamState>>) -> (String, u64) {
    let (bytes, at_boundary, total) = {
        let s = state.lock().unwrap();
        (
            s.tail.clone(),
            s.tail_starts_at_boundary,
            s.total_bytes_seen,
        )
    };
    let decoded = decode_stream_output(bytes);
    let text = if at_boundary {
        decoded
    } else {
        match decoded.find('\n') {
            None => decoded,
            Some(idx) => decoded[idx + 1..].to_string(),
        }
    };
    (text, total)
}

/// Everything a detached background-bash driver owns.
struct BackgroundBash {
    process: ProcessGuard,
    child_pid: i32,
    stdout_reader: tokio::task::JoinHandle<()>,
    stderr_reader: tokio::task::JoinHandle<()>,
    stdout_state: Arc<Mutex<StreamState>>,
    stderr_state: Arc<Mutex<StreamState>>,
    capture_error: Arc<Mutex<Option<String>>>,
    spill_path: PathBuf,
    command: String,
    task_id: TaskId,
    cancel: CancellationToken,
    events: TaskEventSink,
}

/// Drive a background bash task to completion: announce it with
/// `TaskStart`, emit throttled `TaskOutput` snapshots, kill the
/// process group on cancellation, and finish with the registry status
/// flip + completion notice.
async fn drive_background_bash(task: BackgroundBash) {
    let BackgroundBash {
        mut process,
        child_pid,
        stdout_reader,
        stderr_reader,
        stdout_state,
        stderr_state,
        capture_error,
        spill_path,
        command,
        task_id,
        cancel,
        events,
    } = task;

    events
        .started(TaskKind::Bash {
            command: command.clone(),
        })
        .await;

    let mut last_update = Instant::now() - UPDATE_DEBOUNCE;
    // `None` forces the leading-edge emit so the TUI cell shows the
    // running command immediately.
    let mut last_totals: Option<(u64, u64)> = None;
    let (process_status, capture_released) = loop {
        let now = Instant::now();
        if now.duration_since(last_update) >= UPDATE_DEBOUNCE {
            let totals = (
                stdout_state.lock().unwrap().total_bytes_seen,
                stderr_state.lock().unwrap().total_bytes_seen,
            );
            // Skip the emit while the streams are quiet: an idle
            // watcher task would otherwise push identical snapshots
            // (each cloning both rolling tails) onto the bus at the
            // throttle rate for as long as it runs.
            if last_totals != Some(totals) {
                let mut partial = snapshot_partial(&command, &stdout_state, &stderr_state);
                if let ToolDetails::Bash {
                    task_id: tid,
                    full_output_path,
                    ..
                } = &mut partial
                {
                    *tid = Some(task_id);
                    *full_output_path = Some(spill_path.clone());
                }
                events.output(partial).await;
                last_totals = Some(totals);
            }
            last_update = now;
        }

        tokio::select! {
            biased;
            // The task token (a child of the registry's session root)
            // is the only cancellation that reaches a background
            // task: task_stop, the picker's kill action, and shutdown
            // all fire it. The originating turn's token is
            // deliberately not wired in — outliving the turn is the
            // point.
            _ = cancel.cancelled() => {
                let capture_released = process.terminate_user_command().await
                    == ProcessTermination::OwnershipReleased;
                break (TaskStatus::Killed, capture_released);
            }
            res = process.child_mut().wait() => {
                break (TaskStatus::Exited(res.ok().and_then(|s| s.code())), false);
            }
            _ = tokio::time::sleep(UPDATE_DEBOUNCE) => {}
        }
    };

    // The task is over, so the same bound applies one level down: a
    // straggler holding the pipes would otherwise keep the notice from
    // ever rendering and the registry row from ever settling.
    let capture_end = drain_capture(
        stdout_reader,
        stderr_reader,
        &capture_error,
        child_pid,
        capture_released,
    )
    .await;
    let capture_end = if capture_released {
        CaptureEnd::ReapReleased
    } else {
        capture_end
    };
    let capture_error = capture_error.lock().unwrap().clone();
    let status = background_terminal_status(process_status, capture_error.is_some());

    let (stdout_str, stdout_truncation) = {
        let s = stdout_state.lock().unwrap();
        finalize_stream(&s)
    };
    let (stderr_str, stderr_truncation) = {
        let s = stderr_state.lock().unwrap();
        finalize_stream(&s)
    };

    let mut body = format!(
        "Background task #{task_id} finished: {command} — {}",
        task_status_text(status)
    );
    let tail = render_stream_block(
        &stdout_str,
        &stderr_str,
        stdout_truncation.as_ref(),
        stderr_truncation.as_ref(),
        Some(&spill_path),
    );
    if !tail.is_empty() {
        body.push('\n');
        body.push_str(&tail);
    }
    if let Some(error) = capture_error {
        body.push_str(&format!("\nOutput capture failed: {error}"));
    }
    if let Some(trailer) = capture_end_trailer(capture_end) {
        body.push('\n');
        body.push_str(&trailer);
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!("Full output: {}", spill_path.display()));

    let notice = TaskNotice {
        owner: events.owner(),
        task_id,
        kind: TaskKind::Bash {
            command: command.clone(),
        },
        label: command,
        status,
        body,
    };
    events.finished(status, notice).await;
    // Every cancellation or panic above keeps the guard armed. Only a fully
    // reported driver relinquishes the reaped child or completed bounded
    // ownership release.
    process.release();
}

/// Human-readable terminal-status phrase shared by completion notices
/// and `task_output` / `task_stop` reports.
pub(crate) fn task_status_text(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "still running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exit code {code}"),
        TaskStatus::Exited(None) => "terminated by signal".to_string(),
        TaskStatus::CaptureFailed(Some(code)) => {
            format!("output capture failed after exit code {code}")
        }
        TaskStatus::CaptureFailed(None) => {
            "output capture failed after termination by signal".to_string()
        }
        TaskStatus::Killed => "killed".to_string(),
    }
}

fn background_terminal_status(status: TaskStatus, capture_failed: bool) -> TaskStatus {
    match (status, capture_failed) {
        (TaskStatus::Exited(code), true) => TaskStatus::CaptureFailed(code),
        (status, _) => status,
    }
}

/// Resolve a stream's rolling tail into a (possibly-truncated)
/// display string plus an optional structured truncation summary.
/// When the source overflowed either cap we drop any leading partial
/// line from the rolling tail and then apply [`truncate_tail`] to fit
/// the per-stream byte/line cap exactly.
///
/// Reads the state without consuming it: background tasks finalize
/// for the completion notice while `task_output` snapshots must keep
/// seeing the tail afterwards.
#[allow(clippy::as_conversions)]
fn finalize_stream(state: &StreamState) -> (String, Option<BashStreamTruncation>) {
    let total_lines = state.total_lines();
    let total_bytes = state.total_bytes_seen;

    let tail_decoded = decode_stream_output(state.tail.clone());

    let overflowed = total_lines > BASH_MAX_LINES as u64 || total_bytes > BASH_MAX_BYTES as u64;
    if !overflowed {
        return (tail_decoded, None);
    }

    // Drop a leading partial line so `truncate_tail` always operates
    // on whole-line boundaries when the rolling buffer happened to be
    // trimmed in the middle of a line. The exception is when the
    // tail contains no newlines at all: in that case the whole tail
    // belongs to a single source line that's bigger than the byte
    // budget, so we keep it and let `truncate_tail` flag the result
    // as `last_line_partial`.
    let snapshot_text: String = if state.tail_starts_at_boundary {
        tail_decoded
    } else {
        match tail_decoded.find('\n') {
            None => tail_decoded,
            Some(idx) => tail_decoded[idx + 1..].to_string(),
        }
    };

    let tt = truncate_tail(&snapshot_text, BASH_MAX_LINES, BASH_MAX_BYTES);

    // `truncate_tail` flags its own cap-fire; when the snapshot
    // already fit (we trimmed it small upstream) fall back to whichever
    // global budget the source overflowed.
    let truncated_by = tt.truncated_by.unwrap_or({
        if total_bytes > BASH_MAX_BYTES as u64 {
            TruncatedBy::Bytes
        } else {
            TruncatedBy::Lines
        }
    });

    let summary = BashStreamTruncation {
        total_lines,
        total_bytes,
        output_lines: tt.output_lines as u64,
        output_bytes: tt.output_bytes as u64,
        truncated_by,
        last_line_partial: tt.last_line_partial,
        last_line_bytes: state.current_line_bytes,
    };

    (tt.content, Some(summary))
}

/// Build a [`ToolDetails::Bash`] partial from the in-flight state. Used
/// for `emit_update` snapshots while the child is running. The
/// structured per-stream summaries are intentionally left `None` —
/// they only become meaningful once the stream has closed and we can
/// run `truncate_tail` on the final rolling tail. The boolean
/// `truncated` flag is updated live so the UI can show a "truncated"
/// badge as soon as the source crosses the cap.
#[allow(clippy::as_conversions)]
fn snapshot_partial(
    command: &str,
    stdout_state: &Arc<Mutex<StreamState>>,
    stderr_state: &Arc<Mutex<StreamState>>,
) -> ToolDetails {
    let stdout_state = stdout_state.lock().unwrap();
    let stderr_state = stderr_state.lock().unwrap();
    let stdout_data = stdout_state.tail.clone();
    let stderr_data = stderr_state.tail.clone();
    let truncated = stdout_state.total_lines() > BASH_MAX_LINES as u64
        || stdout_state.total_bytes_seen > BASH_MAX_BYTES as u64
        || stderr_state.total_lines() > BASH_MAX_LINES as u64
        || stderr_state.total_bytes_seen > BASH_MAX_BYTES as u64;
    ToolDetails::Bash {
        command: command.to_string(),
        stdout: decode_stream_output(stdout_data),
        stderr: decode_stream_output(stderr_data),
        exit_code: None,
        truncated,
        full_output_path: None,
        stdout_truncation: None,
        stderr_truncation: None,
        task_id: None,
    }
}

/// Build the wire content the model sees. Per-stream truncation
/// markers (`[Showing lines X-Y of TOTAL ...]`) are inserted right
/// after each affected stream's content so the model reads the
/// elision context next to the truncated text. The trailing
/// exit-status / cancel / timeout block stays last.
#[allow(clippy::too_many_arguments)]
fn build_wire_content(
    stdout: &str,
    stderr: &str,
    stdout_truncation: Option<&BashStreamTruncation>,
    stderr_truncation: Option<&BashStreamTruncation>,
    outcome: &ChildExit,
    exit_code: Option<i32>,
    timeout_secs: u64,
    full_output_path: Option<&std::path::Path>,
    capture_end: CaptureEnd,
) -> String {
    let mut wire = render_stream_block(
        stdout,
        stderr,
        stdout_truncation,
        stderr_truncation,
        full_output_path,
    );
    match outcome {
        ChildExit::Exited(_) => {
            if let Some(code) = exit_code {
                if code != 0 {
                    if !wire.is_empty() && !wire.ends_with('\n') {
                        wire.push('\n');
                    }
                    wire.push_str(&format!("Command failed with exit code: {}", code));
                }
            } else {
                // Killed by signal: report something sensible so the
                // model can reason about the failure.
                if !wire.is_empty() && !wire.ends_with('\n') {
                    wire.push('\n');
                }
                wire.push_str("Command terminated by signal");
            }
        }
        ChildExit::Cancelled => {
            if !wire.is_empty() && !wire.ends_with('\n') {
                wire.push('\n');
            }
            wire.push_str("Command cancelled");
        }
        ChildExit::TimedOut => {
            if !wire.is_empty() && !wire.ends_with('\n') {
                wire.push('\n');
            }
            wire.push_str(&format!("Command timed out after {} seconds", timeout_secs));
        }
    }
    if let Some(trailer) = capture_end_trailer(capture_end) {
        if !wire.is_empty() && !wire.ends_with('\n') {
            wire.push('\n');
        }
        wire.push_str(&trailer);
    }
    wire
}

/// Render the two streams plus their truncation markers — the shared
/// body of foreground wire content, background completion notices,
/// and `task_output` reports.
pub(crate) fn render_stream_block(
    stdout: &str,
    stderr: &str,
    stdout_truncation: Option<&BashStreamTruncation>,
    stderr_truncation: Option<&BashStreamTruncation>,
    full_output_path: Option<&std::path::Path>,
) -> String {
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(stdout);
    }
    if let Some(t) = stdout_truncation {
        push_marker(&mut out, &stream_marker("stdout", t, full_output_path));
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("STDERR:\n");
        out.push_str(stderr);
    }
    if let Some(t) = stderr_truncation {
        push_marker(&mut out, &stream_marker("stderr", t, full_output_path));
    }
    out
}

/// Append `marker` to `wire` on its own line, inserting a separating
/// newline only when one isn't already there.
fn push_marker(wire: &mut String, marker: &str) {
    if !wire.is_empty() && !wire.ends_with('\n') {
        wire.push('\n');
    }
    wire.push_str(marker);
}

/// Render a single stream's truncation marker.
///
/// - `last_line_partial`: `[Showing last <bytes> of <stream> line N (line is <size>). Full output at <path>]`
/// - line cap fired: `[Showing lines X-Y of TOTAL of <stream>. Full output at <path>]`
/// - byte cap fired: `[Showing lines X-Y of TOTAL of <stream> (50.0KB limit). Full output at <path>]`
///
/// `full_output_path` is shared across both streams (we tee both into
/// one spill file); a missing path falls back to a path-less form so
/// the marker still tells the model what was dropped.
#[allow(clippy::as_conversions)]
pub fn stream_marker(
    stream: &str,
    t: &BashStreamTruncation,
    full_output_path: Option<&std::path::Path>,
) -> String {
    let suffix = match full_output_path {
        Some(p) => format!(". Full output at {}", p.display()),
        None => String::new(),
    };
    if t.last_line_partial {
        return format!(
            "[Showing last {} of {} line {} (line is {}){}]",
            format_size(t.output_bytes as usize),
            stream,
            t.total_lines,
            format_size(t.last_line_bytes as usize),
            suffix,
        );
    }
    let start = t.total_lines.saturating_sub(t.output_lines) + 1;
    let end = t.total_lines;
    match t.truncated_by {
        TruncatedBy::Lines => format!(
            "[Showing lines {}-{} of {} of {}{}]",
            start, end, t.total_lines, stream, suffix,
        ),
        TruncatedBy::Bytes => format!(
            "[Showing lines {}-{} of {} of {} ({} limit){}]",
            start,
            end,
            t.total_lines,
            stream,
            format_size(BASH_MAX_BYTES),
            suffix,
        ),
    }
}

/// Build a recoverable-error outcome for the spawn-failure path.
/// Surface the failure both as the wire content (so the model can see
/// what went wrong) and as `stderr` in the structured payload (no
/// command actually ran, so there's no real stdout/stderr split).
fn spawn_error_outcome(command: &str, error: String) -> ToolOutcome {
    ToolOutcome {
        content: vec![UserContent::text(error.clone())],
        details: ToolDetails::Bash {
            command: command.to_string(),
            stdout: String::new(),
            stderr: error,
            exit_code: None,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id: None,
        },
        is_error: true,
    }
}

/// Decode subprocess output bytes to UTF-8 (lossy) and sanitise them
/// before they leave the bash tool.
///
/// Sanitisation strips ANSI escape sequences, drops carriage returns,
/// and removes other terminal-control bytes that would either corrupt
/// the renderer's width math (so the tool-output bubble's right edge
/// stays flush instead of breaking on overprints / erase-in-line) or
/// waste tokens in the model's context. See [`crate::sanitize`] for
/// the exact transform.
fn decode_stream_output(bytes: Vec<u8>) -> String {
    let lossy = String::from_utf8_lossy(&bytes);
    crate::sanitize_terminal_output(&lossy)
}

/// Owns a spawned process group's teardown for as long as this tool's future
/// can be dropped.
///
/// The driver races a tool against cancellation and drops the losing
/// future rather than polling it again, so a tool cannot clean up by
/// observing its own token: a drop is the one moment nothing polls it.
/// Dropping a [`Child`] does not signal the process and dropping a
/// `JoinHandle` only detaches its task, so without this a cancelled
/// turn leaves the whole process group running and both pipe read ends
/// held.
///
/// Armed at the spawn, before the first fallible step, because a
/// command that outlives an early `?` leaks exactly as a cancelled one
/// does. A normal foreground return disarms after the command has ended. A
/// background handoff moves the armed guard into its tracked driver, which
/// disarms only after final reporting. Every abnormal way out (a `?`, a panic,
/// or either future being dropped) leaves it armed and tears the group down.
///
/// Only the `SIGTERM` is guaranteed. The escalation to `SIGKILL` and
/// the reap run on the runtime, so a host exiting under an in-flight
/// command loses them and a command that ignores the signal outlives
/// it. What is lost has an heir: init reaps the orphan, and the pipe
/// read ends close with the host process.
struct ProcessGuard {
    /// `None` once disarmed.
    armed: Option<ArmedProcess>,
}

struct ArmedProcess {
    child: Child,
    pgid: i32,
    /// Retains the session's advisory-lock lifetime through asynchronous
    /// teardown when the owning tool or task future is dropped.
    _cleanup: TaskCleanupGuard,
    teardown: ProcessTeardown,
    /// Empty between the spawn and [`ProcessGuard::watch_readers`]: the
    /// reader tasks do not exist for the first few lines of a call.
    readers: Vec<tokio::task::AbortHandle>,
}

#[derive(Clone, Copy)]
enum ProcessTeardown {
    /// User commands get a chance to handle TERM before escalation.
    Graceful,
    /// Optional host probes are discarded with an immediate group kill.
    Immediate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessTermination {
    Reaped,
    OwnershipReleased,
}

impl ProcessGuard {
    /// Take ownership of a freshly spawned user-command child.
    ///
    /// Fails only if the pid is unavailable, which cannot happen
    /// between a spawn and the reap, and which would leave nothing to
    /// signal anyway.
    fn arm(child: Child, cleanup: TaskCleanupGuard) -> Result<Self, aj_agent::BoxError> {
        Self::arm_with(child, cleanup, ProcessTeardown::Graceful)
    }

    /// Own a bounded host helper under the session's cleanup lease.
    fn arm_host_helper(
        child: Child,
        cleanup: TaskCleanupGuard,
    ) -> Result<Self, aj_agent::BoxError> {
        Self::arm_with(child, cleanup, ProcessTeardown::Immediate)
    }

    fn arm_with(
        child: Child,
        cleanup: TaskCleanupGuard,
        teardown: ProcessTeardown,
    ) -> Result<Self, aj_agent::BoxError> {
        // The pid is the group id, because the child was spawned with
        // `process_group(0)`.
        let pgid: i32 = child
            .id()
            .ok_or("child PID unavailable after spawn")?
            .try_into()
            .map_err(|e| format!("child PID does not fit in i32: {e}"))?;
        Ok(Self {
            armed: Some(ArmedProcess {
                child,
                pgid,
                _cleanup: cleanup,
                teardown,
                readers: Vec::new(),
            }),
        })
    }

    /// The command's process-group id.
    fn pgid(&self) -> i32 {
        self.expect_armed().pgid
    }

    /// The child, for the caller that is still driving it.
    fn child_mut(&mut self) -> &mut Child {
        &mut self
            .armed
            .as_mut()
            .expect("the guard stays armed until the command's lifetime moves")
            .child
    }

    /// Hand the capture readers to the teardown, which drops the pipe
    /// read ends after it has signalled.
    fn watch_readers(&mut self, readers: [tokio::task::AbortHandle; 2]) {
        self.armed
            .as_mut()
            .expect("the guard stays armed until the command's lifetime moves")
            .readers
            .extend(readers);
    }

    /// Release process, capture, and cleanup ownership after every async step
    /// that needs it has completed.
    fn release(&mut self) {
        let Some(armed) = self.armed.take() else {
            return;
        };
        for reader in &armed.readers {
            reader.abort();
        }
    }

    /// Give a user command its TERM grace, escalate to KILL, and retain
    /// ownership until either the leader is reaped or the reap bound expires.
    ///
    /// On expiry the capture handles, child, and cleanup lease are released in
    /// this method's synchronous tail. Cancellation can therefore only occur
    /// while the still-armed guard owns all three.
    async fn terminate_user_command(&mut self) -> ProcessTermination {
        let reaped = {
            let armed = self
                .armed
                .as_mut()
                .expect("the guard stays armed through command termination");
            debug_assert!(matches!(armed.teardown, ProcessTeardown::Graceful));
            terminate_process_group(&mut armed.child, armed.pgid).await
        };
        if reaped {
            ProcessTermination::Reaped
        } else {
            self.release();
            ProcessTermination::OwnershipReleased
        }
    }

    /// Kill an optional helper's group and make a bounded attempt to reap its
    /// immediate child. Ownership stays in `self` across the reap, so
    /// cancellation hands the still-armed group to [`Drop`] rather than
    /// detaching it. A child stuck in uninterruptible kernel I/O cannot retain
    /// the session cleanup lease beyond the reap bound.
    async fn terminate(&mut self) {
        let Some(armed) = self.armed.as_mut() else {
            return;
        };
        debug_assert!(matches!(armed.teardown, ProcessTeardown::Immediate));
        signal_process_group(armed.pgid, GroupSignal::Kill);
        reap_child_bounded(&mut armed.child).await;
        // Every await is complete. Taking the process now releases the cleanup
        // lease without leaving a cancellation point between ownership moves.
        self.release();
    }

    fn expect_armed(&self) -> &ArmedProcess {
        self.armed
            .as_ref()
            .expect("the guard stays armed until the command's lifetime moves")
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let Some(armed) = self.armed.take() else {
            return;
        };
        // The first signal leaves from `Drop` itself rather than from
        // the teardown, because a spawn is not a promise: a runtime
        // that is shutting down answers one by dropping the future
        // unpolled, and then nothing would ever be sent.
        let teardown = armed.teardown;
        signal_process_group(
            armed.pgid,
            match teardown {
                ProcessTeardown::Graceful => GroupSignal::Term,
                ProcessTeardown::Immediate => GroupSignal::Kill,
            },
        );
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    match teardown {
                        ProcessTeardown::Graceful => tear_down_process(armed).await,
                        ProcessTeardown::Immediate => reap_killed_process(armed).await,
                    }
                });
            }
            Err(_) => {
                // Dropped outside a runtime, so there is nothing to
                // escalate or reap on. The descriptors are still ours
                // to release.
                for reader in &armed.readers {
                    reader.abort();
                }
            }
        }
    }
}

/// Finish what [`ProcessGuard`]'s caller started: give the group its
/// grace, escalate to `SIGKILL`, reap what is still ours, and drop the
/// pipe read ends.
///
/// The grace is a timer on every path, never [`Child::wait`]. Once the
/// leader has been reaped, `wait` answers instantly from a cached
/// status and the grace collapses to nothing. Waiting on a live child
/// is no better: `bash` exits the moment it takes the `SIGTERM` while
/// the descendants it forked are still running their handlers, and
/// those descendants are who the grace is for.
///
/// The `SIGKILL` goes to the group whatever became of the leader,
/// because a reaped leader is exactly the drain-expiry case where a
/// pipe holder is still alive and still holding a turn's descriptors.
/// The stray-kill worry that would argue for skipping it does not
/// survive its own two cases: a group with a live member still owns
/// its id, since the id stays reserved for as long as the group is
/// non-empty, and a group that has emptied answers `ESRCH` and reaches
/// nobody. The signal is either on target or a no-op, which is what
/// `drain_capture` says about the same hazard on the completion path.
///
/// The residue, so that argument is not read as more than it is: once
/// the group has emptied *and* the leader has been reaped, the pid
/// number is free, so a `SIGKILL` landing after a full pid-space
/// wraparound inside the grace window could reach a stranger. That is
/// the same theoretical stray kill the completion path already
/// accepts, and reaping after the kill rather than before is what
/// keeps the leader's zombie pinning the id for the whole window.
async fn tear_down_process(armed: ArmedProcess) {
    let ArmedProcess {
        mut child,
        pgid,
        _cleanup,
        teardown: _,
        readers,
    } = armed;
    // Drop or synchronous termination sent SIGTERM before entering here. The
    // sleep is the window that signal gets.
    tokio::time::sleep(KILL_GRACE).await;
    signal_process_group(pgid, GroupSignal::Kill);
    if child.id().is_some() {
        // Bounded, because a `SIGKILL` is only delivered once the
        // target leaves an uninterruptible wait, and a teardown that
        // parks forever on one is a leak of a different shape.
        let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
    }
    // Dropping the read ends comes last. A command handling its
    // `SIGTERM` dies of `SIGPIPE` before it reaches its handler if its
    // output disappears first, which costs exactly the cleanup the
    // grace was for (measured against this teardown, not assumed).
    for reader in &readers {
        reader.abort();
    }
    drop(_cleanup);
}

/// Reap an optional host helper after `Drop` has synchronously killed its
/// process group. The cleanup lease remains here through the bounded reap
/// attempt, then the child handle and capture readers are released together.
async fn reap_killed_process(armed: ArmedProcess) {
    let ArmedProcess {
        mut child,
        pgid: _,
        _cleanup,
        teardown: _,
        readers,
    } = armed;
    reap_child_bounded(&mut child).await;
    for reader in &readers {
        reader.abort();
    }
    drop(_cleanup);
}

/// Reap a killed child when the kernel makes it available, without allowing an
/// uninterruptible wait to retain the caller's cleanup ownership indefinitely.
async fn reap_child_bounded(child: &mut Child) -> bool {
    child.id().is_none()
        || matches!(
            tokio::time::timeout(KILL_GRACE, child.wait()).await,
            Ok(Ok(_))
        )
}

/// Send one signal to a command's process group.
///
/// `pgid` is the child's pid, which equals its group id because the
/// child was spawned with `process_group(0)`. That also guarantees the
/// id is greater than 1, so this never targets group 0, which is our
/// own, or group 1, which `killpg` turns into the `kill(-1)` broadcast
/// to every process we are allowed to signal. Errors mean the group is
/// already gone (`ESRCH`) or we lack permission, and there is nothing
/// actionable to do with either. Process groups are a Unix notion.
/// Elsewhere a straggler keeps its descriptors until the readers are
/// dropped.
fn signal_process_group(pgid: i32, signal: GroupSignal) {
    debug_assert!(pgid > 1, "child pid/pgid must be > 1");
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let signal = match signal {
            GroupSignal::Term => Signal::SIGTERM,
            GroupSignal::Kill => Signal::SIGKILL,
        };
        let _ = killpg(Pid::from_raw(pgid), signal);
    }
    #[cfg(not(unix))]
    let _ = signal;
}

/// Terminate the child's whole process group and make a bounded reap attempt.
///
/// Sends `SIGTERM` to the group first so the command (and any
/// descendants the shell forked) can run their cleanup handlers, then
/// waits up to [`KILL_GRACE`] for the leader to exit before escalating
/// to an unconditional `SIGKILL`. Returns `false` only when the post-KILL reap
/// bound expires. The armed caller then releases its child, capture handles,
/// and cleanup lease together.
///
/// Only for a child that is still running: the grace window keys on
/// [`Child::wait`], which returns the cached status instantly once the
/// child has been reaped. [`drain_capture`] is what signals a group
/// after that point.
#[cfg(unix)]
async fn terminate_process_group(child: &mut Child, pgid: i32) -> bool {
    signal_process_group(pgid, GroupSignal::Term);
    if matches!(
        tokio::time::timeout(KILL_GRACE, child.wait()).await,
        Ok(Ok(_))
    ) {
        return true;
    }
    // A child still alive after the grace window, or one whose wait could not
    // establish an exit, gets an unconditional escalation. Bound the reap
    // independently because SIGKILL remains pending in uninterruptible I/O.
    signal_process_group(pgid, GroupSignal::Kill);
    reap_child_bounded(child).await
}

#[cfg(not(unix))]
async fn terminate_process_group(child: &mut Child, _pgid: i32) -> bool {
    // Process-group semantics are Unix-only; elsewhere we kill just the
    // immediate child and accept that shell-forked grandchildren may
    // leak.
    let _ = child.start_kill();
    reap_child_bounded(child).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use aj_agent::TaskRegistry;
    use aj_models::types::UserContent;
    use std::ffi::OsString;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{self, AtomicBool};
    use std::task::{Context, Poll};
    use tempfile::TempDir;
    use tokio::io::{AsyncRead, ReadBuf};
    use tokio_util::sync::CancellationToken;

    fn arm_for_test(child: Child) -> ProcessGuard {
        let registry = TaskRegistry::default();
        ProcessGuard::arm(child, registry.track_cleanup()).expect("arm")
    }

    fn hook_cleanup() -> TaskCleanupGuard {
        TaskRegistry::default().track_cleanup()
    }

    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected read failure",
            )))
        }
    }

    #[tokio::test]
    async fn read_stream_surfaces_pipe_read_failure() {
        let state = Arc::new(Mutex::new(StreamState::new()));
        let spill = Arc::new(Mutex::new(SpillState::new(None).expect("spill")));
        let error = Arc::new(Mutex::new(None));

        read_stream(FailingReader, state, spill, Arc::clone(&error), "stdout").await;

        assert_eq!(
            error.lock().unwrap().as_deref(),
            Some("stdout pipe read: injected read failure")
        );
    }

    #[tokio::test]
    async fn read_stream_surfaces_spill_write_failure() {
        let state = Arc::new(Mutex::new(StreamState::new()));
        let mut unavailable_spill = SpillState::new(None).expect("spill");
        unavailable_spill.file = None;
        let spill = Arc::new(Mutex::new(unavailable_spill));
        let error = Arc::new(Mutex::new(None));

        read_stream(
            &b"captured bytes"[..],
            Arc::clone(&state),
            spill,
            Arc::clone(&error),
            "stderr",
        )
        .await;

        assert_eq!(
            error.lock().unwrap().as_deref(),
            Some("stderr spill write: spill file is unavailable")
        );
        assert_eq!(state.lock().unwrap().total_bytes_seen, 0);
    }

    #[test]
    fn background_capture_failure_does_not_report_process_success() {
        assert_eq!(
            background_terminal_status(TaskStatus::Exited(Some(0)), true),
            TaskStatus::CaptureFailed(Some(0))
        );
        assert_eq!(
            background_terminal_status(TaskStatus::Exited(Some(7)), true),
            TaskStatus::CaptureFailed(Some(7))
        );
        assert_eq!(
            background_terminal_status(TaskStatus::Killed, true),
            TaskStatus::Killed
        );
    }

    #[tokio::test]
    async fn rtk_hook_check_rewrites_known_commands() {
        let host_path = std::env::var_os("PATH");
        let working_dir = std::env::current_dir().expect("current directory");
        let Some(rtk) = find_rtk_on_path(host_path.as_deref()) else {
            return;
        };
        // Plain single commands get the rtk prefix.
        assert_eq!(
            rtk_hook_check(&rtk, &working_dir, "git status", hook_cleanup())
                .await
                .as_deref(),
            Some("rtk git status")
        );
        // rtk's rewriter is shell-aware: it rewrites each eligible
        // subcommand in a compound, handles env/sudo prefixes, and
        // rewrites only the producer side of a pipe. We inherit that
        // by delegating rather than reimplementing it.
        assert_eq!(
            rtk_hook_check(
                &rtk,
                &working_dir,
                "cargo fmt && cargo check",
                hook_cleanup(),
            )
            .await
            .as_deref(),
            Some("rtk cargo fmt && rtk cargo check")
        );
        assert_eq!(
            rtk_hook_check(&rtk, &working_dir, "env FOO=bar git status", hook_cleanup(),)
                .await
                .as_deref(),
            Some("env FOO=bar rtk git status")
        );
        assert_eq!(
            rtk_hook_check(&rtk, &working_dir, "git log | grep foo", hook_cleanup(),)
                .await
                .as_deref(),
            Some("rtk git log | grep foo")
        );
    }

    #[tokio::test]
    async fn rtk_hook_check_returns_none_for_non_rewriteable() {
        let host_path = std::env::var_os("PATH");
        let working_dir = std::env::current_dir().expect("current directory");
        let Some(rtk) = find_rtk_on_path(host_path.as_deref()) else {
            return;
        };
        // rtk declines commands it has no proxy for (echo) and the
        // shell-builtin collision (test); we surface that as None and
        // run the original verbatim.
        assert_eq!(
            rtk_hook_check(&rtk, &working_dir, "echo hi", hook_cleanup()).await,
            None
        );
        assert_eq!(
            rtk_hook_check(&rtk, &working_dir, "test -f x", hook_cleanup()).await,
            None
        );
    }

    #[tokio::test]
    async fn rtk_rewrite_disabled_when_rtk_flag_off() {
        // With passthrough off the method short-circuits before
        // inspecting the host PATH or spawning rtk, so no rtk
        // installation is needed for this test.
        let tool = BashTool::new(false, None);
        let tasks = TaskRegistry::default();
        assert_eq!(
            tool.rtk_rewrite("git status", Path::new("."), &tasks).await,
            None
        );
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).expect("write executable fixture");
        let mut permissions = std::fs::metadata(path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make fixture executable");
    }

    #[cfg(unix)]
    fn host_executable(name: &str) -> PathBuf {
        let path = std::env::var_os("PATH").expect("supported host has PATH");
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .filter(|candidate| {
                candidate.is_file()
                    && nix::unistd::access(candidate, nix::unistd::AccessFlags::X_OK).is_ok()
            })
            .find_map(|candidate| candidate.canonicalize().ok())
            .unwrap_or_else(|| panic!("supported host PATH has executable {name}"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rtk_hook_check_rejects_non_utf8_output() {
        let dir = TempDir::new().expect("tempdir");
        let rtk = dir.path().join("rtk");
        write_executable(&rtk, "#!/bin/sh\nprintf 'rtk git status\\377\\n'\n");

        assert_eq!(
            rtk_hook_check(&rtk, dir.path(), "git status", hook_cleanup(),).await,
            None,
            "malformed hook output must fall back to the original command"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rtk_hook_check_rejects_nul_output() {
        let dir = TempDir::new().expect("tempdir");
        let rtk = dir.path().join("rtk");
        write_executable(&rtk, "#!/bin/sh\nprintf 'rtk git status\\000\n'\n");

        assert_eq!(
            rtk_hook_check(&rtk, dir.path(), "git status", hook_cleanup()).await,
            None,
            "an unrepresentable rewrite must fall back to the original command"
        );
    }

    #[test]
    fn rtk_rewrite_binds_only_inserted_helpers_to_the_selected_executable() {
        let selected = Path::new("/host/bin/rtk");
        assert_eq!(
            bind_rtk_rewrite(
                "PATH=/command/bin rtk git status && rtk cargo check",
                selected,
            )
            .as_deref(),
            Some("PATH=/command/bin '/host/bin/rtk' git status && '/host/bin/rtk' cargo check")
        );
        assert_eq!(
            bind_rtk_rewrite("rtk grep needle src", selected).as_deref(),
            Some("'/host/bin/rtk' grep needle src"),
            "canonicalizing hook rewrites must retain passthrough"
        );
        assert_eq!(
            bind_rtk_rewrite("artk git status", selected),
            None,
            "embedded text is not a helper command"
        );
    }

    #[test]
    fn rtk_rewrite_shell_quotes_the_selected_executable() {
        assert_eq!(
            bind_rtk_rewrite("rtk git status", Path::new("/selected path/it's/bin/rtk")).as_deref(),
            Some("'/selected path/it'\"'\"'s/bin/rtk' git status")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rtk_rewrite_declines_a_non_utf8_selected_executable() {
        use std::os::unix::ffi::OsStringExt;

        let selected = PathBuf::from(OsString::from_vec(b"/host/\xff/rtk".to_vec()));
        assert_eq!(bind_rtk_rewrite("rtk git status", &selected), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rtk_rewrite_declines_a_command_local_rtk_binding() {
        let dir = TempDir::new().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("create PATH directory");
        let marker = dir.path().join("hook-ran");
        write_executable(
            &bin.join("rtk"),
            &format!(
                "#!/bin/sh\nprintf hook > '{}'\nprintf 'rtk git status\\n'\n",
                marker.display()
            ),
        );
        let path = std::env::join_paths([&bin]).expect("fixture PATH");
        assert_eq!(
            find_rtk_on_path(Some(&path)),
            Some(bin.join("rtk")),
            "the fixture must offer the helper that would otherwise run"
        );

        let rewritten = BashTool::new(true, None)
            .rtk_rewrite_with_host_path(
                "rtk() { printf rebound; }; git status",
                Some(&path),
                dir.path(),
                &TaskRegistry::default(),
            )
            .await;
        assert_eq!(rewritten, None);
        assert!(
            !marker.exists(),
            "the hook ran despite the local rtk binding"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_local_path_cannot_retarget_the_selected_rtk() {
        let dir = TempDir::new().expect("tempdir");
        let host_bin = dir.path().join("host-bin");
        let command_bin = dir.path().join("command-bin");
        std::fs::create_dir(&host_bin).expect("create host PATH");
        std::fs::create_dir(&command_bin).expect("create command PATH");
        write_executable(
            &command_bin.join("git"),
            "#!/bin/sh\nprintf 'command-path-git\\n'\n",
        );
        let command = format!("PATH={} git status", command_bin.display());
        write_executable(
            &host_bin.join("rtk"),
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1 $2\" = \"hook check\" ]; then\n\
                   printf '%s\\n' 'PATH={} rtk git status'\n\
                 elif [ \"$1 $2\" = \"git status\" ]; then\n\
                   printf 'selected-host-rtk\\n'\n\
                 fi\n",
                command_bin.display()
            ),
        );
        assert!(
            !command_bin.join("rtk").exists(),
            "the command-local PATH must be unable to resolve literal rtk"
        );
        let host_path = std::env::join_paths([&host_bin]).expect("fixture host PATH");
        let rewritten = BashTool::new(true, None)
            .rtk_rewrite_with_host_path(
                &command,
                Some(&host_path),
                dir.path(),
                &TaskRegistry::default(),
            )
            .await
            .expect("the host helper rewrites the command");
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&rewritten)
            .current_dir(dir.path())
            .output()
            .expect("execute bound rewrite");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"selected-host-rtk\n");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn killed_helper_reap_releases_ownership_after_its_bound() {
        let sleep = host_executable("sleep");
        let mut command = Command::new(sleep);
        command.arg("30").process_group(0);
        let child = command.spawn().expect("spawn unreaped-child fixture");
        let pid: i32 = child
            .id()
            .expect("fixture pid")
            .try_into()
            .expect("fixture pid fits i32");
        let mut fixture = FixtureProcess(Some(pid));
        let registry = TaskRegistry::default();
        let stdout_reader = tokio::spawn(std::future::pending::<()>());
        let stderr_reader = tokio::spawn(std::future::pending::<()>());
        let armed = ArmedProcess {
            child,
            pgid: pid,
            _cleanup: registry.track_cleanup(),
            teardown: ProcessTeardown::Immediate,
            readers: vec![stdout_reader.abort_handle(), stderr_reader.abort_handle()],
        };

        // Do not signal this fixture. It stands in for a SIGKILL-pending child
        // that the kernel cannot reap yet, which ordinary process states cannot
        // reproduce safely in a test.
        let result = tokio::time::timeout(
            KILL_GRACE + Duration::from_secs(1),
            reap_killed_process(armed),
        )
        .await;
        assert!(result.is_ok(), "helper reap exceeded its ownership bound");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stdout_reader)
                .await
                .expect("stdout reader abort was bounded")
                .expect_err("stdout reader was aborted")
                .is_cancelled()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stderr_reader)
                .await
                .expect("stderr reader abort was bounded")
                .expect_err("stderr reader was aborted")
                .is_cancelled()
        );
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "bounded reap retained the session cleanup lease"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        wait_until(
            || !process_is_live(pid),
            "the unreaped-child fixture to terminate",
        )
        .await;
        fixture.disarm();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn terminating_helper_releases_readers_and_cleanup_after_its_bound() {
        let sleep = host_executable("sleep");
        let child = Command::new(sleep)
            .arg("30")
            .spawn()
            .expect("spawn unreaped-child fixture");
        let pid: i32 = child
            .id()
            .expect("fixture pid")
            .try_into()
            .expect("fixture pid fits i32");
        let mut fixture = FixtureProcess(Some(pid));
        let registry = TaskRegistry::default();
        let mut guard = ProcessGuard {
            armed: Some(ArmedProcess {
                child,
                // No process group owns this id, so terminate's SIGKILL cannot
                // make the fixture reapable. This deterministically models the
                // kernel-level wait the production bound protects against.
                pgid: i32::MAX,
                _cleanup: registry.track_cleanup(),
                teardown: ProcessTeardown::Immediate,
                readers: Vec::new(),
            }),
        };
        let stdout_reader = tokio::spawn(std::future::pending::<()>());
        let stderr_reader = tokio::spawn(std::future::pending::<()>());
        guard.watch_readers([stdout_reader.abort_handle(), stderr_reader.abort_handle()]);

        let result =
            tokio::time::timeout(KILL_GRACE + Duration::from_secs(1), guard.terminate()).await;
        assert!(
            result.is_ok(),
            "helper termination exceeded its ownership bound"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stdout_reader)
                .await
                .expect("stdout reader abort was bounded")
                .expect_err("stdout reader was aborted")
                .is_cancelled()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stderr_reader)
                .await
                .expect("stderr reader abort was bounded")
                .expect_err("stderr reader was aborted")
                .is_cancelled()
        );
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "bounded termination retained the session cleanup lease"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        wait_until(|| !process_is_live(pid), "the terminate fixture to stop").await;
        fixture.disarm();
    }

    /// A direct command can remain unavailable to `wait` after SIGKILL while
    /// the kernel finishes an uninterruptible operation. The TERM grace and
    /// post-KILL reap windows still end with one synchronous release of the
    /// child, capture readers, and session cleanup lease.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_command_reap_bound_releases_capture_and_cleanup_ownership() {
        let sleep = host_executable("sleep");
        let child = Command::new(sleep)
            .arg("30")
            .spawn()
            .expect("spawn unavailable direct-command fixture");
        let pid: i32 = child
            .id()
            .expect("fixture pid")
            .try_into()
            .expect("fixture pid fits i32");
        let mut fixture = FixtureProcess(Some(pid));
        let registry = TaskRegistry::default();
        let stdout_reader = tokio::spawn(std::future::pending::<()>());
        let stderr_reader = tokio::spawn(std::future::pending::<()>());
        let mut guard = ProcessGuard {
            armed: Some(ArmedProcess {
                child,
                // No process group owns this id, so neither signal makes the
                // live fixture reapable. That models a post-SIGKILL kernel wait
                // without placing the test process in uninterruptible I/O.
                pgid: i32::MAX,
                _cleanup: registry.track_cleanup(),
                teardown: ProcessTeardown::Graceful,
                readers: vec![stdout_reader.abort_handle(), stderr_reader.abort_handle()],
            }),
        };

        assert!(
            process_is_live(pid),
            "the fixture must be live before its unavailable reap is modelled"
        );
        let result = tokio::time::timeout(
            KILL_GRACE + KILL_GRACE + Duration::from_secs(1),
            guard.terminate_user_command(),
        )
        .await
        .expect("direct-command termination exceeded both of its bounds");
        assert_eq!(result, ProcessTermination::OwnershipReleased);
        assert!(
            process_is_live(pid),
            "the unavailable child exited, so the reap bound was not exercised"
        );
        for (stream, reader) in [("stdout", stdout_reader), ("stderr", stderr_reader)] {
            assert!(
                tokio::time::timeout(Duration::from_secs(1), reader)
                    .await
                    .unwrap_or_else(|_| panic!("{stream} reader release was not bounded"))
                    .unwrap_err()
                    .is_cancelled(),
                "{stream} reader completed instead of being released with capture ownership"
            );
        }
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "the direct-command reap bound retained its session cleanup lease"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        wait_until(
            || !Path::new(&format!("/proc/{pid}")).exists(),
            "the observed fixture process to reach reaped terminal state",
        )
        .await;
        fixture.disarm();
    }

    /// The detached driver uses the same direct-command termination boundary.
    /// Its terminal registry row is not enough: quiescence also requires the
    /// driver record, capture readers, and process cleanup lease to be gone.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_background_command_settles_after_an_unavailable_reap() {
        let sleep = host_executable("sleep");
        let child = Command::new(sleep)
            .arg("30")
            .spawn()
            .expect("spawn unavailable background-command fixture");
        let pid: i32 = child
            .id()
            .expect("fixture pid")
            .try_into()
            .expect("fixture pid fits i32");
        let mut fixture = FixtureProcess(Some(pid));
        let mut ctx = DummyToolContext::default();
        let registry = ctx.task_registry();
        let stdout_reader = tokio::spawn(std::future::pending::<()>());
        let stderr_reader = tokio::spawn(std::future::pending::<()>());
        let stdout_observer = stdout_reader.abort_handle();
        let stderr_observer = stderr_reader.abort_handle();
        let process = ProcessGuard {
            armed: Some(ArmedProcess {
                child,
                pgid: i32::MAX,
                _cleanup: registry.track_cleanup(),
                teardown: ProcessTeardown::Graceful,
                readers: vec![stdout_reader.abort_handle(), stderr_reader.abort_handle()],
            }),
        };
        let stdout_state = Arc::new(Mutex::new(StreamState::new()));
        let stderr_state = Arc::new(Mutex::new(StreamState::new()));
        let spill_dir = TempDir::new().expect("spill tempdir");
        let spill_path = spill_dir.path().join("background.log");
        std::fs::write(&spill_path, []).expect("create spill fixture");
        let kind = TaskKind::Bash {
            command: "unavailable reap fixture".to_string(),
        };
        let StartedTask {
            id,
            cancel,
            events,
            driver,
        } = ctx.start_background_task(
            kind,
            "unavailable reap fixture".to_string(),
            Arc::new(BashTaskOutput {
                stdout: Arc::clone(&stdout_state),
                stderr: Arc::clone(&stderr_state),
                spill_path: spill_path.clone(),
            }),
        );
        driver.spawn(drive_background_bash(BackgroundBash {
            process,
            child_pid: pid,
            stdout_reader,
            stderr_reader,
            stdout_state,
            stderr_state,
            capture_error: Arc::new(Mutex::new(None)),
            spill_path,
            command: "unavailable reap fixture".to_string(),
            task_id: id,
            cancel,
            events,
        }));

        assert!(registry.kill(id), "cancel the live background driver");
        assert!(
            registry
                .quiesce(KILL_GRACE + KILL_GRACE + Duration::from_secs(1))
                .await,
            "the background driver exceeded the direct-command ownership bound"
        );
        assert_eq!(registry.status(id), Some(TaskStatus::Killed));
        assert!(
            stdout_observer.is_finished() && stderr_observer.is_finished(),
            "background capture outlived terminal driver ownership"
        );
        let notices = registry.drain_notices(aj_agent::events::AgentId::Main);
        assert_eq!(notices.len(), 1, "the cancelled driver reports once");
        let notice = &notices[0].body;
        assert!(
            notice.contains(&reap_release_trailer()),
            "the notice names the reap boundary that cut capture: {notice:?}"
        );
        assert!(
            !notice.contains("Output capture failed") && !notice.contains(&capture_cut_trailer()),
            "expected reader cancellation was reported as a capture failure or pipe-holder cut: \
             {notice:?}"
        );
        assert!(
            process_is_live(pid),
            "the unavailable background child exited, so the reap bound was not exercised"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        wait_until(
            || !Path::new(&format!("/proc/{pid}")).exists(),
            "the observed background fixture to reach reaped terminal state",
        )
        .await;
        fixture.disarm();
    }

    #[cfg(target_os = "linux")]
    struct FixtureProcess(Option<i32>);

    #[cfg(target_os = "linux")]
    impl FixtureProcess {
        fn disarm(&mut self) {
            self.0 = None;
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for FixtureProcess {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rtk_hook_timeout_terminates_helper_descendants() {
        let started = std::time::Instant::now();
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::bash::tests::rtk_hook_timeout_child",
                "--nocapture",
            ])
            .env("AJ_RTK_TIMEOUT_FIXTURE", "1")
            .output()
            .expect("run isolated hook-timeout test");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "hook timeout did not promptly own down its process group: {output:?}"
        );
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the exact child test did not run: {output:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rtk_hook_timeout_child() {
        if std::env::var_os("AJ_RTK_TIMEOUT_FIXTURE").is_none() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let rtk = dir.path().join("rtk");
        let leader_pid_file = dir.path().join("rtk.pid");
        let pid_file = dir.path().join("rtk.child.pid");
        write_executable(
            &rtk,
            "#!/bin/sh\nprintf '%s\n' \"$$\" > \"$0.pid\"\n/bin/sh -c 'trap \"\" TERM; exec /bin/sleep 10' &\nprintf '%s\n' \"$!\" > \"$0.child.pid\"\nwait\n",
        );
        let registry = TaskRegistry::default();

        assert_eq!(
            rtk_hook_check_with_timeout(
                &rtk,
                dir.path(),
                "git status",
                registry.track_cleanup(),
                Duration::from_secs(2),
            )
            .await,
            None,
        );
        let leader_pid = read_pid(&leader_pid_file);
        let pid = read_pid(&pid_file);
        let mut leader = FixtureProcess(Some(leader_pid));
        let mut fixture = FixtureProcess(Some(pid));
        assert!(
            !Path::new(&format!("/proc/{leader_pid}")).exists(),
            "the timed-out hook leader {leader_pid} was not reaped"
        );
        wait_until(
            || !process_is_live(pid),
            "the timed-out hook descendant to terminate",
        )
        .await;
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "hook cleanup lease survived synchronous timeout cleanup"
        );
        leader.disarm();
        fixture.disarm();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_rtk_hook_check_terminates_helper_descendants() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::bash::tests::dropping_rtk_hook_check_child",
                "--nocapture",
            ])
            .env("AJ_RTK_DROP_FIXTURE", "1")
            .output()
            .expect("run isolated hook-cancellation test");
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the exact child test did not run: {output:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_rtk_hook_check_child() {
        if std::env::var_os("AJ_RTK_DROP_FIXTURE").is_none() {
            return;
        }
        let dir = TempDir::new().expect("tempdir");
        let rtk = dir.path().join("rtk");
        let leader_pid_file = dir.path().join("rtk.pid");
        let pid_file = dir.path().join("rtk.child.pid");
        write_executable(
            &rtk,
            "#!/bin/sh\nprintf '%s\n' \"$$\" > \"$0.pid\"\n/bin/sh -c 'trap \"\" TERM; exec /bin/sleep 10' &\nprintf '%s\n' \"$!\" > \"$0.child.pid\"\nwait\n",
        );
        let registry = TaskRegistry::default();

        drop_when_ready(
            rtk_hook_check_with_timeout(
                &rtk,
                dir.path(),
                "git status",
                registry.track_cleanup(),
                Duration::from_secs(5),
            ),
            &pid_file,
        )
        .await;
        let leader_pid = read_pid(&leader_pid_file);
        let pid = read_pid(&pid_file);
        let mut leader = FixtureProcess(Some(leader_pid));
        let mut fixture = FixtureProcess(Some(pid));
        assert!(
            registry.quiesce(Duration::from_secs(1)).await,
            "outer-cancelled hook teardown did not release its cleanup lease"
        );
        assert!(
            !Path::new(&format!("/proc/{leader_pid}")).exists(),
            "the cancelled hook leader {leader_pid} was not reaped"
        );
        wait_until(
            || !process_is_live(pid),
            "the cancelled rtk hook descendant to terminate",
        )
        .await;
        leader.disarm();
        fixture.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn rtk_path_lookup_ignores_the_windows_executable_name() {
        let dir = TempDir::new().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).expect("create first PATH directory");
        std::fs::create_dir(&second).expect("create second PATH directory");
        write_executable(&first.join("rtk.exe"), "#!/bin/sh\nexit 0\n");
        write_executable(&second.join("rtk"), "#!/bin/sh\nexit 0\n");
        let path = std::env::join_paths([&first, &second]).expect("join fixture PATH");

        assert_eq!(find_rtk_on_path(Some(&path)), Some(second.join("rtk")));
    }

    #[cfg(unix)]
    #[test]
    fn rtk_selection_and_hook_environment_belong_to_the_host() {
        let dir = TempDir::new().expect("tempdir");
        let shadow_bin = dir.path().join("shadow-bin");
        let host_bin = dir.path().join("host-bin");
        let session_bin = dir.path().join("session-bin");
        std::fs::create_dir(&shadow_bin).expect("create shadow bin");
        std::fs::create_dir(&host_bin).expect("create host bin");
        std::fs::create_dir(&session_bin).expect("create session bin");
        std::fs::write(
            shadow_bin.join("rtk"),
            "#!/bin/sh\nprintf 'non-executable-shadow\\n'\n",
        )
        .expect("write non-executable rtk shadow");
        assert!(
            nix::unistd::access(&shadow_bin.join("rtk"), nix::unistd::AccessFlags::X_OK,).is_err(),
            "the earlier PATH candidate must be genuinely unexecutable"
        );
        let bash = host_executable("bash");
        std::os::unix::fs::symlink(bash, session_bin.join("bash"))
            .expect("link bash into session PATH");
        write_executable(
            &host_bin.join("rtk"),
            "#!/bin/sh\n\
             if [ \"$1 $2\" = \"hook check\" ]; then\n\
               [ \"$PATH\" = \"$AJ_RTK_EXPECTED_HOST_PATH\" ] || exit 20\n\
               [ -z \"${AJ_RTK_SESSION_ONLY+x}\" ] || exit 21\n\
               printf 'rtk host-probe\\n'\n\
             elif [ \"$1\" = \"host-probe\" ]; then\n\
               [ \"$AJ_RTK_SESSION_ONLY\" = session ] || exit 22\n\
               printf 'host-rewrite\\n'\n\
             fi\n",
        );
        write_executable(
            &session_bin.join("rtk"),
            "#!/bin/sh\nif [ \"$1 $2\" = \"hook check\" ]; then printf 'rtk session-probe\\n'; else printf 'session-rewrite\\n'; fi\n",
        );
        write_executable(
            &session_bin.join("git"),
            "#!/bin/sh\nprintf 'raw-session-git\\n'\n",
        );

        let host_path = std::env::join_paths([&shadow_bin, &host_bin]).expect("join host PATH");
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::bash::tests::rtk_host_contract_child",
                "--nocapture",
            ])
            .env("PATH", &host_path)
            .env("AJ_RTK_EXPECTED_HOST_PATH", &host_path)
            .env("AJ_RTK_SESSION_BIN", &session_bin)
            .output()
            .expect("run isolated host-environment test");
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the exact child test did not run: {output:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rtk_host_contract_child() {
        let Some(session_bin) = std::env::var_os("AJ_RTK_SESSION_BIN") else {
            return;
        };
        let session_bin = PathBuf::from(session_bin);
        let mut ctx = DummyToolContext {
            working_directory: session_bin
                .parent()
                .expect("session bin has a parent")
                .to_path_buf(),
            session_env: std::collections::BTreeMap::from([
                (
                    "PATH".to_string(),
                    session_bin.to_string_lossy().into_owned(),
                ),
                ("AJ_RTK_SESSION_ONLY".to_string(), "session".to_string()),
            ]),
            ..DummyToolContext::default()
        };
        let outcome = BashTool::new(true, Some(ctx.working_directory.clone()))
            .execute(
                &mut ctx,
                BashInput {
                    command: "git status".to_string(),
                    timeout: 5,
                    description: "test host-owned rtk".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute with distinct host and session environments");
        let ToolDetails::Bash {
            exit_code,
            stdout,
            stderr,
            ..
        } = &outcome.details
        else {
            panic!("expected Bash details, got {:?}", outcome.details)
        };
        assert_eq!(*exit_code, Some(0), "stderr: {stderr}");
        assert_eq!(stdout, "host-rewrite\n", "stderr: {stderr}");
    }

    #[cfg(unix)]
    #[test]
    fn rtk_rewrite_declines_an_empty_path_component() {
        let dir = TempDir::new().expect("tempdir");
        write_executable(&dir.path().join("rtk"), "#!/bin/sh\nexit 0\n");
        assert_eq!(
            find_rtk_on_path(Some(OsStr::new(""))),
            None,
            "an empty component moves with the shell cwd"
        );
    }

    /// `ToolContext` wrapper that records every `emit_update` snapshot
    /// for assertion. Delegates everything else to a [`DummyToolContext`].
    struct RecordingCtx {
        inner: DummyToolContext,
        updates: Arc<StdMutex<Vec<ToolDetails>>>,
    }

    impl RecordingCtx {
        fn new() -> (Self, Arc<StdMutex<Vec<ToolDetails>>>) {
            let updates = Arc::new(StdMutex::new(Vec::new()));
            let ctx = Self {
                inner: DummyToolContext::default(),
                updates: Arc::clone(&updates),
            };
            (ctx, updates)
        }
    }

    impl ToolContext for RecordingCtx {
        fn working_directory(&self) -> PathBuf {
            self.inner.working_directory()
        }

        fn get_todo_list(&self) -> Vec<aj_agent::tool::TodoItem> {
            self.inner.get_todo_list()
        }

        fn set_todo_list(&mut self, todos: Vec<aj_agent::tool::TodoItem>) {
            self.inner.set_todo_list(todos);
        }

        fn spawn_agent<'a>(
            &'a mut self,
            task: String,
            mode: aj_agent::tool::SpawnMode,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<aj_agent::tool::SpawnResult, aj_agent::BoxError>,
                    > + Send
                    + 'a,
            >,
        > {
            self.inner.spawn_agent(task, mode)
        }

        fn emit_update<'a>(
            &'a mut self,
            partial: ToolDetails,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            self.updates.lock().unwrap().push(partial);
            Box::pin(async {})
        }

        fn cancellation(&self) -> CancellationToken {
            self.inner.cancellation.clone()
        }

        fn task_registry(&self) -> aj_agent::TaskRegistry {
            self.inner.task_registry()
        }

        fn agent_id(&self) -> aj_agent::events::AgentId {
            self.inner.agent_id()
        }

        fn start_background_task(
            &mut self,
            kind: aj_agent::tool::TaskKind,
            label: String,
            output: Arc<dyn aj_agent::tool::TaskOutputSource>,
        ) -> aj_agent::tool::StartedTask {
            self.inner.start_background_task(kind, label, output)
        }
    }

    fn extract_text(content: &[UserContent]) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Locks in `Sequential` execution mode — bash runs arbitrary
    /// commands and must serialize against any other in-flight tool
    /// call.
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(
            BashTool::default().execution_mode(),
            ExecutionMode::Sequential
        );
    }

    /// Successful command. Wire content carries stdout verbatim;
    /// structured details report exit 0 with no truncation and no
    /// spill file.
    #[tokio::test]
    async fn echo_returns_stdout_and_exit_zero() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo hello".to_string(),
                    timeout: 30,
                    description: "test echo".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        assert_eq!(extract_text(&outcome.content), "hello\n");
        match &outcome.details {
            ToolDetails::Bash {
                command,
                stdout,
                stderr,
                exit_code,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                task_id: _,
            } => {
                assert_eq!(command, "echo hello");
                assert_eq!(stdout, "hello\n");
                assert!(stderr.is_empty(), "stderr: {stderr:?}");
                assert_eq!(*exit_code, Some(0));
                assert!(!*truncated);
                assert!(full_output_path.is_none());
                assert!(stdout_truncation.is_none());
                assert!(stderr_truncation.is_none());
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// Non-zero exit code surfaces in both the wire content (the
    /// "Command failed with exit code: N" line) and the structured
    /// payload's `exit_code`. We don't mark it as `is_error` — the wire
    /// content already carries the failure for the model.
    #[tokio::test]
    async fn nonzero_exit_code_is_not_marked_as_error() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo fail; exit 7".to_string(),
                    timeout: 30,
                    description: "test failing exit".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("fail"), "wire: {wire:?}");
        assert!(
            wire.contains("Command failed with exit code: 7"),
            "wire: {wire:?}"
        );
        match &outcome.details {
            ToolDetails::Bash { exit_code, .. } => {
                assert_eq!(*exit_code, Some(7));
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// stderr captures show up under a `STDERR:` header on the wire,
    /// and as a separate field in the structured payload.
    #[tokio::test]
    async fn stderr_is_captured_under_its_own_header() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo to-stdout; echo to-stderr 1>&2".to_string(),
                    timeout: 30,
                    description: "test stderr".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("to-stdout\n"), "wire: {wire:?}");
        assert!(wire.contains("STDERR:\n"), "wire: {wire:?}");
        assert!(wire.contains("to-stderr"), "wire: {wire:?}");
        match &outcome.details {
            ToolDetails::Bash { stdout, stderr, .. } => {
                assert!(stdout.contains("to-stdout"), "stdout: {stdout:?}");
                assert!(stderr.contains("to-stderr"), "stderr: {stderr:?}");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// Output exceeding the per-stream cap is truncated in the
    /// structured payload but the spill file retains the full output;
    /// `truncated = true`, the structured per-stream summary is set,
    /// and `full_output_path` is populated. The wire content picks up
    /// the `[Showing lines X-Y of TOTAL ...]` marker.
    #[allow(clippy::as_conversions)]
    #[tokio::test]
    async fn large_output_truncates_and_spills_to_temp_file() {
        let mut ctx = DummyToolContext::default();
        // Print enough bytes to overflow the 50 KiB per-stream cap.
        // `yes` would be unbounded; bound it with `head -c` so the
        // command terminates naturally. Each "ABCDEFGH\n" is 9 bytes,
        // so 200 KB ≈ 22_756 lines — well over the 2000-line cap too.
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "yes ABCDEFGH | head -c 200000".to_string(),
                    timeout: 30,
                    description: "test truncation".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Bash {
                stdout,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                ..
            } => {
                assert!(*truncated, "expected truncation");
                let path = full_output_path.as_ref().expect("spill path on truncation");
                let on_disk = std::fs::read_to_string(path).expect("read spill");
                assert!(
                    on_disk.len() >= 200_000,
                    "spill should hold the full output, got {} bytes",
                    on_disk.len()
                );
                assert!(stderr_truncation.is_none(), "stderr did not overflow");
                let t = stdout_truncation
                    .as_ref()
                    .expect("stdout_truncation should be set");
                assert!(t.total_bytes >= 200_000, "total_bytes: {}", t.total_bytes);
                assert!(t.total_lines > 2000, "total_lines: {}", t.total_lines);
                // Output is capped: either at the line cap or the
                // byte cap, whichever fired first.
                assert!(t.output_lines <= 2000);
                assert!(t.output_bytes <= 50 * 1024);
                assert!(!t.last_line_partial, "uniform-line output is not partial");
                // The captured stdout matches what the structured
                // summary describes.
                assert_eq!(
                    stdout.len() as u64,
                    t.output_bytes,
                    "stdout length should equal output_bytes"
                );
                std::fs::remove_file(path).ok();
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("[Showing lines "),
            "wire should mention truncation: {:?}",
            &wire[wire.len().saturating_sub(200)..]
        );
        assert!(
            wire.contains(" of stdout"),
            "marker should name the stream: {:?}",
            &wire[wire.len().saturating_sub(200)..]
        );
    }

    /// A single line bigger than the byte cap triggers the
    /// `last_line_partial` path: the marker switches to the
    /// `[Showing last <output> of <stream> line N (line is <full>)...]`
    /// form, and `stdout` carries only the tail of the line.
    #[tokio::test]
    async fn single_huge_line_emits_last_line_partial_marker() {
        let mut ctx = DummyToolContext::default();
        // Truncating persists the spill, so the tool writes into a directory
        // this test owns.
        let spill_dir = TempDir::new().expect("create temp dir");
        // One ~120 KB line with no internal newlines, no trailing
        // newline. Exceeds the 50 KB byte cap; line cap is irrelevant
        // (one line total).
        let outcome = BashTool::new(false, Some(spill_dir.path().to_path_buf()))
            .execute(
                &mut ctx,
                BashInput {
                    command: "head -c 120000 /dev/zero | tr '\\0' 'x'".to_string(),
                    timeout: 30,
                    description: "test last_line_partial".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        match &outcome.details {
            ToolDetails::Bash {
                stdout,
                stdout_truncation,
                full_output_path,
                ..
            } => {
                let path = full_output_path.as_ref().expect("a truncated run spills");
                assert!(
                    path.starts_with(spill_dir.path()),
                    "the spill honours the configured directory: {path:?}",
                );
                let t = stdout_truncation
                    .as_ref()
                    .expect("partial-line case should populate truncation");
                assert!(t.last_line_partial, "expected last_line_partial");
                assert_eq!(t.output_lines, 1);
                assert!(t.last_line_bytes >= 120_000);
                assert!(stdout.len() <= 50 * 1024 + 16);
                // Verify the marker uses the partial-line phrasing.
                assert!(
                    wire.contains("[Showing last "),
                    "wire tail: {:?}",
                    &wire[wire.len().saturating_sub(200)..]
                );
                assert!(
                    wire.contains("(line is "),
                    "wire tail: {:?}",
                    &wire[wire.len().saturating_sub(200)..]
                );
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// Cancellation kills the process and surfaces an `is_error: true`
    /// outcome with no exit code.
    #[tokio::test]
    async fn cancellation_kills_command_and_marks_error() {
        let (mut ctx, _updates) = RecordingCtx::new();
        let token = ctx.cancellation();
        // Trigger cancellation shortly after the bash command starts.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            token.cancel();
        });

        let start = Instant::now();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "sleep 30".to_string(),
                    timeout: 60,
                    description: "test cancellation".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "cancellation should be near-instant, took {elapsed:?}"
        );
        assert!(outcome.is_error, "cancellation should mark is_error");
        match &outcome.details {
            ToolDetails::Bash { exit_code, .. } => {
                assert!(exit_code.is_none(), "killed process has no exit code");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("Command cancelled"), "wire: {wire:?}");
    }

    /// A command that ignores `SIGTERM` is still killed: once the grace
    /// window elapses we escalate to `SIGKILL`, so cancellation finishes
    /// long before the command's natural runtime. Without escalation the
    /// whole group would shrug off the `SIGTERM` and run for the full
    /// timeout.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancellation_escalates_to_sigkill_when_sigterm_is_ignored() {
        let dir = TempDir::new().expect("tempdir");
        let pid_path = dir.path().join("command.pid");
        let (mut ctx, _updates) = RecordingCtx::new();
        let registry = ctx.task_registry();
        let token = ctx.cancellation();
        let ready = pid_path.clone();
        let canceller = tokio::spawn(async move {
            wait_until(
                || std::fs::metadata(&ready).is_ok_and(|metadata| metadata.len() > 0),
                "the cancellation fixture to publish its pid",
            )
            .await;
            token.cancel();
        });

        let start = Instant::now();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    // `trap '' TERM` makes the shell ignore SIGTERM; the
                    // loop keeps it (and thus the group) alive until the
                    // escalation SIGKILL lands.
                    command: format!(
                        "trap '' TERM; printf '%s' $$ > '{}'; while true; do sleep 0.2; done",
                        pid_path.display()
                    ),
                    timeout: 60,
                    description: "test sigkill escalation".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");
        canceller.await.expect("cancellation fixture");
        let elapsed = start.elapsed();
        let pid = read_pid(&pid_path);

        // We waited out the grace window (proving SIGTERM alone did not
        // end it) but still finished far short of the 60s timeout.
        assert!(
            elapsed >= KILL_GRACE,
            "should have waited the grace window before SIGKILL, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "escalation should kill shortly after the grace window, took {elapsed:?}"
        );
        assert!(outcome.is_error, "cancellation should mark is_error");
        match &outcome.details {
            ToolDetails::Bash { exit_code, .. } => {
                assert!(exit_code.is_none(), "killed process has no exit code");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("Command cancelled"), "wire: {wire:?}");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "cancellation returned before its command leader was reaped"
        );
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "cancellation returned with process cleanup still owned"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_kills_command_and_marks_error() {
        let dir = TempDir::new().expect("tempdir");
        let pid_path = dir.path().join("command.pid");
        let mut ctx = DummyToolContext::default();
        let registry = ctx.task_registry();
        let start = Instant::now();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: format!(
                        "trap '' TERM; printf '%s' $$ > '{}'; while true; do sleep 0.2; done",
                        pid_path.display()
                    ),
                    timeout: 1,
                    description: "test timeout".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");
        let elapsed = start.elapsed();
        let pid = read_pid(&pid_path);

        assert!(
            elapsed >= Duration::from_secs(1) + KILL_GRACE,
            "timeout did not preserve the command's TERM grace, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "timeout and process teardown should remain bounded, took {elapsed:?}"
        );
        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Bash { exit_code, .. } => {
                assert!(exit_code.is_none());
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Command timed out after 1 seconds"),
            "wire: {wire:?}"
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "timeout returned before its command leader was reaped"
        );
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "timeout returned with process cleanup still owned"
        );
    }

    /// A command whose shell exits while a descendant still holds a
    /// pipe write end must not hold the turn open until that descendant
    /// is done: `read_stream` returns only on EOF, and EOF needs every
    /// write end closed, so the wait is otherwise unbounded. The
    /// timeout does not cover this case, the loop breaks on
    /// `ChildExit::Exited` and discards the deadline with it.
    ///
    /// Linux-only: the fixture proves the descendant inherited fd 1 by
    /// reading it back from `/proc`, which is the one proof that
    /// survives a run where the turn never returns at all.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn descendant_holding_a_pipe_does_not_hold_the_turn_open() {
        /// How long the turn may take once the shell has exited. Well
        /// above any bounded drain, well below the descendant's own
        /// lifetime, so a run that reaches it is waiting on EOF.
        const BOUND: Duration = Duration::from_secs(5);

        let dir = TempDir::new().expect("create temp dir");
        let fd1_path = dir.path().join("descendant-fd1");
        let pid_path = dir.path().join("descendant-pid");

        // The descendant records its fd 1, its pid, and writes to stdout
        // before the shell is allowed to exit, so the evidence lands
        // whatever the tool does next. It then holds the pipe until this
        // test drops `dir`, with `$SECONDS` as a backstop so it cannot
        // outlive the test process by more than a few seconds.
        let command = format!(
            "{{ readlink /proc/$BASHPID/fd/1 > '{fd1}'; \
                echo $BASHPID > '{pid}'; \
                echo descendant-wrote; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{fd1}' ] && [ -s '{pid}' ]; do sleep 0.01; done; \
             echo shell-done",
            fd1 = fd1_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let result = tokio::time::timeout(
            BOUND,
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    // Short so that a fix which spends the command's
                    // remaining timeout budget on the drain still
                    // finishes inside `BOUND`.
                    timeout: 1,
                    description: "test descendant holding the pipes".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        let inherited = std::fs::read_to_string(&fd1_path).unwrap_or_default();
        assert!(
            inherited.starts_with("pipe:"),
            "the descendant should have inherited the stdout pipe, its fd 1 was {inherited:?}: \
             with nothing holding a write end this test measures nothing"
        );

        let outcome = result
            .unwrap_or_else(|_| {
                panic!(
                    "the shell exited at once, but the turn was still waiting on capture \
                     after {BOUND:?}, with the descendant still holding the pipe"
                )
            })
            .expect("execute");

        assert!(!outcome.is_error, "the command itself succeeded");
        match &outcome.details {
            ToolDetails::Bash {
                exit_code, stdout, ..
            } => {
                // `Some(0)` is what puts this run on the `Exited` arm
                // of the select: a timeout or a cancel leaves it unset.
                assert_eq!(*exit_code, Some(0), "the shell exited normally");
                assert!(
                    stdout.contains("shell-done"),
                    "the shell's own output is reported, stdout: {stdout:?}"
                );
                assert!(
                    stdout.contains("descendant-wrote"),
                    "output written before the shell exited is reported, stdout: {stdout:?}"
                );
            }
            other => panic!("expected Bash details, got {other:?}"),
        }

        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains(&capture_cut_trailer()),
            "the model is told the capture was cut short, wire: {wire:?}"
        );
        let pid = read_pid(&pid_path);
        wait_until(
            || !process_is_live(pid),
            "the pipe holder to be killed with its group",
        )
        .await;
    }

    /// The rule from the other side: a straggler that redirected the
    /// tool's stdout and stderr away has let go of the capture channel,
    /// so it reaches EOF at once and is none of the tool's business. It
    /// keeps running and nothing is reported.
    ///
    /// This is what keeps the drain from becoming an unconditional kill
    /// of everything a command leaves behind, which would break every
    /// `nohup`-style daemon that behaves.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_straggler_that_let_go_of_the_pipes_is_left_alone() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("daemon-pid");

        // `exec` drops this subshell's copies of the pipes before it
        // does anything else, which is the daemon discipline the tool
        // description asks for. The shell waits for its pid so the test
        // never races the fork.
        let command = format!(
            "{{ exec >/dev/null 2>&1; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             echo shell-done",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let start = Instant::now();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 30,
                    description: "test daemon that redirected its output".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");
        let elapsed = start.elapsed();

        let pid = read_pid(&pid_path);
        // Long enough for a teardown that should never have started to
        // have signalled: the guard is disarmed on this path, and
        // without the wait a stray SIGTERM would still be in flight
        // when the assertion reads the process table.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            process_is_live(pid),
            "the straggler let go of the pipes and must be left running: \
             a test that kills it here measures nothing"
        );
        assert!(
            elapsed < CAPTURE_DRAIN_GRACE,
            "capture closed with the shell, so no drain window should have been \
             spent, took {elapsed:?}"
        );
        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            !wire.contains(&capture_cut_trailer()),
            "nothing was cut short, so nothing is reported, wire: {wire:?}"
        );
        match &outcome.details {
            ToolDetails::Bash {
                exit_code, stdout, ..
            } => {
                assert_eq!(*exit_code, Some(0));
                assert!(stdout.contains("shell-done"), "stdout: {stdout:?}");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// A holder in its own session survives the process-group kill, so
    /// dropping the pipe read ends is the only thing that ends the wait.
    /// This is the timeout path, where the group kill already ran and a
    /// `setsid`ed descendant walked away from it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_holder_outside_the_group_is_bounded_by_dropping_the_pipes() {
        /// Timeout, two drain windows, the post-kill window, and room
        /// for a loaded machine.
        const BOUND: Duration = Duration::from_secs(12);

        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("session-leader-pid");

        // `setsid` leaves the tool's process group, keeping the
        // inherited pipes. The shell then blocks so the run ends on the
        // timeout rather than on child exit.
        let command = format!(
            "setsid bash -c \"echo \\$$ > '{pid}'; \
                while [ -d '{dir}' ] && [ \\$SECONDS -lt 30 ]; do sleep 0.05; done\" & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let result = tokio::time::timeout(
            BOUND,
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 1,
                    description: "test holder outside the process group".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        let pid = read_pid(&pid_path);
        let outcome = result
            .unwrap_or_else(|_| {
                panic!(
                    "the command timed out at 1s, but the turn was still waiting on capture \
                     after {BOUND:?}: the group kill cannot reach a holder in its own session"
                )
            })
            .expect("execute");

        assert!(
            process_is_live(pid),
            "the holder is outside the group and no signal of ours reaches it, so the wait \
             ended by dropping the read ends: with it dead this test measures the kill instead"
        );
        assert!(outcome.is_error, "the command timed out");
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Command timed out after 1 seconds"),
            "wire: {wire:?}"
        );
        assert!(
            wire.contains(&capture_cut_trailer()),
            "the incomplete capture is reported beside the timeout, wire: {wire:?}"
        );
    }

    /// A straggler can hold one stream and not the other, so the drain
    /// rounds have to cope with the two readers finishing in different
    /// windows: here stdout reaches EOF with the shell and stderr is
    /// held open past the grace.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_straggler_holding_one_stream_is_drained_like_any_other() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("stderr-holder-pid");

        // `exec >/dev/null` drops only this subshell's stdout, so the
        // stderr pipe is the one thing keeping capture open.
        let command = format!(
            "{{ exec >/dev/null; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             echo shell-done",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 1,
                    description: "test straggler holding stderr only".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        let pid = read_pid(&pid_path);
        let outcome = result
            .unwrap_or_else(|_| panic!("the turn was still waiting on the held stderr pipe"))
            .expect("execute");

        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains(&capture_cut_trailer()),
            "one held stream is enough to cut the capture short, wire: {wire:?}"
        );
        match &outcome.details {
            ToolDetails::Bash {
                exit_code, stdout, ..
            } => {
                assert_eq!(*exit_code, Some(0));
                // The stream that did close is complete: a partial
                // drain must not cost the output it already had.
                assert!(stdout.contains("shell-done"), "stdout: {stdout:?}");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        wait_until(
            || !process_is_live(pid),
            "the stderr holder to be killed with its group",
        )
        .await;
    }

    /// The drain escalates rather than giving up: the group gets a
    /// `SIGTERM` first, so a holder that cleans up on signals gets the
    /// chance, and a holder that takes the signal and keeps the pipes
    /// anyway is killed in the next window.
    ///
    /// Mirrors `cancellation_escalates_to_sigkill_when_sigterm_is_ignored`
    /// for the drain's own escalation, which cannot key on the child
    /// because the child has already been reaped.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_drain_escalates_from_sigterm_to_sigkill() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("holder-pid");
        let termed_path = dir.path().join("holder-was-termed");

        // The holder records `SIGTERM` and carries on holding the pipes,
        // so the run can only end on the escalation. `trap` fires
        // between commands, i.e. within one `sleep` of the signal.
        let command = format!(
            "{{ trap \"echo termed > '{termed}'\" TERM; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             echo shell-done",
            termed = termed_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 1,
                    description: "test drain escalation".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        let pid = read_pid(&pid_path);
        let outcome = result
            .unwrap_or_else(|_| panic!("the turn was still waiting on the held pipes"))
            .expect("execute");

        assert!(
            termed_path.exists(),
            "the group is signalled before it is killed, so a holder can clean up: \
             without the TERM this test measures only the kill"
        );
        wait_until(
            || !process_is_live(pid),
            "the holder that shrugged off SIGTERM to be killed",
        )
        .await;
        let wire = extract_text(&outcome.content);
        assert!(wire.contains(&capture_cut_trailer()), "wire: {wire:?}");
    }

    /// A drop can land mid-drain, when the child has already been
    /// reaped and the drain is waiting on a straggler's pipes. The
    /// guard has to tear the group down from there too, which is why
    /// its escalation runs on a timer: `Child::wait` answers instantly
    /// from the cached status at that point and would collapse the
    /// grace to nothing.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_drop_during_the_drain_still_kills_the_group() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("holder-pid");
        let termed_path = dir.path().join("holder-was-termed");

        // The shell exits at once and the holder keeps the pipes, so
        // execute is inside the drain when the timeout below drops it.
        // The holder cleans up on SIGTERM, which is what the guard's
        // grace has to leave room for even here, where the child was
        // reaped long before the drop.
        let command = format!(
            "{{ trap \"echo termed > '{termed}'; exit\" TERM; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             echo shell-done",
            termed = termed_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 30,
                    description: "test drop during the drain".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        assert!(
            dropped.is_err(),
            "the future has to still be draining when it is dropped, \
             otherwise this measures a completed call"
        );
        let pid = read_pid(&pid_path);
        wait_until(
            || !process_is_live(pid),
            "the dropped command's group to be killed",
        )
        .await;
        assert!(
            termed_path.exists(),
            "the holder should have run its SIGTERM handler: an escalation keyed on the \
             already-reaped child would answer instantly and kill it with no grace at all"
        );
    }

    /// When a cancel does reach bash's own arm (the window where the
    /// token fires while a poll is in flight), the group kill cannot
    /// touch a holder in its own session, so the bounded drain is what
    /// returns the turn. The holder survives, which is what says the
    /// wait ended by dropping the read ends.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_cancel_with_a_holder_outside_the_group_is_still_bounded() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("session-leader-pid");

        let command = format!(
            "setsid bash -c \"echo \\$$ > '{pid}'; \
                while [ -d '{dir}' ] && [ \\$SECONDS -lt 30 ]; do sleep 0.05; done\" & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let token = ctx.cancellation.clone();
        let ready = pid_path.clone();
        tokio::spawn(async move {
            wait_until(
                || {
                    std::fs::metadata(&ready)
                        .map(|m| m.len() > 0)
                        .unwrap_or(false)
                },
                "the holder to be up before the cancel: cancelling first would leave \
                 nothing for the drain to be bounded against",
            )
            .await;
            token.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(12),
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test cancel with a holder outside the group".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;

        let pid = read_pid(&pid_path);
        let outcome = result
            .unwrap_or_else(|_| {
                panic!("the cancelled turn was still waiting on a holder no signal reaches")
            })
            .expect("execute");

        assert!(
            process_is_live(pid),
            "the holder is in its own session, so no kill of ours reaches it and the \
             drain's abort is what ended the wait: with it dead this measures the kill"
        );
        assert!(outcome.is_error, "cancellation marks the outcome");
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("Command cancelled"), "wire: {wire:?}");
        assert!(
            wire.contains(&capture_cut_trailer()),
            "the incomplete capture is reported beside the cancel, wire: {wire:?}"
        );
    }

    /// A failure between the spawn and the first await leaks the
    /// command unless the guard is already armed. `spill_dir` is user
    /// configuration, so an unwritable or full one turns every call
    /// into this path, which is the failure this guard exists to
    /// prevent arriving through the back door.
    ///
    /// The error's own text is asserted so the second phase cannot be
    /// satisfied by some unrelated failure. What no assertion here can
    /// hold is that the spill step is still *after* the spawn: moving
    /// it before would keep this test green, and would do so precisely
    /// because the guard is supposed to kill the child faster than it
    /// can leave a trace of its own.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_spill_failure_does_not_leak_the_command() {
        let dir = TempDir::new().expect("create temp dir");
        let allowed = dir.path().join("spill");
        // `SpillState::new` runs `create_dir_all`, which cannot make a
        // directory underneath a regular file.
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").expect("write blocker");
        let refused = blocker.join("spill");

        let ran = dir.path().join("ran");
        let leaked = dir.path().join("leaked");
        let input = |marker: &Path| BashInput {
            command: format!("sleep 1; touch '{}'", marker.display()),
            timeout: 30,
            description: "test spill failure".to_string(),
            run_in_background: false,
        };

        // With a usable spill directory the command reaches its marker,
        // which is what makes the second phase's absence mean
        // something.
        let mut ctx = DummyToolContext::default();
        BashTool::new(false, Some(allowed))
            .execute(&mut ctx, input(&ran))
            .await
            .expect("execute");
        assert!(ran.exists(), "the command writes its marker when it runs");

        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::new(false, Some(refused))
            .execute(&mut ctx, input(&leaked))
            .await;
        let error = outcome
            .err()
            .expect("an unusable spill directory fails the call")
            .to_string();
        assert!(
            error.contains("Not a directory"),
            "the call has to fail on the spill directory, not on something before it: {error}"
        );

        // Past the command's own sleep: if the guard was armed at the
        // spawn the child never got here, and if it was armed after the
        // spill it is still running.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !leaked.exists(),
            "the command outlived the error return that abandoned it"
        );
    }

    /// A command that ignores `SIGTERM` is still killed. Nothing else
    /// pins the escalation on the guard's path: every other teardown
    /// test uses a command that goes away on the first signal.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_command_that_ignores_sigterm_is_killed() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let command = format!(
            "trap '' TERM; echo $$ > '{pid}'; while true; do sleep 0.2; done",
            pid = pid_path.display(),
        );

        let mut ctx = DummyToolContext::default();
        drop_when_ready(
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test sigterm-ignoring command".to_string(),
                    run_in_background: false,
                },
            ),
            &pid_path,
        )
        .await;

        let pid = read_pid(&pid_path);
        assert!(
            process_is_live(pid),
            "the command shrugs off the SIGTERM, otherwise this measures the grace"
        );
        wait_until(
            || !process_is_live(pid),
            "the SIGTERM-ignoring command to be killed after the grace",
        )
        .await;
    }

    /// The teardown drops the pipe read ends, which is one of the two
    /// harms a dropped command leaves behind and the one no other test
    /// looks at.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_command_releases_its_capture_readers() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().expect("spawn");
        let pgid: i32 = child.id().expect("pid").try_into().expect("pid fits");

        // Stand-ins for the capture readers: they never finish on their
        // own, so finishing at all means the teardown aborted them.
        let stdout_reader = tokio::spawn(std::future::pending::<()>());
        let stderr_reader = tokio::spawn(std::future::pending::<()>());
        let registry = TaskRegistry::default();
        let mut guard =
            ProcessGuard::arm(child, registry.track_cleanup()).expect("arm process guard");
        guard.watch_readers([stdout_reader.abort_handle(), stderr_reader.abort_handle()]);

        assert!(
            !stdout_reader.is_finished() && !stderr_reader.is_finished(),
            "the readers have to be running before the drop"
        );
        drop(guard);
        assert!(
            !registry.quiesce(Duration::ZERO).await,
            "the dropped guard transfers its session lease into asynchronous process cleanup"
        );

        wait_until(
            || stdout_reader.is_finished() && stderr_reader.is_finished(),
            "both capture readers to be aborted",
        )
        .await;
        wait_until(
            || !process_is_live(pgid),
            "the dropped command to be killed",
        )
        .await;
        assert!(
            registry.quiesce(Duration::ZERO).await,
            "the session lease ends only after process and reader cleanup"
        );
    }

    /// The tool hands its reader abort handles to the guard, so a
    /// dropped call releases the pipe read ends as well as the process
    /// group. Losing that handoff leaves both reader tasks running on
    /// a pipe nobody will ever close, which is the second harm this
    /// bead names and the one every other test here reaches through a
    /// guard it wired itself.
    ///
    /// The oracle is a straggler in its own session, out of reach of
    /// the group kill, whose next write to the inherited pipe fails
    /// once the host has let go of the read end. It ignores `SIGPIPE`
    /// so the failed write is an error it can report rather than the
    /// signal that would kill it, and it reports into a file rather
    /// than into the pipe under test.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_call_releases_the_pipes_the_tool_opened() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("session-leader-pid");
        let write_failed = dir.path().join("write-failed");

        let command = format!(
            "setsid bash -c \"trap '' PIPE; echo \\$$ > '{pid}'; \
                while [ -d '{dir}' ] && [ \\$SECONDS -lt 30 ]; do \
                    echo tick || {{ echo gone > '{failed}'; exit 0; }}; \
                    sleep 0.05; \
                done\" & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            pid = pid_path.display(),
            dir = dir.path().display(),
            failed = write_failed.display(),
        );

        let mut ctx = DummyToolContext::default();
        drop_when_ready(
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test reader release through the tool".to_string(),
                    run_in_background: false,
                },
            ),
            &pid_path,
        )
        .await;

        let holder = read_pid(&pid_path);
        assert!(
            process_is_live(holder),
            "the holder is in its own session, so no kill of ours reaches it: with it dead \
             this would measure the kill instead of the descriptors"
        );
        wait_until(
            || write_failed.exists(),
            "the straggler's write to fail once the host released the read ends",
        )
        .await;
    }

    /// The grace is the group's, not the leader's. `bash` exits the
    /// instant it takes the `SIGTERM` while the descendants it forked
    /// are still running their handlers, so a teardown that waits on
    /// the child kills exactly the processes the grace was for.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_grace_outlives_a_leader_that_exits_at_once() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("descendant-pid");
        let cleaned_path = dir.path().join("descendant-cleaned-up");

        // The leader goes on the first signal. Its descendant needs
        // half a second to finish cleaning up, far longer than the
        // leader survives and far shorter than the grace.
        let command = format!(
            "{{ trap \"sleep 0.5; echo done > '{cleaned}'; exit 0\" TERM; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             trap 'exit 0' TERM; \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            cleaned = cleaned_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        drop_when_ready(
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test a leader that exits at once".to_string(),
                    run_in_background: false,
                },
            ),
            &pid_path,
        )
        .await;

        let descendant = read_pid(&pid_path);
        wait_until(
            || cleaned_path.exists(),
            "the descendant to finish its SIGTERM handler: a grace that ends when the \
             leader dies takes the handler down with it",
        )
        .await;
        wait_until(
            || !process_is_live(descendant),
            "the descendant to be killed once the grace is over",
        )
        .await;
    }

    /// A drop landing after the leader was reaped still kills the
    /// group. That is the drain-expiry case, where the shell is gone
    /// and a holder of the turn's pipes is still alive: skipping the
    /// escalation there leaves it running, which would make a cancelled
    /// command strictly weaker than a completed one, since the
    /// completion path kills the same holder.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_reaped_leader_still_gets_its_group_killed() {
        let dir = TempDir::new().expect("create temp dir");
        let shell_pid_path = dir.path().join("shell-pid");
        let holder_pid_path = dir.path().join("holder-pid");

        // The holder ignores SIGTERM, so only the escalation can end
        // it. The shell exits as soon as the holder is up, which puts
        // the call in the drain with its leader already reaped.
        let command = format!(
            "echo $$ > '{shell}'; \
             {{ trap '' TERM; echo $BASHPID > '{holder}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{holder}' ]; do sleep 0.01; done; \
             echo shell-done",
            shell = shell_pid_path.display(),
            holder = holder_pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            BashTool::default().execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test a drop with the leader already reaped".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;
        assert!(
            dropped.is_err(),
            "the call has to still be draining when it is dropped"
        );

        let shell = read_pid(&shell_pid_path);
        let holder = read_pid(&holder_pid_path);
        assert!(
            !process_is_live(shell),
            "the shell has to be gone before the drop, otherwise this measures the \
             live-leader path and says nothing about a reaped one"
        );
        assert!(
            process_is_live(holder),
            "the holder has to outlive the SIGTERM it ignores, otherwise the escalation \
             has nothing left to kill"
        );
        wait_until(
            || !process_is_live(holder),
            "the SIGTERM-immune holder of a reaped leader's group to be killed",
        )
        .await;
    }

    /// A guard dropped after its runtime is gone has nothing to spawn
    /// onto, and still signals rather than panicking on a runtime that
    /// is not there.
    ///
    /// This is the guard carried out of the runtime and dropped on a
    /// plain thread. It is NOT the host-exit path: a runtime that is
    /// shutting down still answers `Handle::try_current` with `Ok`, so
    /// what a host exit does with an in-flight command is a separate
    /// question and a separate test.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_guard_dropped_without_a_runtime_still_kills_the_group() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("descendant-pid");

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (guard, pgid, descendant) = runtime.block_on(async {
            let command = format!(
                "{{ echo $BASHPID > '{pid}'; \
                    while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
                 until [ -s '{pid}' ]; do sleep 0.01; done; \
                 sleep 30",
                pid = pid_path.display(),
                dir = dir.path().display(),
            );
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(&command)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            let child = cmd.spawn().expect("spawn");
            let pgid: i32 = child.id().expect("pid").try_into().expect("pid fits");
            // The readers are irrelevant here, only their handles are.
            let readers = [
                tokio::spawn(std::future::pending::<()>()).abort_handle(),
                tokio::spawn(std::future::pending::<()>()).abort_handle(),
            ];
            // Wait for the descendant so the group has a member the
            // immediate child's death would not take with it.
            for _ in 0..400 {
                if std::fs::metadata(&pid_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let descendant = read_pid(&pid_path);
            let mut guard = arm_for_test(child);
            guard.watch_readers(readers);
            (guard, pgid, descendant)
        });

        assert!(
            process_is_live(pgid) && process_is_live(descendant),
            "the command and its descendant should be running before the runtime goes: \
             otherwise this measures nothing"
        );

        // The host is on its way out: no runtime, no async, no reap.
        drop(runtime);
        drop(guard);

        for _ in 0..400 {
            if !process_is_live(pgid) && !process_is_live(descendant) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the group outlived the runtime: leader {pgid}, descendant {descendant}");
    }

    /// The `SIGTERM` lands even when the spawned teardown is never
    /// polled, which is what a host exit does to it: a runtime being
    /// dropped shuts down inside its own context, so the guard's spawn
    /// is answered with a handle whose future is discarded unpolled.
    /// A teardown that owned the first signal would send nothing at
    /// all here and the whole group would outlive the host.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_guard_dropped_by_a_dying_runtime_still_signals() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let termed_path = dir.path().join("shell-was-termed");
        // The trap is installed before the pid appears, so a readable
        // pid also says the command can answer a signal.
        let command = format!(
            "trap \"echo termed > '{termed}'; exit\" TERM; \
             echo $$ > '{pid}'; \
             while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done",
            termed = termed_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );
        let held = GuardHeldByItsRuntime::spawn(command, &pid_path);

        held.drop_the_runtime();

        for _ in 0..400 {
            if termed_path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the command never took a SIGTERM: on this path the only signal is the one Drop \
             sends itself, since the teardown it spawned is discarded unpolled"
        );
    }

    /// The residue of the guarantee above, specified rather than
    /// fixed: escalation belongs to the spawned teardown and dies with
    /// the runtime, so a command that ignores `SIGTERM` outlives the
    /// host that held it. Under a live host the same command is
    /// killed, which is what
    /// `a_dropped_command_that_ignores_sigterm_is_killed` holds.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_term_immune_command_outlives_the_runtime_that_held_it() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let command = format!(
            "trap '' TERM; echo $$ > '{pid}'; \
             while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );
        let held = GuardHeldByItsRuntime::spawn(command, &pid_path);
        let pid = held.pid;

        held.drop_the_runtime();

        // Past the point a live host would have escalated. What
        // survives this survives because there was nothing left to
        // escalate, not because the kill is still on its way.
        std::thread::sleep(KILL_GRACE + Duration::from_millis(500));
        assert!(
            process_is_live(pid),
            "a SIGTERM-immune command was killed after its runtime went: the escalation is \
             a courtesy of the spawned teardown, and a synchronous kill in Drop would stall \
             every host exit by the grace, per live guard"
        );
    }

    /// A command running under an armed guard that only the runtime's
    /// own shutdown can drop, which is the shape a host exit has.
    ///
    /// The command is expected to write its pid to `pid_path` once it
    /// is ready to take a signal, and to end on its own if nobody ever
    /// signals it.
    #[cfg(target_os = "linux")]
    struct GuardHeldByItsRuntime {
        runtime: tokio::runtime::Runtime,
        pid: i32,
        dropped_in_runtime_context: Arc<AtomicBool>,
    }

    #[cfg(target_os = "linux")]
    impl GuardHeldByItsRuntime {
        fn spawn(command: String, pid_path: &Path) -> Self {
            let dropped_in_runtime_context = Arc::new(AtomicBool::new(false));
            let probe = Arc::clone(&dropped_in_runtime_context);
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            // The task never finishes, so nothing but the runtime's
            // shutdown reaches the guard.
            runtime.spawn(async move {
                let mut cmd = Command::new("bash");
                cmd.arg("-c")
                    .arg(&command)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0);
                let child = cmd.spawn().expect("spawn");
                let _guard = arm_for_test(child);
                let _probe = DropContextProbe(probe);
                std::future::pending::<()>().await;
            });

            for _ in 0..400 {
                if std::fs::metadata(pid_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let pid = read_pid(pid_path);
            assert!(
                process_is_live(pid),
                "the command has to be running when the runtime goes, \
                 otherwise this measures nothing"
            );
            Self {
                runtime,
                pid,
                dropped_in_runtime_context,
            }
        }

        /// End the host, and hold the construction to the shape it
        /// claims before the caller reads anything into the result.
        fn drop_the_runtime(self) {
            let context = self.dropped_in_runtime_context;
            drop(self.runtime);
            assert!(
                context.load(atomic::Ordering::SeqCst),
                "the guard was dropped outside the runtime context, which is the plain-thread \
                 case another test already covers: this fixture only measures the host-exit \
                 shape while a shutting-down runtime still answers Handle::try_current with Ok"
            );
        }
    }

    /// Records where its drop landed, so a tokio that stops shutting
    /// down in-context cannot quietly turn the host-exit tests into
    /// the plain-thread case.
    #[cfg(target_os = "linux")]
    struct DropContextProbe(Arc<AtomicBool>);

    #[cfg(target_os = "linux")]
    impl Drop for DropContextProbe {
        fn drop(&mut self) {
            self.0.store(
                tokio::runtime::Handle::try_current().is_ok(),
                atomic::Ordering::SeqCst,
            );
        }
    }

    /// Read a pid a fixture wrote, failing with the shape of the file
    /// rather than a parse error.
    #[cfg(target_os = "linux")]
    fn read_pid(path: &Path) -> i32 {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        raw.trim()
            .parse()
            .unwrap_or_else(|_| panic!("fixture should have recorded a pid, file held {raw:?}"))
    }

    /// Whether `pid` is a live process. Reads `/proc` rather than
    /// signalling: signal 0 also succeeds for a zombie that nobody has
    /// reaped yet, which would read as alive right after a kill.
    #[cfg(target_os = "linux")]
    fn process_is_live(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // The state letter follows the parenthesised comm field, which
        // may itself contain spaces and parentheses.
        let Some((_, after_comm)) = stat.rsplit_once(')') else {
            return false;
        };
        !matches!(after_comm.split_whitespace().next(), Some("Z") | None)
    }

    /// Spin until `cond` holds, bounded, for a state a signal reaches
    /// asynchronously. Yields to the runtime rather than blocking the
    /// thread: a teardown spawned by a dropped guard has to be polled
    /// before it can change anything.
    #[cfg(target_os = "linux")]
    async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Drive a tool call until its fixture reports ready, then drop it,
    /// which is what the driver does to a cancelled turn.
    ///
    /// The handshake is the fixture's own marker rather than a fixed
    /// delay, so a loaded machine cannot drop a command that has not
    /// started yet and leave the test measuring an empty process group.
    /// A call that finishes on its own before the marker appears is a
    /// broken fixture, not a passed test.
    #[cfg(target_os = "linux")]
    async fn drop_when_ready<F: std::future::Future>(future: F, ready: &Path) {
        let mut future = std::pin::pin!(future);
        for _ in 0..400 {
            tokio::select! {
                biased;
                _ = future.as_mut() => {
                    panic!("the call ended before {} appeared, so nothing was dropped", ready.display())
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            if std::fs::metadata(ready)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
            {
                // Dropping the future here is the whole point: it
                // happens on the way out of this scope.
                return;
            }
        }
        panic!("the fixture never reported ready at {}", ready.display());
    }

    /// `emit_update` is invoked at least once during execution; the
    /// snapshot carries the same `command` the caller passed, no
    /// structured truncation summary, and an unset exit code.
    #[tokio::test]
    async fn emit_update_fires_during_execution() {
        let (mut ctx, updates) = RecordingCtx::new();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo hi; sleep 0.3; echo bye".to_string(),
                    timeout: 30,
                    description: "test progress".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let recorded = updates.lock().unwrap();
        assert!(
            !recorded.is_empty(),
            "expected at least one emit_update snapshot"
        );
        for partial in recorded.iter() {
            match partial {
                ToolDetails::Bash {
                    command,
                    exit_code,
                    full_output_path,
                    stdout_truncation,
                    stderr_truncation,
                    ..
                } => {
                    assert_eq!(command, "echo hi; sleep 0.3; echo bye");
                    assert!(exit_code.is_none(), "partial should not carry exit_code");
                    assert!(
                        full_output_path.is_none(),
                        "partial should not carry spill path"
                    );
                    assert!(
                        stdout_truncation.is_none() && stderr_truncation.is_none(),
                        "partial should not carry final truncation summary"
                    );
                }
                other => panic!("expected Bash partial, got {other:?}"),
            }
        }
    }

    /// Spawning a binary that doesn't exist surfaces as a recoverable
    /// error outcome rather than a bubbled `Err`. We model this by
    /// asking bash to run something it can't find — bash itself
    /// succeeds (exit 127), but the error landing in stderr matches
    /// the contract: the model sees a clear failure and can adjust.
    #[tokio::test]
    async fn missing_binary_surfaces_as_normal_failure() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "this-binary-does-not-exist-aj".to_string(),
                    timeout: 30,
                    description: "test missing binary".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        // Bash itself ran; the wrapped command failed (exit 127). We
        // don't mark this `is_error` because bash exited normally.
        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Bash { exit_code, .. } => {
                assert_eq!(*exit_code, Some(127), "expected `command not found`");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// Working directory honored: bash runs in the directory the
    /// `ToolContext` reports.
    #[tokio::test]
    async fn command_runs_in_context_working_directory() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut ctx = DummyToolContext {
            working_directory: dir.path().to_path_buf(),
            ..DummyToolContext::default()
        };
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "pwd".to_string(),
                    timeout: 30,
                    description: "test cwd".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        // On macOS `/tmp` resolves through `/private/tmp`; compare
        // canonicalized paths to avoid that confusion.
        let want = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let got_line = wire.trim();
        let got = std::fs::canonicalize(Path::new(got_line)).unwrap_or_else(|_| got_line.into());
        assert_eq!(got, want, "wire: {wire:?}");
    }

    /// Unit-test the marker formatter against a synthesised summary
    /// to lock in the exact phrasing for all three variants.
    #[test]
    fn stream_marker_phrasings() {
        let path = PathBuf::from("/tmp/aj-bash-xyz.log");
        let lines_only = BashStreamTruncation {
            total_lines: 5000,
            total_bytes: 5000 * 8,
            output_lines: 2000,
            output_bytes: 2000 * 8,
            truncated_by: TruncatedBy::Lines,
            last_line_partial: false,
            last_line_bytes: 0,
        };
        let m = stream_marker("stdout", &lines_only, Some(&path));
        assert_eq!(
            m,
            "[Showing lines 3001-5000 of 5000 of stdout. Full output at /tmp/aj-bash-xyz.log]"
        );

        let bytes_only = BashStreamTruncation {
            total_lines: 60,
            total_bytes: 100 * 1024,
            output_lines: 30,
            output_bytes: 50 * 1024,
            truncated_by: TruncatedBy::Bytes,
            last_line_partial: false,
            last_line_bytes: 0,
        };
        let m = stream_marker("stderr", &bytes_only, Some(&path));
        assert_eq!(
            m,
            "[Showing lines 31-60 of 60 of stderr (50.0KB limit). Full output at /tmp/aj-bash-xyz.log]"
        );

        let partial = BashStreamTruncation {
            total_lines: 1,
            total_bytes: 200 * 1024,
            output_lines: 1,
            output_bytes: 50 * 1024,
            truncated_by: TruncatedBy::Bytes,
            last_line_partial: true,
            last_line_bytes: 200 * 1024,
        };
        let m = stream_marker("stdout", &partial, Some(&path));
        assert_eq!(
            m,
            "[Showing last 50.0KB of stdout line 1 (line is 200.0KB). Full output at /tmp/aj-bash-xyz.log]"
        );
    }

    // ---- Background mode --------------------------------------------------

    /// Execute `command` as a background task on `ctx`, returning the minted
    /// task id, the spill path from the started result, and the guard that
    /// removes the directory holding it.
    ///
    /// A background task's spill is persisted by contract, so pointing the tool
    /// at an owned directory is what keeps the file from outliving the test.
    async fn start_background(
        ctx: &mut DummyToolContext,
        command: &str,
        timeout: u64,
    ) -> (aj_agent::tool::TaskId, PathBuf, TempDir) {
        let spill_dir = TempDir::new().expect("create temp dir");
        let outcome = BashTool::new(false, Some(spill_dir.path().to_path_buf()))
            .execute(
                ctx,
                BashInput {
                    command: command.to_string(),
                    timeout,
                    description: "test background".to_string(),
                    run_in_background: true,
                },
            )
            .await
            .expect("execute");
        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Bash {
                task_id: Some(id),
                full_output_path: Some(path),
                exit_code: None,
                ..
            } => (*id, path.clone(), spill_dir),
            other => panic!("expected started Bash details with task id + spill path: {other:?}"),
        }
    }

    /// Await terminality with a test-level bound so a wedged driver
    /// fails the test instead of hanging it.
    async fn await_terminal(
        registry: &aj_agent::TaskRegistry,
        id: aj_agent::tool::TaskId,
    ) -> aj_agent::tool::TaskStatus {
        tokio::time::timeout(Duration::from_secs(10), registry.wait_terminal(id))
            .await
            .expect("task should reach a terminal status")
            .expect("task id is known")
    }

    /// Poll `cond` until it holds (bounded), yielding to the runtime
    /// so detached drivers and reader tasks make progress.
    async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The started result returns immediately with the task id and
    /// the always-persisted spill path; the real outcome arrives as a
    /// completion notice carrying exit status, tail, and spill path.
    #[tokio::test]
    async fn background_started_result_carries_id_and_spill_path() {
        let mut ctx = DummyToolContext::default();
        let spill_dir = TempDir::new().expect("create temp dir");
        let outcome = BashTool::new(false, Some(spill_dir.path().to_path_buf()))
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo hello; sleep 0.2".to_string(),
                    timeout: 30,
                    description: "test background start".to_string(),
                    run_in_background: true,
                },
            )
            .await
            .expect("execute");
        // Immediacy is proven structurally: the started result has no
        // exit code while the 200ms child is still sleeping.

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            wire.starts_with("Started background task #1: echo hello; sleep 0.2"),
            "wire: {wire:?}"
        );
        assert!(
            wire.contains("notified when it completes"),
            "the started result promises the completion notice: {wire:?}"
        );
        let spill_path = match &outcome.details {
            ToolDetails::Bash {
                task_id: Some(1),
                full_output_path: Some(path),
                exit_code: None,
                truncated: false,
                ..
            } => path.clone(),
            other => panic!("expected started Bash details, got {other:?}"),
        };
        assert!(
            wire.contains(&spill_path.display().to_string()),
            "wire names the spill path: {wire:?}"
        );

        let registry = ctx.task_registry();
        let status = await_terminal(&registry, 1).await;
        assert_eq!(status, aj_agent::tool::TaskStatus::Exited(Some(0)));

        // The spill file holds the full output even though nothing
        // truncated.
        let on_disk = std::fs::read_to_string(&spill_path).expect("spill readable");
        assert_eq!(on_disk, "hello\n");

        // The completion notice carries exit status, tail, and path.
        let notices = registry.drain_notices(aj_agent::events::AgentId::Main);
        assert_eq!(notices.len(), 1);
        let body = &notices[0].body;
        assert!(
            body.starts_with("Background task #1 finished: echo hello; sleep 0.2 — exit code 0"),
            "notice body: {body:?}"
        );
        assert!(body.contains("hello"), "notice body: {body:?}");
        assert!(
            body.contains(&format!("Full output: {}", spill_path.display())),
            "notice body: {body:?}"
        );
    }

    /// The driver's join is the same defect one level down: a straggler
    /// holding the pipes after the task's own process exited would keep
    /// the completion notice from ever rendering and the registry row
    /// from ever settling. Same drain, so the notice arrives, says the
    /// capture was cut short, and still reports the real exit status.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn background_task_with_a_pipe_holder_still_finishes() {
        let dir = TempDir::new().expect("create temp dir");
        let mut ctx = DummyToolContext::default();
        let command = format!(
            "{{ echo descendant-wrote; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             echo shell-done",
            dir = dir.path().display(),
        );

        let (id, _spill_path, _spill_dir) = start_background(&mut ctx, &command, 30).await;
        let registry = ctx.task_registry();
        let status = await_terminal(&registry, id).await;

        // The task's own process succeeded: a cut capture is not a
        // capture failure and must not be reported as one.
        assert_eq!(status, aj_agent::tool::TaskStatus::Exited(Some(0)));
        let notices = registry.drain_notices(aj_agent::events::AgentId::Main);
        assert_eq!(notices.len(), 1);
        let body = &notices[0].body;
        assert!(
            body.contains(&capture_cut_trailer()),
            "notice body: {body:?}"
        );
        assert!(body.contains("shell-done"), "notice body: {body:?}");
    }

    /// The detached driver announces the task on the bus as its
    /// first act: `TaskStart` is emitted and precedes the same
    /// task's `TaskEnd`. Deleting the driver's `started` emit must
    /// fail here.
    #[tokio::test]
    async fn background_driver_emits_task_start_before_task_end() {
        let mut ctx = DummyToolContext::default();
        let events: Arc<StdMutex<Vec<(&'static str, aj_agent::tool::TaskId)>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let _sub = ctx.bus.subscribe(aj_agent::bus::listener_from_sync(
            move |event| match event {
                aj_agent::events::AgentEvent::TaskStart { task_id, .. } => {
                    events_clone.lock().unwrap().push(("start", *task_id));
                }
                aj_agent::events::AgentEvent::TaskEnd { task_id, .. } => {
                    events_clone.lock().unwrap().push(("end", *task_id));
                }
                _ => {}
            },
        ));

        let (id, _spill_path, _spill_dir) = start_background(&mut ctx, "true", 30).await;
        // The registry flips terminal before the `TaskEnd` emit, so
        // wait for the emit itself.
        wait_for(
            || events.lock().unwrap().contains(&("end", id)),
            "TaskEnd on the bus",
        )
        .await;
        assert_eq!(
            *events.lock().unwrap(),
            vec![("start", id), ("end", id)],
            "TaskStart precedes TaskEnd"
        );
    }

    /// `timeout` is ignored in background mode: a command outliving
    /// the configured timeout still runs to completion.
    #[tokio::test]
    async fn background_ignores_timeout() {
        let mut ctx = DummyToolContext::default();
        // A zero-second timeout would kill the foreground path
        // immediately; the background task must run to its natural
        // exit anyway.
        let (id, spill_path, _spill_dir) =
            start_background(&mut ctx, "sleep 0.3; echo done", 0).await;

        let registry = ctx.task_registry();
        let status = await_terminal(&registry, id).await;
        assert_eq!(status, aj_agent::tool::TaskStatus::Exited(Some(0)));
        let on_disk = std::fs::read_to_string(&spill_path).expect("spill readable");
        assert_eq!(on_disk, "done\n");
    }

    /// The spill file is live: it can be read (e.g. via `read_file`
    /// with offset/limit) while the task is still running.
    #[tokio::test]
    async fn background_spill_file_readable_while_running() {
        let mut ctx = DummyToolContext::default();
        let (id, spill_path, _spill_dir) =
            start_background(&mut ctx, "echo first; sleep 30", 30).await;

        let registry = ctx.task_registry();
        let path = spill_path.clone();
        wait_for(
            || {
                std::fs::read_to_string(&path)
                    .map(|s| s.contains("first"))
                    .unwrap_or(false)
            },
            "spill file to carry early output",
        )
        .await;
        assert_eq!(
            registry.status(id),
            Some(aj_agent::tool::TaskStatus::Running),
            "task still running while the spill is readable"
        );

        // Kill via the registry (the picker path) and confirm the
        // driver flips the status to Killed.
        assert!(registry.kill(id));
        let status = await_terminal(&registry, id).await;
        assert_eq!(status, aj_agent::tool::TaskStatus::Killed);
    }
}
