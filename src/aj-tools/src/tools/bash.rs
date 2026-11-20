//! `bash` builtin — execute a command in the system shell.
//!
//! Implements [`aj_agent::tool::ToolDefinition`]. Returns a
//! [`ToolOutcome`] whose `details` is [`ToolDetails::Bash`] on completion,
//! carrying a bounded rolling tail of the command's output, the exit code,
//! a `truncated` flag, an optional `full_output_path` pointing at the file
//! with the complete output, and a [`BashStreamTruncation`] summary with
//! the line/byte totals renderers use to compose
//! `[Showing lines X-Y of TOTAL ...]` markers. The wire `content` is the
//! output, its truncation marker when it was truncated, and then
//! `Command failed with exit code: N` or what else ended the command.
//!
//! Every command runs through one runner, [`detached`]: under a shell
//! wrapper in a session of its own, with its output in a file and its exit
//! status in a second one. A foreground call waits for the command and
//! reports its end as the tool result. A background call returns at once,
//! its command is recorded by the session before it starts, and the
//! command's end arrives as a task notification.
//!
//! Output handling:
//!
//! - **One stream.** Stdout and stderr share the output file, in the order
//!   the command wrote them. The tail and the totals are of both and are
//!   reported as stdout.
//! - **Bounded tail.** The model sees at most
//!   [`crate::truncate::BASH_MAX_BYTES`] / [`crate::truncate::BASH_MAX_LINES`].
//!   In memory we keep a rolling window up to [`ROLLING_CAP_BYTES`] (2× the
//!   byte cap) and lazily trim it back whenever it crosses
//!   [`TRIM_TRIGGER_BYTES`] (4× the byte cap). The first time the source
//!   overflows either cap, `truncated` flips to `true`.
//! - **Full output.** The output file is the complete output. A foreground
//!   call names it when the output was truncated and removes it otherwise.
//!   A background task always keeps it.
//! - **Progress updates.** While a foreground command runs the tool emits
//!   `ToolContext::emit_update` snapshots as output arrives, at most one per
//!   [`UPDATE_DEBOUNCE`] (~10/s), with a leading-edge fire so a renderer
//!   shows the running command at once. Live snapshots carry the boolean
//!   `truncated` flag but not the structured summary, which is only
//!   meaningful once the command has ended.
//! - **Cancellation / timeout.** Both stop the command's whole process
//!   group: `SIGTERM`, then `SIGKILL` after [`KILL_GRACE`]. A cancel
//!   mostly arrives as a drop, because the agent drops a cancelled call's
//!   future instead of polling it again. See [`detached`] for what a drop
//!   does.
//! - **The host ending.** A foreground command never outlives the process
//!   that runs it, however that process ends. See [`detached`].
//! - **Parallel execution.** Independent commands may run concurrently.
//!   Callers must sequence commands that depend on or conflict with each other.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::tool::{
    BashStreamTruncation, ExecutionMode, TaskId, TaskStatus, ToolContext, ToolDefinition,
    ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::truncate::{BASH_MAX_BYTES, BASH_MAX_LINES, TruncatedBy, format_size, truncate_tail};

mod detached;
pub use detached::{
    DetachedProcess, SavedBashTask, ended_background_bash, still_running_outcome,
    watch_background_bash,
};

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash). The command will be run in the
working directory of the agent session.

- This tool enforces no permission checks or sandboxing. Obtain any approval
  required for the command's effects before running it.
- Commands have a configurable timeout to prevent hanging (default: 30s). It
  bounds the command itself: a process the command leaves behind keeps
  running, and what it writes after the command ended is not reported.
- Stdout and stderr are reported as one stream, in the order written.
- Output is truncated to the last 2000 lines or 50KB (whichever fires first).
  When truncated, the full output is saved to a file and the marker points
  at it.
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
  — one task, one task notification — instead of polling in the foreground.
- You do not need to wait for a background task. Keep working and the
  task notification reaches you once it finishes. Never wait by sleeping in
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

#[derive(Clone)]
pub struct BashTool {
    /// When true, eligible commands are dispatched through `rtk`
    /// (https://github.com/rtk-ai/rtk) to compress their output before
    /// it reaches the model. See [`Self::rtk_rewrite`].
    rtk: bool,
    /// Where commands write their output and exit files when no task
    /// directory is set. `None` uses the ambient temp directory, which is
    /// what an unset `spill_dir` config means.
    spill_dir: Option<PathBuf>,
    /// Where commands write their output and exit files. `None` uses the
    /// spill directory.
    task_dir: Option<PathBuf>,
}

impl BashTool {
    /// Construct with the given rtk passthrough setting and spill directory.
    pub fn new(rtk: bool, spill_dir: Option<PathBuf>) -> Self {
        Self {
            rtk,
            spill_dir,
            task_dir: None,
        }
    }

    /// Write commands' files to `dir` instead of the spill directory. A
    /// task record can outlive the process, so its files belong
    /// somewhere that is not cleaned behind it.
    pub fn with_task_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.task_dir = dir;
        self
    }

    /// Create the output file of a command. Dropping the handle does not
    /// remove it: whoever reports the command's end decides whether the
    /// file stays as its full output.
    fn create_task_log(&self) -> std::io::Result<(std::fs::File, PathBuf)> {
        let dir = self.task_dir.as_deref().or(self.spill_dir.as_deref());
        let mut builder = tempfile::Builder::new();
        builder.prefix("aj-bash-").suffix(".log");
        let file = match dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                builder.tempfile_in(dir)?
            }
            None => builder.tempfile()?,
        };
        file.keep().map_err(|error| error.error)
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new(false, None)
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
    /// with a task id and the output path, and a task notification
    /// arrives when the task finishes. `timeout` is ignored in
    /// background mode, the task runs until it exits or is stopped.
    #[serde(default)]
    pub run_in_background: bool,
}

/// Machine-facing captured tails, without display markers or UI details.
#[derive(Serialize, JsonSchema)]
struct BashResult<'a> {
    stdout: &'a str,
    stderr: &'a str,
    exit_code: Option<i32>,
    /// True when the captured tail exceeded its display budget.
    truncated: bool,
    full_output_path: Option<&'a Path>,
    /// Present for a background launch. Its output is initially empty.
    task_id: Option<TaskId>,
}

fn default_timeout() -> u64 {
    30
}

impl BashTool {
    /// Ask the optional RTK hook for a rewrite, falling back to the original
    /// command when the hook is unavailable, unsuccessful, or times out.
    /// The hook inherits the host environment. Its answer runs unchanged in
    /// the tool shell, where the session and command control PATH resolution.
    async fn rtk_rewrite(&self, command: &str, working_dir: &Path) -> Option<String> {
        if !self.rtk {
            return None;
        }
        let output = tokio::time::timeout(
            RTK_HOOK_TIMEOUT,
            Command::new("rtk")
                .args(["hook", "check", command])
                .current_dir(working_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let rewritten = String::from_utf8(output.stdout).ok()?;
        let rewritten = rewritten.trim();
        if rewritten.is_empty() || rewritten == command || rewritten.contains('\0') {
            return None;
        }
        Some(rewritten.to_string())
    }
}

impl ToolDefinition for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(schemars::schema_for!(BashResult).to_value())
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let working_dir = ctx.working_directory();
        let command = input.command.clone();

        // Optionally dispatch through `rtk` to compress output. The
        // model-facing `command` stays the original. Only the string
        // handed to `bash -c` is rewritten, so snapshots, the wire
        // trailer, and `ToolDetails::Bash` keep showing what the model
        // asked for.
        let executed = match self.rtk_rewrite(&command, &working_dir).await {
            Some(rewritten) => {
                tracing::debug!(original = %command, rewritten = %rewritten, "rtk passthrough");
                rewritten
            }
            None => command.clone(),
        };

        if input.run_in_background {
            return detached::launch(self, ctx, command, executed, working_dir);
        }
        detached::run(self, ctx, command, executed, working_dir, input.timeout).await
    }
}

/// Give `cmd` the environment every command of the tool runs in.
fn apply_environment(cmd: &mut Command, session_env: std::collections::BTreeMap<String, String>) {
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
}

/// Why a foreground command stopped. Drives both the wire content's
/// trailer and the `is_error` flag.
#[derive(Clone, Copy, Debug)]
enum ChildExit {
    /// The command ran to its end, with the status the shell reports: a
    /// command killed by signal `N` has exit code `128 + N`.
    Exited(i32),
    /// `ToolContext::cancellation` fired during execution.
    Cancelled,
    /// The configured timeout elapsed before the command ended.
    TimedOut,
    /// The command's process is gone and left no exit status.
    Lost,
}

/// Rolling-tail state of a command's output as it has been read so far.
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
    /// Bytes at the start of the source that were never read (see
    /// [`Self::skip`]). When nonzero the line counts are of the part that
    /// was read, and only the byte total is of the whole source.
    skipped_bytes: u64,
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
            skipped_bytes: 0,
        }
    }

    /// Account for the first `bytes` of the source without reading them,
    /// before any chunk is applied. `at_boundary` says whether the last of
    /// them was `\n`, so that what follows starts on a line.
    fn skip(&mut self, bytes: u64, at_boundary: bool) {
        debug_assert_eq!(self.total_bytes_seen, 0);
        self.skipped_bytes = bytes;
        self.total_bytes_seen = bytes;
        self.tail_starts_at_boundary = at_boundary;
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

/// Human-readable terminal-status phrase shared by task notifications
/// and `task_output` / `task_stop` reports.
pub(crate) fn task_status_text(status: TaskStatus) -> String {
    match status {
        TaskStatus::Running => "still running".to_string(),
        TaskStatus::Exited(Some(code)) => format!("exit code {code}"),
        TaskStatus::Exited(None) => "lost".to_string(),
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
/// for the task notification while `task_output` snapshots must keep
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
/// for `emit_update` snapshots while the command is running. The
/// structured truncation summary is intentionally left `None`: it only
/// becomes meaningful once the command has ended and we can run
/// `truncate_tail` on the final rolling tail. The boolean `truncated`
/// flag is updated live so the UI can show a "truncated" badge as soon
/// as the source crosses the cap.
#[allow(clippy::as_conversions)]
fn snapshot_partial(command: &str, state: &Arc<Mutex<StreamState>>) -> ToolDetails {
    let state = state.lock().unwrap();
    let truncated = state.total_lines() > BASH_MAX_LINES as u64
        || state.total_bytes_seen > BASH_MAX_BYTES as u64;
    ToolDetails::Bash {
        command: command.to_string(),
        stdout: decode_stream_output(state.tail.clone()),
        stderr: String::new(),
        exit_code: None,
        truncated,
        full_output_path: None,
        stdout_truncation: None,
        stderr_truncation: None,
        task_id: None,
    }
}

/// Build the wire content the model sees for a foreground command: its
/// output, the truncation marker right after it so the model reads the
/// elision context next to the truncated text, and last what ended the
/// command when that was not exit code 0.
fn build_wire_content(
    output: &str,
    truncation: Option<&BashStreamTruncation>,
    outcome: ChildExit,
    timeout_secs: u64,
    full_output_path: Option<&std::path::Path>,
) -> String {
    let mut wire = render_stream_block(output, "", truncation, None, full_output_path);
    let trailer = match outcome {
        ChildExit::Exited(0) => return wire,
        ChildExit::Exited(code) => format!("Command failed with exit code: {code}"),
        ChildExit::Cancelled => "Command cancelled".to_string(),
        ChildExit::TimedOut => format!("Command timed out after {timeout_secs} seconds"),
        ChildExit::Lost => "Command lost: its process is gone and left no exit status. \
                            It was killed, or the machine restarted."
            .to_string(),
    };
    push_marker(&mut wire, &trailer);
    wire
}

/// Render output plus its truncation markers: the shared
/// body of foreground wire content, background task notifications,
/// and `task_output` reports. A bash command has one stream, passed as
/// stdout.
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
/// `full_output_path` is the command's output file. A missing path falls
/// back to a path-less form so the marker still tells the model what was
/// dropped.
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

/// The truncation marker of a stream whose start was never read, so its
/// lines cannot be counted: `[Showing last <bytes> of <total> of <stream>.
/// Full output at <path>]`.
fn skipped_stream_marker(
    stream: &str,
    t: &BashStreamTruncation,
    full_output_path: &std::path::Path,
) -> String {
    let size = |bytes: u64| format_size(usize::try_from(bytes).unwrap_or(usize::MAX));
    format!(
        "[Showing last {} of {} of {}. Full output at {}]",
        size(t.output_bytes),
        size(t.total_bytes),
        stream,
        full_output_path.display(),
    )
}

/// Build a recoverable-error outcome for the spawn-failure path.
/// Surface the failure both as the wire content (so the model can see
/// what went wrong) and as `stderr` in the structured payload (no
/// command actually ran, so there's no real stdout/stderr split).
fn spawn_error_outcome(command: &str, error: String) -> ToolOutcome {
    ToolOutcome {
        structured_content: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use aj_models::types::UserContent;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{self, AtomicBool};
    use tempfile::TempDir;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    /// A tool that writes its commands' files below `dir`, so a file a
    /// test's command leaves is removed with the test's directory.
    fn tool_in(dir: &TempDir) -> BashTool {
        BashTool::new(false, Some(dir.path().join("output")))
    }

    /// The files commands of [`tool_in`] left in `dir`.
    fn left_in(dir: &TempDir) -> Vec<PathBuf> {
        std::fs::read_dir(dir.path().join("output"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    }

    /// A real Bash process connects at the phase under test and waits for EOF.
    /// Holding the accepted socket prevents completion, and dropping it releases
    /// the command even on assertion failure. Requires Bash network redirections.
    struct BashGate(tokio::net::TcpListener);

    impl BashGate {
        async fn new() -> Self {
            Self(
                tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind Bash gate"),
            )
        }

        fn command(&self) -> String {
            let port = self.0.local_addr().expect("gate address").port();
            format!("IFS= read -r release < /dev/tcp/127.0.0.1/{port} || :")
        }

        async fn ready(&self) -> tokio::net::TcpStream {
            tokio::time::timeout(Duration::from_secs(5), self.0.accept())
                .await
                .expect("Bash did not reach its gate")
                .expect("accept Bash gate")
                .0
        }
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
    #[test]
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits for the 500ms RTK hook timeout"
    )]
    fn rtk_passthrough_runs_the_hook_answer_or_original_command() {
        let bash = host_executable("bash");
        let sleep = host_executable("sleep");
        for case in [
            "rewrite",
            "empty",
            "unchanged",
            "failure",
            "non_utf8",
            "nul",
            "timeout",
            "disabled",
            "missing",
        ] {
            let dir = TempDir::new().expect("fixture directory");
            std::os::unix::fs::symlink(&bash, dir.path().join("bash")).expect("fixture bash");
            if case != "missing" {
                write_executable(
                    &dir.path().join("rtk"),
                    r#"#!/bin/sh
if [ "$1" = result ]; then
    printf rewritten
    exit 0
fi
[ "$1" = hook ] && [ "$2" = check ] && [ "$3" = 'printf original' ] || exit 1
printf called > "$AJ_RTK_TEST_MARKER"
case "$AJ_RTK_TEST_CASE" in
    empty) exit 0 ;;
    unchanged) printf '%s' "$3" ;;
    failure) printf 'rtk result\n'; exit 1 ;;
    non_utf8) printf '\377' ;;
    nul) printf 'rtk result\000' ;;
    timeout) exec "$AJ_RTK_TEST_SLEEP" 10 ;;
    *) printf 'rtk result\n' ;;
esac
"#,
                );
            }
            let marker = dir.path().join("called");
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "tools::bash::tests::rtk_passthrough_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PATH", dir.path())
                .env("AJ_RTK_TEST_CASE", case)
                .env("AJ_RTK_TEST_SLEEP", &sleep)
                .env("AJ_RTK_TEST_MARKER", &marker)
                .current_dir(dir.path())
                .output()
                .expect("run isolated RTK fixture");
            assert!(output.status.success(), "{case}: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
            assert_eq!(
                marker.exists(),
                !matches!(case, "disabled" | "missing"),
                "{case}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "runs with an isolated PATH from the passthrough test"]
    async fn rtk_passthrough_child() {
        let case = std::env::var("AJ_RTK_TEST_CASE").expect("fixture case");
        let mut ctx = DummyToolContext::default();
        let started = Instant::now();
        let outcome = BashTool::new(case != "disabled", Some(ctx.working_directory.clone()))
            .execute(
                &mut ctx,
                BashInput {
                    command: "printf original".to_string(),
                    timeout: 5,
                    description: "RTK passthrough".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute command");
        let ToolDetails::Bash {
            command,
            exit_code,
            stdout,
            stderr,
            ..
        } = outcome.details
        else {
            panic!("expected Bash details")
        };
        assert_eq!(command, "printf original");
        assert_eq!(exit_code, Some(0), "{stderr}");
        assert_eq!(
            stdout,
            if case == "rewrite" {
                "rewritten"
            } else {
                "original"
            }
        );
        if case == "timeout" {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "hook blocked command execution"
            );
        }
    }

    /// Kills a process a test means to outlive its command, when the test
    /// ends, however it ends.
    #[cfg(target_os = "linux")]
    struct Straggler(i32);

    #[cfg(target_os = "linux")]
    impl Drop for Straggler {
        fn drop(&mut self) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
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
        assert_eq!(
            outcome.structured_content,
            Some(serde_json::json!({
                "stdout": "hello\n", "stderr": "", "exit_code": 0,
                "truncated": false, "full_output_path": null, "task_id": null,
            }))
        );
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

    /// Stdout and stderr are one stream in the order written. Nothing is
    /// reported under a `STDERR:` header or in the `stderr` field.
    #[tokio::test]
    async fn stderr_joins_the_output_in_the_order_written() {
        let mut ctx = DummyToolContext::default();
        let outcome = BashTool::default()
            .execute(
                &mut ctx,
                BashInput {
                    command: "echo to-stdout; echo to-stderr 1>&2; echo again".to_string(),
                    timeout: 30,
                    description: "test stderr".to_string(),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert_eq!(wire, "to-stdout\nto-stderr\nagain\n");
        match &outcome.details {
            ToolDetails::Bash { stdout, stderr, .. } => {
                assert_eq!(stdout, "to-stdout\nto-stderr\nagain\n");
                assert!(stderr.is_empty(), "stderr: {stderr:?}");
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
        let spill_dir = TempDir::new().expect("spill directory");
        let outcome = BashTool::new(false, Some(spill_dir.path().to_path_buf()))
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
        let api = outcome.structured_content.as_ref().expect("machine result");
        assert_eq!(api["truncated"], true);
        assert_eq!(api["exit_code"], 0);
        assert_eq!(api["task_id"], serde_json::Value::Null);
        assert_eq!(api["stderr"], "");
        let tail = api["stdout"].as_str().unwrap();
        assert!(tail.len() <= BASH_MAX_BYTES);
        assert!(tail.lines().count() <= BASH_MAX_LINES);
        assert!(!tail.contains("[Showing"));
        let full = std::fs::read(api["full_output_path"].as_str().unwrap()).unwrap();
        assert_eq!(full.len(), 200_000);
        assert!(full.ends_with(tail.as_bytes()));
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
        let gate = BashGate::new().await;

        let start = Instant::now();
        let tool = BashTool::default();
        let call = tool.execute(
            &mut ctx,
            BashInput {
                command: gate.command(),
                timeout: 60,
                description: "test cancellation".to_string(),
                run_in_background: false,
            },
        );
        tokio::pin!(call);
        let _held = tokio::select! {
            outcome = &mut call => panic!("command ended before cancellation: {outcome:?}"),
            held = gate.ready() => held,
        };
        token.cancel();
        let outcome = call.await.expect("execute");
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
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits for the SIGKILL escalation grace"
    )]
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
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits for command timeout and TERM grace"
    )]
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

    /// A command whose shell exits while a descendant still has the output
    /// open does not hold the call open: the call ends when the command
    /// does. The descendant is left running, and the result says so and
    /// names the file it writes to.
    ///
    /// Linux-only: the fixture proves the descendant inherited fd 1 by
    /// reading it back from `/proc`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_descendant_holding_the_output_does_not_hold_the_call_open() {
        /// How long the call may take once the shell has exited. Well
        /// below the descendant's own lifetime, so a run that reaches it
        /// is waiting on the descendant.
        const BOUND: Duration = Duration::from_secs(5);

        let dir = TempDir::new().expect("create temp dir");
        let fd1_path = dir.path().join("descendant-fd1");
        let pid_path = dir.path().join("descendant-pid");

        // The descendant records its fd 1, its pid, and writes to stdout
        // before the shell is allowed to exit. It then holds the output
        // until this test drops `dir`, with `$SECONDS` as a backstop.
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
            tool_in(&dir).execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 30,
                    description: "test descendant holding the output".to_string(),
                    run_in_background: false,
                },
            ),
        )
        .await;
        let _straggler = Straggler(read_pid(&pid_path));

        let outcome = result
            .unwrap_or_else(|_| {
                panic!("the shell exited at once, but the call was still waiting after {BOUND:?}")
            })
            .expect("execute");

        assert!(!outcome.is_error, "the command itself succeeded");
        let ToolDetails::Bash {
            exit_code,
            stdout,
            full_output_path,
            ..
        } = &outcome.details
        else {
            panic!("expected Bash details, got {:?}", outcome.details);
        };
        assert_eq!(*exit_code, Some(0), "the shell exited normally");
        assert!(stdout.contains("shell-done"), "stdout: {stdout:?}");
        assert!(
            stdout.contains("descendant-wrote"),
            "output written before the shell exited is reported, stdout: {stdout:?}"
        );
        let path = full_output_path
            .as_ref()
            .expect("the file a process still writes to stays");
        let inherited = std::fs::read_to_string(&fd1_path).unwrap_or_default();
        assert_eq!(
            Path::new(inherited.trim()),
            path,
            "the descendant should hold the output file: otherwise this test measures nothing"
        );
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("A process this command started is still running")
                && wire.contains(&path.display().to_string()),
            "the model is told where the rest goes, wire: {wire:?}"
        );
        assert!(
            process_is_live(read_pid(&pid_path)),
            "the descendant is left running"
        );
    }

    /// A straggler that redirected its output away is left running as
    /// well. This is what keeps a call's end from killing every
    /// `nohup`-style daemon that behaves.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits through a 300ms survival window"
    )]
    async fn a_straggler_that_let_go_of_the_output_is_left_alone() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("daemon-pid");

        // `exec` drops this subshell's copies of the output before it
        // does anything else. The shell waits for its pid so the test
        // never races the branch.
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
        let outcome = tool_in(&dir)
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
        let _straggler = Straggler(pid);
        // Long enough for a teardown that should never have started to
        // have signalled.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            process_is_live(pid),
            "the straggler must be left running: a test that kills it here measures nothing"
        );
        assert!(
            elapsed < KILL_GRACE,
            "the call ends with the shell, took {elapsed:?}"
        );
        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Bash {
                exit_code, stdout, ..
            } => {
                assert_eq!(*exit_code, Some(0));
                assert!(stdout.contains("shell-done"), "stdout: {stdout:?}");
            }
            other => panic!("expected Bash details, got {other:?}"),
        }
        // Whether a straggler still has the output open is not known, so
        // every one is reported.
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("A process this command started is still running"),
            "wire: {wire:?}"
        );
    }

    /// A process in a session of its own survives the kill of the
    /// command's process group, and does not hold the call open past the
    /// timeout.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits for the command timeout"
    )]
    async fn a_process_outside_the_group_does_not_hold_a_timed_out_call_open() {
        const BOUND: Duration = Duration::from_secs(6);

        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("session-leader-pid");

        // `setsid` leaves the command's process group, keeping the
        // inherited output. The shell then blocks so the run ends on the
        // timeout rather than on its own.
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
            tool_in(&dir).execute(
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
        let _straggler = Straggler(pid);
        let outcome = result
            .unwrap_or_else(|_| {
                panic!(
                    "the command timed out at 1s, but the call was still waiting after {BOUND:?}"
                )
            })
            .expect("execute");

        assert!(
            process_is_live(pid),
            "the holder is outside the group and no signal of ours reaches it"
        );
        assert!(outcome.is_error, "the command timed out");
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Command timed out after 1 seconds"),
            "wire: {wire:?}"
        );
    }

    /// The same for a cancel that reaches bash's own arm, the window
    /// where the token fires while a poll is in flight.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_process_outside_the_group_does_not_hold_a_cancelled_call_open() {
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
                || std::fs::metadata(&ready).is_ok_and(|m| m.len() > 0),
                "the holder to be up before the cancel",
            )
            .await;
            token.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(6),
            tool_in(&dir).execute(
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
        let _straggler = Straggler(pid);
        let outcome = result
            .unwrap_or_else(|_| {
                panic!("the cancelled call was still waiting on a holder no signal reaches")
            })
            .expect("execute");

        assert!(
            process_is_live(pid),
            "the holder is in its own session, so no kill of ours reaches it"
        );
        assert!(outcome.is_error, "cancellation marks the outcome");
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("Command cancelled"), "wire: {wire:?}");
    }

    /// The output directory is user configuration, so an unwritable or
    /// full one turns every call into this path: the call fails, and its
    /// command does not run.
    ///
    /// The error's own text is asserted so the second phase cannot be
    /// satisfied by some unrelated failure.
    #[tokio::test]
    async fn an_unusable_output_directory_fails_the_call_and_runs_nothing() {
        let dir = TempDir::new().expect("create temp dir");
        let allowed = dir.path().join("spill");
        // The tool creates the directory, which cannot be done underneath
        // a regular file.
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").expect("write blocker");
        let refused = blocker.join("spill");

        let ran = dir.path().join("ran");
        let leaked = dir.path().join("leaked");
        let input = |marker: &Path| BashInput {
            command: format!("touch '{}'", marker.display()),
            timeout: 30,
            description: "test unusable output directory".to_string(),
            run_in_background: false,
        };

        // With a usable directory the command reaches its marker, which
        // is what makes the second phase's absence mean something.
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
            .expect("an unusable output directory fails the call")
            .to_string();
        assert!(
            error.contains("Not a directory"),
            "the call has to fail on the directory, not on something else: {error}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!leaked.exists(), "the command ran without its output file");
    }

    /// A dropped call stops its command, which is what a cancel by the
    /// user comes to. The session stays open until the teardown the drop
    /// handed to the runtime is done, and the command's files go with it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_call_stops_its_command_and_removes_its_files() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let command = format!(
            "echo $$ > '{pid}'; while true; do sleep 0.2; done",
            pid = pid_path.display(),
        );

        let mut ctx = DummyToolContext::default();
        let registry = ctx.task_registry();
        drop_when_ready(
            tool_in(&dir).execute(
                &mut ctx,
                BashInput {
                    command,
                    timeout: 60,
                    description: "test dropped call".to_string(),
                    run_in_background: false,
                },
            ),
            &pid_path,
        )
        .await;

        assert!(
            !registry.quiesce(Duration::ZERO).await,
            "the dropped call hands its session lease to the teardown"
        );
        let pid = read_pid(&pid_path);
        wait_until(
            || !process_is_live(pid),
            "the dropped command to be stopped",
        )
        .await;
        assert!(
            registry.quiesce(Duration::from_secs(5)).await,
            "the lease ends with the teardown"
        );
        assert_eq!(left_in(&dir), Vec::<PathBuf>::new());
    }

    /// A dropped command that ignores `SIGTERM` is still killed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[cfg_attr(
        not(feature = "slow-tests"),
        ignore = "slow: waits for the SIGKILL escalation grace"
    )]
    async fn a_dropped_command_that_ignores_sigterm_is_killed() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let command = format!(
            "trap '' TERM; echo $$ > '{pid}'; while true; do sleep 0.2; done",
            pid = pid_path.display(),
        );

        let mut ctx = DummyToolContext::default();
        drop_when_ready(
            tool_in(&dir).execute(
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

    /// The grace is the group's, not the leader's. `bash` exits the
    /// instant it takes the `SIGTERM` while the descendants it forked
    /// are still running their handlers, so a teardown that waits on
    /// the leader kills exactly the processes the grace was for.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_grace_outlives_a_leader_that_exits_at_once() {
        let dir = TempDir::new().expect("create temp dir");
        let gate = BashGate::new().await;
        let leader_path = dir.path().join("leader-pid");
        let pid_path = dir.path().join("descendant-pid");
        let cleaned_path = dir.path().join("descendant-cleaned-up");

        // Hold the descendant's handler until the leader is observed dead.
        // The leader's end must not cut short the group's real TERM grace.
        let command = format!(
            "trap 'exit 0' TERM; echo $$ > '{leader}'; \
             {{ trap \"{gate}; echo done > '{cleaned}'; exit 0\" TERM; \
                echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            cleaned = cleaned_path.display(),
            gate = gate.command(),
            leader = leader_path.display(),
            pid = pid_path.display(),
            dir = dir.path().display(),
        );

        let mut ctx = DummyToolContext::default();
        drop_when_ready(
            tool_in(&dir).execute(
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

        let held = gate.ready().await;
        let leader = read_pid(&leader_path);
        let descendant = read_pid(&pid_path);
        wait_until(|| !process_is_live(leader), "the leader to exit on TERM").await;
        assert!(
            process_is_live(descendant),
            "the handler must outlive its leader"
        );
        assert!(!cleaned_path.exists(), "the handler must still be held");
        drop(held);
        wait_until(
            || cleaned_path.exists(),
            "the descendant to finish its SIGTERM handler: a grace that ends when the \
             leader dies takes the handler down with it",
        )
        .await;
        wait_until(
            || !process_is_live(descendant),
            "the descendant to exit after completing its TERM handler",
        )
        .await;
    }

    /// A call dropped after its runtime is gone has nothing to spawn its
    /// teardown onto, and still signals rather than panicking on a
    /// runtime that is not there.
    ///
    /// This is the call carried out of the runtime and dropped on a plain
    /// thread. It is NOT the host-exit path: a runtime that is shutting
    /// down still answers `Handle::try_current` with `Ok`, so what a host
    /// exit does with an in-flight command is a separate test.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_call_dropped_without_a_runtime_still_signals_the_group() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("descendant-pid");
        let command = format!(
            "{{ echo $BASHPID > '{pid}'; \
                while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done; }} & \
             until [ -s '{pid}' ]; do sleep 0.01; done; \
             sleep 30",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );
        let tool = tool_in(&dir);
        let mut call = Box::pin(async move {
            let mut ctx = DummyToolContext::default();
            let input = BashInput {
                command,
                timeout: 60,
                description: "test drop without a runtime".to_string(),
                run_in_background: false,
            };
            let _ = tool.execute(&mut ctx, input).await;
        });

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        // Wait for the descendant so the group has a member the shell's
        // death would not take with it.
        runtime.block_on(async {
            for _ in 0..400 {
                tokio::select! {
                    biased;
                    () = call.as_mut() => panic!("the call ended before it was dropped"),
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
                if std::fs::metadata(&pid_path).is_ok_and(|m| m.len() > 0) {
                    return;
                }
            }
            panic!("the fixture never reported ready");
        });
        let descendant = read_pid(&pid_path);
        assert!(
            process_is_live(descendant),
            "the command should be running before the runtime goes"
        );

        // The host is on its way out: no runtime, no async, no reap.
        drop(runtime);
        drop(call);

        for _ in 0..400 {
            if !process_is_live(descendant) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the group outlived the dropped call: descendant {descendant}");
    }

    /// A command whose call a dying runtime drops is killed, though it
    /// ignores `SIGTERM`. This is what a host exit does to a call: a
    /// runtime being dropped shuts down inside its own context, so the
    /// drop's spawn is answered with a handle whose future is discarded
    /// unpolled, and the escalation never runs. The lifeline that future
    /// held closes with it, and the wrapper kills the group.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_call_dropped_by_a_dying_runtime_kills_its_command() {
        let dir = TempDir::new().expect("create temp dir");
        let pid_path = dir.path().join("shell-pid");
        let command = format!(
            "trap '' TERM; echo $$ > '{pid}'; \
             while [ -d '{dir}' ] && [ $SECONDS -lt 30 ]; do sleep 0.05; done",
            pid = pid_path.display(),
            dir = dir.path().display(),
        );
        let dropped_in_runtime_context = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&dropped_in_runtime_context);
        let tool = tool_in(&dir);
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        // The call never finishes, so nothing but the runtime's shutdown
        // drops it.
        runtime.spawn(async move {
            let mut ctx = DummyToolContext::default();
            let _probe = DropContextProbe(probe);
            let input = BashInput {
                command,
                timeout: 60,
                description: "test drop by a dying runtime".to_string(),
                run_in_background: false,
            };
            let _ = tool.execute(&mut ctx, input).await;
        });
        for _ in 0..400 {
            if std::fs::metadata(&pid_path).is_ok_and(|m| m.len() > 0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = read_pid(&pid_path);
        assert!(
            process_is_live(pid),
            "the command has to be running when the runtime goes"
        );

        drop(runtime);
        assert!(
            dropped_in_runtime_context.load(atomic::Ordering::SeqCst),
            "the call was dropped outside the runtime context, which is the plain-thread \
             case another test already covers"
        );

        for _ in 0..400 {
            if !process_is_live(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the command outlived the runtime: it ignores the SIGTERM the drop sends, and \
             nothing ran the escalation"
        );
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
                    command: "echo hi; echo bye".to_string(),
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
                    assert_eq!(command, "echo hi; echo bye");
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
    /// task notification carrying exit status, tail, and spill path.
    #[tokio::test]
    async fn background_started_result_carries_id_and_spill_path() {
        let mut ctx = DummyToolContext::default();
        let gate = BashGate::new().await;
        let command = format!("echo hello; {}", gate.command());
        let spill_dir = TempDir::new().expect("create temp dir");
        let outcome = BashTool::new(false, Some(spill_dir.path().to_path_buf()))
            .execute(
                &mut ctx,
                BashInput {
                    command: command.clone(),
                    timeout: 30,
                    description: "test background start".to_string(),
                    run_in_background: true,
                },
            )
            .await
            .expect("execute");
        let held = gate.ready().await;
        // The started result arrives while completion is explicitly held.

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            wire.starts_with(&format!("Started background task #1: {command}")),
            "wire: {wire:?}"
        );
        assert!(
            wire.contains("notified when it completes"),
            "the started result promises the task notification: {wire:?}"
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

        assert_eq!(
            outcome.structured_content,
            Some(serde_json::json!({
                "stdout": "", "stderr": "", "exit_code": null,
                "truncated": false, "full_output_path": spill_path, "task_id": 1,
            }))
        );

        let registry = ctx.task_registry();
        assert_eq!(registry.status(1), Some(TaskStatus::Running));
        assert!(
            registry
                .drain_notices(aj_agent::events::AgentId::Main)
                .is_empty()
        );
        drop(held);
        let status = await_terminal(&registry, 1).await;
        assert_eq!(status, aj_agent::tool::TaskStatus::Exited(Some(0)));

        // The spill file holds the full output even though nothing
        // truncated.
        let on_disk = std::fs::read_to_string(&spill_path).expect("spill readable");
        assert_eq!(on_disk, "hello\n");

        // The task notification carries exit status, tail, and path.
        let notices = registry.drain_notices(aj_agent::events::AgentId::Main);
        assert_eq!(notices.len(), 1);
        let body = &notices[0].body;
        assert!(
            body.starts_with(&format!(
                "Background task #1 finished: {command} — exit code 0"
            )),
            "notice body: {body:?}"
        );
        assert!(body.contains("hello"), "notice body: {body:?}");
        assert!(
            body.contains(&format!("Full output: {}", spill_path.display())),
            "notice body: {body:?}"
        );
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
        let gate = BashGate::new().await;
        // A zero-second timeout would kill the foreground path
        // immediately; the background task must run to its natural
        // exit anyway.
        let command = format!("{}; echo done", gate.command());
        let (id, spill_path, _spill_dir) = start_background(&mut ctx, &command, 0).await;
        let held = gate.ready().await;

        let registry = ctx.task_registry();
        assert_eq!(registry.status(id), Some(TaskStatus::Running));
        drop(held);
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
