//! `bash` builtin — execute a command in the system shell.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Bash`] on completion, carrying a
//! bounded rolling tail of `stdout` / `stderr`, the process exit code,
//! a `truncated` flag, an optional `full_output_path` pointing at a
//! temp file with the complete (un-truncated) capture, and per-stream
//! [`BashStreamTruncation`] summaries with the line/byte totals
//! renderers use to compose `[Showing lines X-Y of TOTAL ...]`
//! markers. The wire `content` keeps the legacy text shape
//! (`<stdout>` then `STDERR:` then `Command failed with exit code: N`)
//! so the model reads the same transcript it always has, with each
//! affected stream's marker inserted right after its content when
//! truncation occurred.
//!
//! Per `docs/aj-next-plan.md` §1.2 / §1.8 / §6:
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
//!   `keep()` persists it and we surface the resulting path.
//! - **Progress updates.** While the child runs the implementation
//!   self-throttles `ToolContext::emit_update` to one snapshot per
//!   [`UPDATE_DEBOUNCE`] (~10/s) using a leading-edge fire so the first
//!   byte of output lights up the UI without waiting for the next
//!   tick. Live snapshots carry the boolean `truncated` flag but not
//!   the structured per-stream summary (that's only meaningful once
//!   the stream has closed).
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

use aj_agent::tool::{
    BashStreamTruncation, ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

use crate::truncate::{BASH_MAX_BYTES, BASH_MAX_LINES, TruncatedBy, format_size, truncate_tail};

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash). The command will be run in the
working directory of the agent session.

- There are no permissions checks or sandboxing. You are free to run any command
  you consider reasonable and safe.
- Commands have a configurable timeout to prevent hanging (default: 30s).
- Output is truncated to the last 2000 lines or 50KB per stream (whichever
  fires first). When truncated, the full output is saved to a temp file and
  the marker points at it.
- The command is passed to `bash -c`, so pipes, redirects, and shell features work.
- For file search, prefer `rg` (ripgrep) over `grep`/`find` — it's faster and
  respects `.gitignore` by default. Use `read_file` for reading file contents
  rather than `cat`.
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

        let stdout_state = Arc::new(Mutex::new(StreamState::new()));
        let stderr_state = Arc::new(Mutex::new(StreamState::new()));

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

        // Finalize per-stream: apply truncate_tail to the rolling tail
        // (after dropping any leading partial line) and produce the
        // model-facing stdout/stderr strings plus optional structured
        // truncation summaries.
        let (stdout_str, stdout_truncation) = {
            let mut s = stdout_state.lock().unwrap();
            finalize_stream(&mut s)
        };
        let (stderr_str, stderr_truncation) = {
            let mut s = stderr_state.lock().unwrap();
            finalize_stream(&mut s)
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

        let wire = build_wire_content(
            &stdout_str,
            &stderr_str,
            stdout_truncation.as_ref(),
            stderr_truncation.as_ref(),
            &outcome_kind,
            exit_code,
            input.timeout,
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
                stdout_truncation,
                stderr_truncation,
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

/// Drain `reader` into the shared stream state, teeing every byte into
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
                // The spill file always sees every byte; the rolling
                // tail is what gets surfaced to the model and the UI.
                {
                    let mut spill = spill.lock().unwrap();
                    let _ = spill.write_all(chunk);
                }
                {
                    let mut s = state.lock().unwrap();
                    s.append_chunk(chunk);
                }
            }
            // A read error before EOF (rare) — treat it the same as
            // EOF and let the rest of the pipeline finish.
            Err(_) => return,
        }
    }
}

/// Resolve a stream's rolling tail into a (possibly-truncated)
/// display string plus an optional structured truncation summary.
/// When the source overflowed either cap we drop any leading partial
/// line from the rolling tail and then apply [`truncate_tail`] to fit
/// the per-stream byte/line cap exactly.
#[allow(clippy::as_conversions)]
fn finalize_stream(state: &mut StreamState) -> (String, Option<BashStreamTruncation>) {
    let total_lines = state.total_lines();
    let total_bytes = state.total_bytes_seen;

    let tail_decoded = decode_stream_output(std::mem::take(&mut state.tail));

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
) -> String {
    let mut wire = String::new();

    if !stdout.is_empty() {
        wire.push_str(stdout);
    }
    if let Some(t) = stdout_truncation {
        push_marker(&mut wire, &stream_marker("stdout", t, full_output_path));
    }
    if !stderr.is_empty() {
        if !wire.is_empty() && !wire.ends_with('\n') {
            wire.push('\n');
        }
        wire.push_str("STDERR:\n");
        wire.push_str(stderr);
    }
    if let Some(t) = stderr_truncation {
        push_marker(&mut wire, &stream_marker("stderr", t, full_output_path));
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
    wire
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
                stdout_truncation,
                stderr_truncation,
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
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "yes ABCDEFGH | head -c 200000".to_string(),
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
        // One ~120 KB line with no internal newlines, no trailing
        // newline. Exceeds the 50 KB byte cap; line cap is irrelevant
        // (one line total).
        let outcome = BashTool
            .execute(
                &mut ctx,
                BashInput {
                    command: "head -c 120000 /dev/zero | tr '\\0' 'x'".to_string(),
                    timeout: 30,
                    description: "test last_line_partial".to_string(),
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
                ..
            } => {
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
    /// snapshot carries the same `command` the caller passed, no
    /// structured truncation summary, and an unset exit code.
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
}
