//! `bash` builtin — execute a command in the system shell.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Bash`] on completion, carrying a
//! bounded rolling tail of `stdout` / `stderr`, the process exit code,
//! a `truncated` flag, and an optional `full_output_path` pointing at
//! a temp file with the complete (un-truncated) capture. The wire
//! `content` keeps the legacy text shape (`<stdout>` then `STDERR:` then
//! `Command failed with exit code: N`) so the model reads the same
//! transcript it always has.
//!
//! Per `docs/aj-next-plan.md` §1.2 / §1.8 / §6:
//!
//! - **Bounded tail.** Each stream is capped at [`STREAM_CAP_BYTES`]
//!   in memory; bytes beyond that get evicted from the in-memory tail
//!   but still land in the spill file. The first time a stream
//!   overflows, `truncated` flips to `true` and the spill path is
//!   surfaced in the structured payload so the user (and a future TUI)
//!   can open the full transcript on demand.
//! - **Spill file.** A `tempfile::NamedTempFile` is created up-front
//!   with prefix `aj-bash-` and suffix `.log`; both reader tasks tee
//!   into it as bytes flow. If no truncation occurred at completion the
//!   `NamedTempFile` is dropped (cleaning up the file). Otherwise
//!   `keep()` persists it and we surface the resulting path.
//! - **Progress updates.** While the child runs the implementation
//!   self-throttles `ToolContext::emit_update` to one snapshot per
//!   [`UPDATE_DEBOUNCE`] (~10/s) using a leading-edge fire so the first
//!   byte of output lights up the UI without waiting for the next
//!   tick.
//! - **Cancellation / timeout.** The child is launched in a fresh
//!   process group (`process_group(0)`) so we can SIGKILL the entire
//!   tree on cancel or timeout. Plain `Child::kill()` only terminates
//!   the immediate child and leaks any grandchildren the shell forked.
//!   On Unix we shell out to `kill -9 -- -<pgid>`; non-Unix builds fall
//!   back to dropping the child handle.
//! - **`Sequential` execution.** `bash` runs arbitrary commands; the
//!   spec marks it `Sequential` so a batch containing it serializes
//!   around any other in-flight tool calls.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::tool::{ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash). The command will be run in the
working directory of the agent session.

- There are no permissions checks or sandboxing. You are free to run any command
  you consider reasonable and safe.
- Commands have a configurable timeout to prevent hanging (default: 30s).
- Output is truncated to 35000 characters to prevent excessive output.
- The command is passed to `bash -c`, so pipes, redirects, and shell features work.
- For file search, prefer `rg` (ripgrep) over `grep`/`find` — it's faster and
  respects `.gitignore` by default. Use `read_file` for reading file contents
  rather than `cat`.
"#;

/// Maximum bytes preserved per stream in the in-memory tail. Each of
/// `stdout` / `stderr` keeps a rolling window at most this large; once
/// either exceeds the cap the structured payload's `truncated` flag is
/// set and the temp-file spill becomes the source of truth for the
/// full output.
const STREAM_CAP_BYTES: usize = 16 * 1024;

/// Hard upper bound on the textual wire content the model receives.
/// Mirrors the legacy 35 000-char ceiling so existing prompts keep
/// behaving the same way; in practice the per-stream cap above keeps us
/// well under this.
const WIRE_CAP_BYTES: usize = 35_000;

/// Minimum spacing between `emit_update` snapshots. ~10 events per
/// second, with a leading-edge fire so the very first chunk of output
/// reaches a renderer without waiting for the next tick.
const UPDATE_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct BashTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct BashInput {
    /// The command to execute in the shell.
    pub command: String,
    /// Timeout in seconds after which the command will be cancelled (default: 30).
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// A description explaining what the command does and why you want to run it.
    pub description: String,
}

fn default_timeout() -> u64 {
    30
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
    /// captured output (`docs/aj-next-plan.md` §1.3).
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        let working_dir = ctx.working_directory();
        let cancellation = ctx.cancellation();
        let timeout = Duration::from_secs(input.timeout);
        let command = input.command.clone();

        // Build the child. `process_group(0)` makes the child the
        // leader of a new process group so `kill -9 -- -<pgid>` reaches
        // every descendant the shell may have spawned (a `Child::kill`
        // alone only signals the immediate child).
        //
        // `stdin: null` keeps any command that reads from stdin from
        // hanging on the agent's terminal — the child gets EOF
        // immediately rather than waiting for input that will never
        // come.
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Ok(spawn_error_outcome(
                    &command,
                    format!("Failed to start command '{}': {}", command, e),
                ));
            }
        };

        // `child.id()` is `Some` between spawn and the eventual `wait`
        // that reaps the zombie, so this never trips in practice.
        let child_pid: i32 = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("child PID unavailable after spawn"))?
            .try_into()
            .map_err(|e| anyhow::anyhow!("child PID does not fit in i32: {e}"))?;
        let stdout = child.stdout.take().expect("stdout was piped above");
        let stderr = child.stderr.take().expect("stderr was piped above");

        // The spill file is created eagerly so both reader tasks can
        // tee into it without coordinating creation. If no truncation
        // happens we drop the `NamedTempFile` at the end and the file
        // gets unlinked; if truncation does happen we `keep()` it and
        // surface the path through the structured payload.
        let spill = Arc::new(Mutex::new(SpillState::new()?));

        let stdout_state = Arc::new(Mutex::new(StreamState::default()));
        let stderr_state = Arc::new(Mutex::new(StreamState::default()));

        let stdout_reader = tokio::spawn(read_stream(
            stdout,
            Arc::clone(&stdout_state),
            Arc::clone(&spill),
        ));
        let stderr_reader = tokio::spawn(read_stream(
            stderr,
            Arc::clone(&stderr_state),
            Arc::clone(&spill),
        ));

        let timeout_at = Instant::now() + timeout;
        // `last_update - UPDATE_DEBOUNCE` triggers a leading-edge fire
        // on the first iteration, so a renderer can show the running
        // command label as soon as we enter the loop.
        let mut last_update = Instant::now() - UPDATE_DEBOUNCE;

        let outcome_kind = loop {
            // Leading-edge debounce: emit a snapshot whenever the
            // backoff window has elapsed since the last emission.
            let now = Instant::now();
            if now.duration_since(last_update) >= UPDATE_DEBOUNCE {
                let snapshot = snapshot_partial(&command, &stdout_state, &stderr_state);
                ctx.emit_update(snapshot);
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
                res = child.wait() => {
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
        if matches!(outcome_kind, ChildExit::Cancelled | ChildExit::TimedOut) {
            kill_process_group(child_pid);
            // Reap the zombie so `child` doesn't outlive the function.
            let _ = child.wait().await;
        }

        // The reader tasks exit when their pipe closes (which happens
        // when the child terminates). Awaiting them ensures every
        // captured byte is in `stdout_state` / `stderr_state` before
        // we build the outcome.
        let _ = stdout_reader.await;
        let _ = stderr_reader.await;

        // Pull the captured bytes out of the shared state.
        let stdout_data = std::mem::take(&mut stdout_state.lock().unwrap().tail);
        let stderr_data = std::mem::take(&mut stderr_state.lock().unwrap().tail);
        let stdout_truncated = stdout_state.lock().unwrap().truncated;
        let stderr_truncated = stderr_state.lock().unwrap().truncated;
        let truncated = stdout_truncated || stderr_truncated;
        let stdout_str = decode_stream_output(stdout_data);
        let stderr_str = decode_stream_output(stderr_data);

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

        let wire = build_wire_content(
            &stdout_str,
            &stderr_str,
            &outcome_kind,
            exit_code,
            input.timeout,
            truncated,
            full_output_path.as_deref(),
        );

        // Cancellation and timeout are exceptional outcomes the model
        // should know to recover from; a non-zero exit code from a
        // command that ran to completion is a normal "the command
        // failed" signal that the wire content already conveys.
        let is_error = matches!(outcome_kind, ChildExit::Cancelled | ChildExit::TimedOut);

        Ok(ToolOutcome {
            content: vec![UserContent::text(wire)],
            details: ToolDetails::Bash {
                command,
                stdout: stdout_str,
                stderr: stderr_str,
                exit_code,
                truncated,
                full_output_path,
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
#[derive(Default)]
struct StreamState {
    /// Bytes captured so far, capped at [`STREAM_CAP_BYTES`].
    tail: Vec<u8>,
    /// True iff at least one byte has been evicted from the front of
    /// `tail` because the cap was exceeded.
    truncated: bool,
}

/// Spill-file state: a `NamedTempFile` we tee both streams into. When
/// truncation occurs we hand its path to the caller; otherwise dropping
/// `Self` unlinks the file.
struct SpillState {
    file: Option<NamedTempFile>,
}

impl SpillState {
    fn new() -> std::io::Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("aj-bash-")
            .suffix(".log")
            .tempfile()?;
        Ok(Self { file: Some(file) })
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(f) = self.file.as_mut() {
            f.as_file_mut().write_all(bytes)?;
        }
        Ok(())
    }

    /// Persist the spill file at its current path, returning that path
    /// for the caller to surface in the structured payload. After this
    /// call the file is no longer cleaned up on drop.
    fn persist(&mut self) -> std::io::Result<PathBuf> {
        let f = self
            .file
            .take()
            .expect("spill file already persisted or discarded");
        let (_, path) = f.keep()?;
        Ok(path)
    }
}

/// Drain `reader` into the shared tail buffer, teeing every byte into
/// the spill file as it arrives. Terminates when the pipe closes.
async fn read_stream<R>(
    mut reader: R,
    state: Arc<Mutex<StreamState>>,
    spill: Arc<Mutex<SpillState>>,
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
                // The spill file always sees every byte; the tail
                // buffer is what gets surfaced to the model and the UI.
                {
                    let mut spill = spill.lock().unwrap();
                    let _ = spill.write_all(chunk);
                }
                {
                    let mut s = state.lock().unwrap();
                    s.tail.extend_from_slice(chunk);
                    if s.tail.len() > STREAM_CAP_BYTES {
                        let drop_n = s.tail.len() - STREAM_CAP_BYTES;
                        s.tail.drain(..drop_n);
                        s.truncated = true;
                    }
                }
            }
            // A read error before EOF (rare) — treat it the same as
            // EOF and let the rest of the pipeline finish.
            Err(_) => return,
        }
    }
}

/// Build a [`ToolDetails::Bash`] partial from the in-flight state. Used
/// for `emit_update` snapshots while the child is running. `exit_code`,
/// `truncated`, and `full_output_path` are filled in only on the final
/// outcome.
fn snapshot_partial(
    command: &str,
    stdout_state: &Arc<Mutex<StreamState>>,
    stderr_state: &Arc<Mutex<StreamState>>,
) -> ToolDetails {
    let stdout_data = stdout_state.lock().unwrap().tail.clone();
    let stderr_data = stderr_state.lock().unwrap().tail.clone();
    let truncated =
        stdout_state.lock().unwrap().truncated || stderr_state.lock().unwrap().truncated;
    ToolDetails::Bash {
        command: command.to_string(),
        stdout: decode_stream_output(stdout_data),
        stderr: decode_stream_output(stderr_data),
        exit_code: None,
        truncated,
        full_output_path: None,
    }
}

/// Build the wire content the model sees. Mirrors the legacy text
/// format: stdout, then a `STDERR:` block, then exit-status / cancel /
/// timeout / truncation trailers.
fn build_wire_content(
    stdout: &str,
    stderr: &str,
    outcome: &ChildExit,
    exit_code: Option<i32>,
    timeout_secs: u64,
    truncated: bool,
    full_output_path: Option<&std::path::Path>,
) -> String {
    let mut wire = String::new();
    if !stdout.is_empty() {
        wire.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !wire.is_empty() && !wire.ends_with('\n') {
            wire.push('\n');
        }
        wire.push_str("STDERR:\n");
        wire.push_str(stderr);
    }
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
    if truncated {
        if !wire.is_empty() && !wire.ends_with('\n') {
            wire.push('\n');
        }
        if let Some(path) = full_output_path {
            wire.push_str(&format!(
                "[Output truncated; full output at {}]",
                path.display()
            ));
        } else {
            wire.push_str("[Output truncated]");
        }
    }
    // Final-mile cap to honor the legacy 35 000-char wire ceiling. In
    // practice the per-stream caps keep us comfortably under this.
    if wire.len() > WIRE_CAP_BYTES {
        wire.truncate(WIRE_CAP_BYTES);
        wire.push_str("\n[Wire content capped]");
    }
    wire
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
    crate::sanitize::sanitize_terminal_output(&lossy)
}

/// SIGKILL the entire process group rooted at `pid`. Negative argument
/// to `kill(2)` (and the `kill` shell command) targets a process group
/// rather than a single process — combined with `process_group(0)` at
/// spawn time this terminates the immediate child plus every
/// descendant the shell forked.
#[cfg(unix)]
fn kill_process_group(pid: i32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", pid))
        .status();
}

#[cfg(not(unix))]
fn kill_process_group(_pid: i32) {
    // Process-group semantics are Unix-only; on other platforms we
    // rely on the `Child` drop / explicit `kill` to terminate the
    // immediate child. Grandchild leakage is a known limitation.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use aj_models::types::UserContent;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use tokio_util::sync::CancellationToken;

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
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<aj_agent::tool::SpawnedAgent>>
                    + Send
                    + 'a,
            >,
        > {
            self.inner.spawn_agent(task)
        }

        fn emit_update(&mut self, partial: ToolDetails) {
            self.updates.lock().unwrap().push(partial);
        }

        fn cancellation(&self) -> CancellationToken {
            self.inner.cancellation.clone()
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
    /// call (`docs/aj-next-plan.md` §1.3).
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(BashTool.execution_mode(), ExecutionMode::Sequential);
    }

    /// Successful command. Wire content carries stdout verbatim;
    /// structured details report exit 0 with no truncation and no
    /// spill file.
    #[tokio::test]
    async fn echo_returns_stdout_and_exit_zero() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo hello".to_string(),
                    timeout: 30,
                    description: "test echo".to_string(),
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
            } => {
                assert_eq!(command, "echo hello");
                assert_eq!(stdout, "hello\n");
                assert!(stderr.is_empty(), "stderr: {stderr:?}");
                assert_eq!(*exit_code, Some(0));
                assert!(!*truncated);
                assert!(full_output_path.is_none());
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
    }

    /// Non-zero exit code surfaces in both the wire content (the
    /// legacy "Command failed with exit code: N" line) and the
    /// structured payload's `exit_code`. Per `docs/aj-next-plan.md` we
    /// don't mark it as `is_error` — the wire content already carries
    /// the failure for the model.
    #[tokio::test]
    async fn nonzero_exit_code_is_not_marked_as_error() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo fail; exit 7".to_string(),
                    timeout: 30,
                    description: "test failing exit".to_string(),
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
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo to-stdout; echo to-stderr 1>&2".to_string(),
                    timeout: 30,
                    description: "test stderr".to_string(),
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
    /// `truncated = true` and `full_output_path` is populated. The wire
    /// content also picks up the truncation marker pointing at the
    /// spill path.
    #[tokio::test]
    async fn large_output_truncates_and_spills_to_temp_file() {
        let mut ctx = DummyToolContext::default();
        // Print enough bytes to overflow the 16 KiB per-stream cap.
        // `yes` would be unbounded; bound it with `head -c` so the
        // command terminates naturally.
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "yes ABCDEFGH | head -c 50000".to_string(),
                    timeout: 30,
                    description: "test truncation".to_string(),
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
                ..
            } => {
                assert!(*truncated, "expected truncation");
                assert_eq!(
                    stdout.len(),
                    STREAM_CAP_BYTES,
                    "in-memory tail should be capped"
                );
                let path = full_output_path.as_ref().expect("spill path on truncation");
                let on_disk = std::fs::read_to_string(path).expect("read spill");
                assert!(
                    on_disk.len() >= 50_000,
                    "spill should hold the full output, got {} bytes",
                    on_disk.len()
                );
                // Clean up the persisted spill file.
                let _ = std::fs::remove_file(path);
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("[Output truncated"),
            "wire should mention truncation: {wire:?}"
        );
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
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "sleep 30".to_string(),
                    timeout: 60,
                    description: "test cancellation".to_string(),
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

    /// Timeout kills the process and surfaces an `is_error: true`
    /// outcome with no exit code and a timeout marker in the wire
    /// content.
    #[tokio::test]
    async fn timeout_kills_command_and_marks_error() {
        let mut ctx = DummyToolContext::default();
        let start = Instant::now();
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "sleep 30".to_string(),
                    timeout: 1,
                    description: "test timeout".to_string(),
                },
            )
            .await
            .expect("execute");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "timeout should not exceed 5s, took {elapsed:?}"
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
    }

    /// `emit_update` is invoked at least once during execution; the
    /// snapshot carries the same `command` the caller passed.
    #[tokio::test]
    async fn emit_update_fires_during_execution() {
        let (mut ctx, updates) = RecordingCtx::new();
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo hi; sleep 0.3; echo bye".to_string(),
                    timeout: 30,
                    description: "test progress".to_string(),
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
                    ..
                } => {
                    assert_eq!(command, "echo hi; sleep 0.3; echo bye");
                    // Partial snapshots leave exit_code and the spill
                    // path unset — those only get filled in on the
                    // final outcome.
                    assert!(exit_code.is_none(), "partial should not carry exit_code");
                    assert!(
                        full_output_path.is_none(),
                        "partial should not carry spill path"
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
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "this-binary-does-not-exist-aj".to_string(),
                    timeout: 30,
                    description: "test missing binary".to_string(),
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
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "pwd".to_string(),
                    timeout: 30,
                    description: "test cwd".to_string(),
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
}
