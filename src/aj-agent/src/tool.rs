//! Tool contract: trait, context, outcome, and structured rendering details.
//!
//! Tools are the agent's external effectors. Each implementation supplies a
//! [`ToolDefinition`] describing its name, schema, and execution; the agent
//! drives them through a [`ToolContext`] reference and folds the resulting
//! [`ToolOutcome`] into both the wire transcript and the typed event
//! stream.

use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aj_models::types::UserContent;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio_util::sync::CancellationToken;

use crate::TaskRegistry;
use crate::bus::EventBus;
use crate::error::BoxError;
use crate::events::{AgentEvent, AgentId, SubAgentConclusion};

// ---------------------------------------------------------------------------
// Execution mode
// ---------------------------------------------------------------------------

/// Per-tool execution mode.
///
/// The agent partitions a single assistant turn's tool calls into
/// contiguous groups: a maximal run of [`ExecutionMode::Parallel`]
/// tools forms one group that runs concurrently, while each
/// [`ExecutionMode::Sequential`] tool is its own singleton group and
/// acts as an ordering barrier. Groups run one at a time in source
/// order and results are recorded in source order regardless of which
/// calls finish first. Tools that mutate the filesystem or run
/// arbitrary commands opt into [`ExecutionMode::Sequential`] so they
/// never interleave with their neighbours.
///
/// Default: [`ExecutionMode::Parallel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Runs alone, as a barrier: it never overlaps the calls before or
    /// after it in the batch.
    Sequential,
    /// Runs concurrently with the adjacent run of `Parallel` calls.
    Parallel,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Parallel
    }
}

// ---------------------------------------------------------------------------
// Tool details — closed enum keyed by rendering shape
// ---------------------------------------------------------------------------

/// Structured payload describing a tool result for UI rendering and
/// persistence.
///
/// `ToolDetails` is intentionally a closed enum with one variant per
/// **rendering shape**, not per tool. Multiple tools may map onto the
/// same variant (e.g. `read_file` and `agent` both render as
/// [`ToolDetails::Text`]). The closed shape makes persistence cheap
/// (round-trips through JSONL without per-tool deserialization) and
/// keeps event listeners decoupled from concrete tool implementations.
///
/// New tools whose rendering doesn't fit any variant fall back to
/// [`ToolDetails::Json`]; new dedicated variants are added when a
/// rendering pattern repeats.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDetails {
    /// Default rendering: a one-line summary plus optional body.
    /// Used by `read_file`, plain `agent` text replies, and anything
    /// without a dedicated variant.
    Text {
        /// Short headline displayed in collapsed views.
        summary: String,
        /// Full body shown when expanded. May be empty.
        body: String,
    },
    /// File-edit rendering with canonical display lines.
    Diff(DiffDetails),
    /// Command-output rendering for `bash`.
    ///
    /// Cross-field invariant: `stdout`/`stderr` carry a bounded rolling
    /// tail. If either stream exceeded the cap the implementation
    /// spills the full output to a temp file and surfaces its path
    /// through `full_output_path`. Absent truncation fields on older
    /// persisted sessions fall back to the legacy `[Output truncated]`
    /// marker. The per-field docs carry each payload's contract.
    Bash {
        /// The exact command line executed.
        command: String,
        /// Captured (and possibly truncated) standard output.
        stdout: String,
        /// Captured (and possibly truncated) standard error.
        stderr: String,
        /// Process exit code, when the process exited normally.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// True when at least one of `stdout` / `stderr` was truncated.
        #[serde(default)]
        truncated: bool,
        /// Path to a temp file containing the full uncaptured output,
        /// when truncation occurred.
        #[serde(skip_serializing_if = "Option::is_none")]
        full_output_path: Option<PathBuf>,
        /// Structured truncation summary for `stdout` when that stream
        /// exceeded the cap. Renderers and the wire-content footer
        /// consume this to build informative markers. Default `None`
        /// keeps the serialized form stable for sessions captured
        /// before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout_truncation: Option<BashStreamTruncation>,
        /// Structured truncation summary for `stderr`. Same contract as
        /// `stdout_truncation`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stderr_truncation: Option<BashStreamTruncation>,
        /// Background-task id when this call launched the command as a
        /// background task; `None` for foreground runs. Renderers use
        /// it to badge the cell and correlate task events. Default
        /// `None` keeps the serialized form stable for sessions
        /// captured before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
    },
    /// Sub-agent run report — emitted by the `agent` tool when it
    /// runs as a child agent and returns a final report.
    SubAgentReport {
        /// The `n` from this sub-agent's [`AgentId::Sub`] id.
        agent_id: usize,
        /// Task description supplied by the parent.
        task: String,
        /// Final assistant text returned by the sub-agent.
        report: String,
    },
    /// Todo-list rendering for `todo_read` / `todo_write`. The
    /// implementations also emit a [`ToolDetails::Text`] summary for
    /// the LLM; this variant is the structured snapshot the UI
    /// renders.
    Todos {
        /// Snapshot of the current todo list.
        items: Vec<TodoItem>,
    },
    /// Image rendering for tools that return image content (today:
    /// `read_file` on image MIME types). The image bytes themselves
    /// travel in the [`ToolOutcome::content`] vec as a
    /// [`aj_models::types::UserContent::Image`]; this struct is the
    /// display-side metadata the TUI uses to size the inline render
    /// or compose a textual fallback.
    Image {
        /// Short headline displayed in collapsed views — typically the
        /// display path of the image file.
        summary: String,
        /// MIME type of the (possibly resized) image actually attached
        /// to the tool result. Always one of `image/png`, `image/jpeg`,
        /// `image/gif`, `image/webp`.
        mime_type: String,
        /// Original image dimensions in pixels (width, height) before
        /// any resize.
        original_dimensions: (u32, u32),
        /// Final image dimensions actually carried in the tool result
        /// (width, height). Equal to `original_dimensions` when no
        /// resize occurred.
        displayed_dimensions: (u32, u32),
    },
    /// Escape hatch for tools that don't warrant their own variant.
    /// New tools start here and graduate to a dedicated variant once
    /// a rendering pattern repeats.
    Json(Value),
}

const DIFF_CONTEXT: usize = 3;

// Myers can take quadratic time for broad rewrites. The synchronous file tools
// build details before writing, so this bounds their diff work. On expiry,
// `similar` returns a coarse delete-and-insert approximation.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

/// Canonical compact display diff stored in [`ToolDetails::Diff`].
///
/// The payload stores only display lines, not either source snapshot. A complete
/// rewrite of very short lines can therefore exceed the snapshots' combined
/// size due to the display prefixes. Small edits in large files remain bounded
/// by their context windows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct DiffDetails {
    format: DiffFormat,
    path: String,
    lines: Vec<DiffLine>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    missing_newline: Vec<u32>,
    #[serde(skip)]
    content_fingerprint: u64,
}

impl DiffDetails {
    /// Builds the canonical display diff and discards the input snapshots.
    pub fn new(path: impl AsRef<str>, before: impl AsRef<str>, after: impl AsRef<str>) -> Self {
        Self::new_with_config(path, before, after, |config| {
            config.timeout(DIFF_TIMEOUT);
        })
    }

    fn new_with_config(
        path: impl AsRef<str>,
        before: impl AsRef<str>,
        after: impl AsRef<str>,
        configure: impl FnOnce(&mut similar::TextDiffConfig),
    ) -> Self {
        let path = sanitize_diff_path(path.as_ref());
        let before = crate::sanitize_terminal_output(before.as_ref());
        let after = crate::sanitize_terminal_output(after.as_ref());
        let mut lines = Vec::new();
        let mut missing_newline = Vec::new();

        if !before.is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Header, format!("--- a/{path}")));
        }
        if !after.is_empty() {
            lines.push(DiffLine::new(DiffLineKind::Header, format!("+++ b/{path}")));
        }

        let mut config = TextDiff::configure();
        configure(&mut config);
        let diff = config.diff_lines(&before, &after);
        let tags: Vec<ChangeTag> = diff.iter_all_changes().map(|change| change.tag()).collect();
        let mut last_emitted_idx = None;

        for (idx, change) in diff.iter_all_changes().enumerate() {
            if matches!(change.tag(), ChangeTag::Equal)
                && !is_diff_context(&tags, idx, DIFF_CONTEXT)
            {
                continue;
            }

            if last_emitted_idx.is_some_and(|last| idx > last + 1) {
                lines.push(DiffLine::new(DiffLineKind::Separator, "…".to_string()));
            }
            last_emitted_idx = Some(idx);

            let value = change.value().trim_end_matches('\n');
            let (kind, text) = match change.tag() {
                ChangeTag::Delete => (DiffLineKind::Remove, format!("- {value}")),
                ChangeTag::Insert => (DiffLineKind::Add, format!("+ {value}")),
                ChangeTag::Equal => (DiffLineKind::Context, format!("  {value}")),
            };
            if change.missing_newline() {
                let line_index = u32::try_from(lines.len())
                    .expect("a display diff cannot contain more than u32::MAX lines");
                missing_newline.push(line_index);
            }
            lines.push(DiffLine::new(kind, text));
        }

        let content_fingerprint = diff_content_fingerprint(&path, &lines, &missing_newline);
        Self {
            format: DiffFormat::DisplayV1,
            path,
            lines,
            missing_newline,
            content_fingerprint,
        }
    }

    /// Returns the sanitized display path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the canonical semantic display lines.
    pub fn lines(&self) -> &[DiffLine] {
        &self.lines
    }

    /// Returns indexes of display lines whose source lacked a final newline.
    pub fn missing_newline_indexes(&self) -> &[u32] {
        &self.missing_newline
    }

    /// Returns the precomputed fingerprint of immutable display content.
    pub fn content_fingerprint(&self) -> u64 {
        self.content_fingerprint
    }

    fn from_compact(raw: CompactDiffDetails) -> Result<Self, String> {
        let path = sanitize_diff_path(&raw.path);
        let lines = raw
            .lines
            .into_iter()
            .map(|text| {
                if text.contains('\r') || text.contains('\n') {
                    return Err("canonical diff lines must contain exactly one line".to_string());
                }
                let text = crate::sanitize_terminal_output(&text);
                let kind = diff_line_kind(&path, &text)
                    .ok_or_else(|| format!("invalid canonical diff line: {text:?}"))?;
                Ok(DiffLine::new(kind, text))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let mut previous = None;
        for &index in &raw.missing_newline {
            let line_index = usize::try_from(index)
                .map_err(|_| format!("missing-newline index {index} is out of bounds"))?;
            let Some(line) = lines.get(line_index) else {
                return Err(format!("missing-newline index {index} is out of bounds"));
            };
            if previous.is_some_and(|previous| index <= previous) {
                return Err("missing-newline indexes must be strictly increasing".to_string());
            }
            if matches!(line.kind, DiffLineKind::Header | DiffLineKind::Separator) {
                return Err(format!(
                    "missing-newline index {index} does not refer to a content line"
                ));
            }
            previous = Some(index);
        }

        let content_fingerprint = diff_content_fingerprint(&path, &lines, &raw.missing_newline);
        Ok(Self {
            format: raw.format,
            path,
            lines,
            missing_newline: raw.missing_newline,
            content_fingerprint,
        })
    }
}

impl<'de> Deserialize<'de> for DiffDetails {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::from_compact(CompactDiffDetails::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Semantic role of one canonical diff line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffLineKind {
    /// A file header.
    Header,
    /// An inserted line.
    Add,
    /// A removed line.
    Remove,
    /// An unchanged context line.
    Context,
    /// A separator between non-adjacent context windows.
    Separator,
}

/// One prefixed display line and its semantic role.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffLine {
    kind: DiffLineKind,
    text: String,
}

impl DiffLine {
    fn new(kind: DiffLineKind, text: String) -> Self {
        Self { kind, text }
    }

    /// Returns the line's semantic role.
    pub fn kind(&self) -> DiffLineKind {
        self.kind
    }

    /// Returns the canonical prefixed line text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Serialize for DiffLine {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum DiffFormat {
    #[serde(rename = "display-v1")]
    DisplayV1,
}

#[derive(Deserialize)]
struct CompactDiffDetails {
    format: DiffFormat,
    path: String,
    lines: Vec<String>,
    #[serde(default)]
    missing_newline: Vec<u32>,
}

#[derive(Deserialize)]
struct LegacyDiffDetails {
    path: String,
    before: String,
    after: String,
}

enum DiffDetailsWire {
    Compact(DiffDetails),
    Legacy(LegacyDiffDetails),
}

impl<'de> Deserialize<'de> for DiffDetailsWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        if raw.get("format").is_some() || raw.get("lines").is_some() {
            serde_json::from_value(raw)
                .map(Self::Compact)
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value(raw)
                .map(Self::Legacy)
                .map_err(D::Error::custom)
        }
    }
}

fn sanitize_diff_path(path: &str) -> String {
    crate::sanitize_terminal_output(path).replace('\n', "\\n")
}

fn diff_content_fingerprint(path: &str, lines: &[DiffLine], missing_newline: &[u32]) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    lines.hash(&mut hasher);
    missing_newline.hash(&mut hasher);
    hasher.finish()
}

fn diff_line_kind(path: &str, text: &str) -> Option<DiffLineKind> {
    if text == format!("--- a/{path}") || text == format!("+++ b/{path}") {
        Some(DiffLineKind::Header)
    } else if text == "…" {
        Some(DiffLineKind::Separator)
    } else if text.starts_with("+ ") {
        Some(DiffLineKind::Add)
    } else if text.starts_with("- ") {
        Some(DiffLineKind::Remove)
    } else if text.starts_with("  ") {
        Some(DiffLineKind::Context)
    } else {
        None
    }
}

fn is_diff_context(tags: &[ChangeTag], idx: usize, context: usize) -> bool {
    let lo = idx.saturating_sub(context);
    let hi = idx
        .saturating_add(context)
        .min(tags.len().saturating_sub(1));
    (lo..=hi).any(|index| !matches!(tags[index], ChangeTag::Equal))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ToolDetailsWire {
    Text {
        summary: String,
        body: String,
    },
    Diff(DiffDetailsWire),
    Bash {
        command: String,
        stdout: String,
        stderr: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        full_output_path: Option<PathBuf>,
        #[serde(default)]
        stdout_truncation: Option<BashStreamTruncation>,
        #[serde(default)]
        stderr_truncation: Option<BashStreamTruncation>,
        #[serde(default)]
        task_id: Option<TaskId>,
    },
    SubAgentReport {
        agent_id: usize,
        task: String,
        report: String,
    },
    Todos {
        items: Vec<TodoItem>,
    },
    Image {
        summary: String,
        mime_type: String,
        original_dimensions: (u32, u32),
        displayed_dimensions: (u32, u32),
    },
    Json(Value),
}

impl From<ToolDetailsWire> for ToolDetails {
    fn from(wire: ToolDetailsWire) -> Self {
        match wire {
            ToolDetailsWire::Text { summary, body } => Self::Text { summary, body },
            ToolDetailsWire::Diff(DiffDetailsWire::Compact(diff)) => Self::Diff(diff),
            ToolDetailsWire::Diff(DiffDetailsWire::Legacy(legacy)) => {
                Self::Diff(DiffDetails::new(legacy.path, legacy.before, legacy.after))
            }
            ToolDetailsWire::Bash {
                command,
                stdout,
                stderr,
                exit_code,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                task_id,
            } => Self::Bash {
                command,
                stdout,
                stderr,
                exit_code,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                task_id,
            },
            ToolDetailsWire::SubAgentReport {
                agent_id,
                task,
                report,
            } => Self::SubAgentReport {
                agent_id,
                task,
                report,
            },
            ToolDetailsWire::Todos { items } => Self::Todos { items },
            ToolDetailsWire::Image {
                summary,
                mime_type,
                original_dimensions,
                displayed_dimensions,
            } => Self::Image {
                summary,
                mime_type,
                original_dimensions,
                displayed_dimensions,
            },
            ToolDetailsWire::Json(value) => Self::Json(value),
        }
    }
}

impl<'de> Deserialize<'de> for ToolDetails {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ToolDetailsWire::deserialize(deserializer).map(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Bash truncation summary
// ---------------------------------------------------------------------------

/// Per-stream truncation summary attached to [`ToolDetails::Bash`].
///
/// Trimmed to the fields downstream renderers actually consume. The
/// type is intentionally serializable so it round-trips through the
/// JSONL session log alongside the rest of `ToolDetails`; older logs
/// that lack the field deserialize cleanly thanks to `#[serde(default)]`
/// on the enclosing `Option`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashStreamTruncation {
    /// Line count of the original (un-truncated) source stream. A
    /// single trailing newline is consumed and does not add a phantom
    /// empty line.
    pub total_lines: u64,
    /// Byte length of the original source stream.
    pub total_bytes: u64,
    /// Line count of the kept tail content present in `stdout` /
    /// `stderr`. When `last_line_partial` is set this is `1` — a
    /// single partial line counts as one output line.
    pub output_lines: u64,
    /// Byte length of the kept tail content.
    pub output_bytes: u64,
    /// Which budget triggered truncation.
    pub truncated_by: TruncationCause,
    /// True iff the kept tail starts mid-line because the source's
    /// trailing line alone exceeded the byte budget. Renderers use
    /// this to switch to the `[Showing last <bytes> of line N (line
    /// is <size>)]` marker variant.
    #[serde(default)]
    pub last_line_partial: bool,
    /// Size in bytes of the source's trailing line, meaningful when
    /// `last_line_partial` is `true`.
    #[serde(default)]
    pub last_line_bytes: u64,
}

/// Which budget triggered a tool-output truncation.
///
/// Serialized as `"lines"` or `"bytes"`. Lives alongside
/// [`BashStreamTruncation`] in `aj-agent` because the persisted schema
/// is the source of truth for tool-result rendering; the truncation
/// algorithm in `aj-tools::truncate` imports it from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncationCause {
    /// Hit the line cap.
    Lines,
    /// Hit the byte cap.
    Bytes,
}

// ---------------------------------------------------------------------------
// Todo item
// ---------------------------------------------------------------------------

/// A single todo entry, used by [`ToolDetails::Todos`] and the
/// `todo_read` / `todo_write` tool implementations.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    /// Stable identifier within the session.
    pub id: String,
    /// Human-readable description.
    pub content: String,
    /// Priority hint for ordering.
    pub priority: TodoPriority,
    /// Current status.
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TodoPriority {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStatus {
    Todo,
    InProgress,
    Completed,
}

// ---------------------------------------------------------------------------
// Tool outcome
// ---------------------------------------------------------------------------

/// Structured result returned from [`ToolDefinition::execute`].
///
/// The agent splits this into two channels: `content` rides on the
/// wire `tool_result` message back to the model, while `details` is
/// projected onto the [`crate::events::AgentEvent::ToolExecutionEnd`]
/// event for UI rendering and persistence.
#[derive(Clone, Debug)]
pub struct ToolOutcome {
    /// Content sent back to the model as the tool_result message.
    /// Maps directly onto the wire `ToolResultMessage.content`.
    pub content: Vec<UserContent>,
    /// Structured payload for UI rendering. Tool-specific.
    pub details: ToolDetails,
    /// Whether the execution should be reported to the model as an
    /// error tool_result.
    pub is_error: bool,
}

/// Result of a blocking [`ToolContext::spawn_agent`].
///
/// Carries enough information for callers (currently the `agent`
/// builtin) to construct a [`ToolDetails::SubAgentReport`] without
/// looking up the freshly-allocated sub-agent id elsewhere. The
/// `agent_id` is the `n` from this child's [`AgentId::Sub`] id, also
/// surfaced on the bus through
/// [`crate::events::AgentEvent::SubAgentStart`] /
/// [`crate::events::AgentEvent::SubAgentEnd`].
#[derive(Clone, Debug)]
pub struct SpawnedAgent {
    /// The `n` from this sub-agent's [`AgentId::Sub`] id.
    pub agent_id: usize,
    /// Final assistant text returned by the sub-agent.
    pub report: String,
    /// How the run concluded. `Completed` for a clean stop, `Truncated`
    /// when the final message hit the token cap (the parent still
    /// delivers `report` but flags the tool result). A hard failure is
    /// surfaced as an `Err` from `spawn_agent`, not a `Completed`, so this
    /// is never `Failed`.
    pub conclusion: SubAgentConclusion,
}

/// How [`ToolContext::spawn_agent`] runs the child's initial turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnMode {
    /// Run the child inline; the call resolves with its report.
    Blocking,
    /// Register a background task and return once the child's run is
    /// started on a detached driver.
    Background,
}

/// Result of [`ToolContext::spawn_agent`], shaped by the requested
/// [`SpawnMode`].
#[derive(Clone, Debug)]
pub enum SpawnResult {
    /// A blocking spawn ran the child to completion.
    Completed(SpawnedAgent),
    /// A background spawn started the child's run on a detached
    /// driver; the report arrives later through the task's completion
    /// notice (and `task_output` once terminal).
    Started {
        /// The `n` from this sub-agent's [`AgentId::Sub`] id.
        agent_id: usize,
        /// Id of the background task driving the run.
        task_id: TaskId,
    },
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Identifier of a background task. Minted per session by the
/// [`TaskRegistry`], shared between bash tasks and background agent
/// spawns (`#1`, `#2`, …).
pub type TaskId = usize;

/// Lifecycle status of a background task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task's driver is still running.
    Running,
    /// Process exited (code is `None` when signal-killed), or the
    /// agent-backed run completed/failed.
    Exited(Option<i32>),
    /// Killed via `task_stop`, the TUI, or shutdown.
    Killed,
}

impl TaskStatus {
    /// Whether the task has reached a terminal status.
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

/// What kind of work a background task runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// A detached `bash -c` child.
    Bash {
        /// The exact command line executed.
        command: String,
    },
    /// A background sub-agent run.
    Agent {
        /// The `n` from this sub-agent's [`AgentId::Sub`] id.
        agent_id: usize,
        /// Task description supplied by the parent.
        task: String,
    },
}

/// Status-independent snapshot of a background task's output.
///
/// Stateless: repeated snapshots return overlapping content. The
/// rolling tails are bounded; `spill_path` points at the canonical
/// full output on disk (always persisted for background tasks) so
/// callers can read past the tail with `read_file`. Byte totals are
/// exact even when the tail has been trimmed.
#[derive(Clone, Debug, Default)]
pub struct TaskRead {
    /// Rolling tail of standard output.
    pub stdout_tail: String,
    /// Rolling tail of standard error.
    pub stderr_tail: String,
    /// Total bytes that flowed through stdout.
    pub stdout_total_bytes: u64,
    /// Total bytes that flowed through stderr.
    pub stderr_total_bytes: u64,
    /// Path of the spill file carrying the full interleaved output.
    pub spill_path: Option<PathBuf>,
    /// Final report of an agent-backed run, set by the driver right
    /// before the task turns terminal. `None` for bash tasks and
    /// while an agent task is still running.
    pub report: Option<String>,
}

/// Type-erased handle to a background task's output buffer.
///
/// Keeps `aj-agent` free of process details while the tool crate owns
/// the buffering implementation.
pub trait TaskOutputSource: Send + Sync {
    /// Snapshot of the rolling output tail per stream, exact byte
    /// totals, and the spill path. Stateless: repeated calls return
    /// overlapping content; the *caller* applies display truncation.
    fn snapshot(&self) -> TaskRead;
}

/// Plumbing handed to a background task's detached driver by
/// [`ToolContext::start_background_task`].
pub struct StartedTask {
    /// The freshly minted task id.
    pub id: TaskId,
    /// Cancellation token for the driver — a child of the registry's
    /// session-scoped root, NOT of the per-turn token, so the task
    /// outlives the originating turn but dies on shutdown.
    pub cancel: CancellationToken,
    /// Event sink the driver reports through.
    pub events: TaskEventSink,
}

/// Opening tag wrapping a harness-injected task-completion notice when
/// it is projected onto the wire. [`AgentMessage::to_projected_wire`]
/// frames the notice body between this tag and
/// [`TASK_NOTIFICATION_CLOSE_TAG`] as a user message, marking it to the
/// model as harness-injected rather than a user reply. The framing is
/// projection-only: locally a notice is a typed
/// [`AgentMessageKind::TaskNotification`], so the format lives here as
/// the single source of truth for what the model sees.
///
/// [`AgentMessage::to_projected_wire`]: crate::message::AgentMessage::to_projected_wire
/// [`AgentMessageKind::TaskNotification`]: crate::message::AgentMessageKind::TaskNotification
pub(crate) const TASK_NOTIFICATION_OPEN_TAG: &str = "<task-notification>";

/// Closing tag paired with [`TASK_NOTIFICATION_OPEN_TAG`].
pub(crate) const TASK_NOTIFICATION_CLOSE_TAG: &str = "</task-notification>";

/// Completion notice queued when a background task reaches a terminal
/// status, drained into the owner's transcript at the next drain point
/// as a typed [`AgentMessageKind::TaskNotification`].
///
/// [`AgentMessageKind::TaskNotification`]: crate::message::AgentMessageKind::TaskNotification
#[derive(Clone, Debug)]
pub struct TaskNotice {
    /// The agent that started the task and receives the notice.
    pub owner: AgentId,
    /// Id of the finished task.
    pub task_id: TaskId,
    /// What kind of work the task ran.
    pub kind: TaskKind,
    /// Display label (command / task description).
    pub label: String,
    /// Terminal status the task ended with.
    pub status: TaskStatus,
    /// Pre-rendered notice body. For bash: exit status, final output
    /// tail, and the spill path; for agents: the report verbatim.
    pub body: String,
}

/// How a detached task driver reaches the bus and the notice queue
/// without holding the whole agent.
///
/// Drivers are plain tokio tasks, so emits are properly awaited. Bus
/// listener errors are logged and swallowed: a task finishing outside
/// any turn has no turn to abort, and persistence never subscribes to
/// task events.
#[derive(Clone)]
pub struct TaskEventSink {
    bus: EventBus,
    registry: TaskRegistry,
    owner: AgentId,
    task_id: TaskId,
    call_id: String,
    label: String,
}

impl TaskEventSink {
    /// Build a sink for the task `task_id` owned by `owner`,
    /// correlated to the originating tool call `call_id`.
    pub fn new(
        bus: EventBus,
        registry: TaskRegistry,
        owner: AgentId,
        task_id: TaskId,
        call_id: String,
        label: String,
    ) -> Self {
        Self {
            bus,
            registry,
            owner,
            task_id,
            call_id,
            label,
        }
    }

    /// The agent that owns the task (the notice recipient).
    pub fn owner(&self) -> AgentId {
        self.owner
    }

    /// The task this sink reports for.
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// The display label the task was registered with.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Emit [`AgentEvent::TaskStart`] announcing the task to the bus.
    pub async fn started(&self, kind: TaskKind) {
        let result = self
            .bus
            .emit(AgentEvent::TaskStart {
                agent_id: self.owner,
                task_id: self.task_id,
                call_id: self.call_id.clone(),
                kind,
                label: self.label.clone(),
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(
                task_id = self.task_id,
                "task start listener failed: {err:#}"
            );
        }
    }

    /// Emit a [`AgentEvent::TaskOutput`] snapshot (drivers
    /// self-throttle, ~10/s).
    pub async fn output(&self, partial: ToolDetails) {
        let result = self
            .bus
            .emit(AgentEvent::TaskOutput {
                agent_id: self.owner,
                task_id: self.task_id,
                call_id: self.call_id.clone(),
                partial,
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(
                task_id = self.task_id,
                "task output listener failed: {err:#}"
            );
        }
    }

    /// Flip the registry status to `status`, queue `notice`, and emit
    /// [`AgentEvent::TaskEnd`].
    pub async fn finished(&self, status: TaskStatus, notice: TaskNotice) {
        self.registry.set_status(self.task_id, status);
        // Queue the notice before announcing TaskEnd: the binary's
        // wake trigger fires off TaskEnd, and the woken agent must
        // find the notice already queued.
        self.registry.push_notice(notice);
        let result = self
            .bus
            .emit(AgentEvent::TaskEnd {
                agent_id: self.owner,
                task_id: self.task_id,
                call_id: self.call_id.clone(),
                status,
                label: self.label.clone(),
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(task_id = self.task_id, "task end listener failed: {err:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tool trait + erased shape
// ---------------------------------------------------------------------------

/// A typed tool implementation.
///
/// Concrete tools implement this trait with a strongly-typed `Input`
/// and convert into an [`ErasedToolDefinition`] for storage in a
/// heterogeneous collection.
pub trait ToolDefinition: Send + Sync {
    /// The tool's input shape. Deserialized from the model's JSON
    /// arguments.
    type Input: JsonSchema + DeserializeOwned + Send;

    /// The tool name advertised to the model. Must match the name
    /// reported in [`crate::events::AgentEvent::ToolExecutionStart`]
    /// events.
    fn name(&self) -> &'static str;

    /// Free-text description shown to the model.
    fn description(&self) -> &'static str;

    /// JSON Schema for [`Self::Input`]. Default implementation derives
    /// from `JsonSchema`.
    fn input_schema(&self) -> Value {
        derive_schema::<Self::Input>()
    }

    /// Per-tool execution mode. Default [`ExecutionMode::Parallel`].
    /// Tools that mutate the filesystem or run arbitrary commands
    /// should override to [`ExecutionMode::Sequential`].
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::default()
    }

    /// Run the tool. Errors should be surfaced as `is_error: true`
    /// outcomes when the model can recover; bubbling up an `Err`
    /// causes the agent to synthesize a generic error tool_result
    /// and abort the turn.
    fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> impl Future<Output = Result<ToolOutcome, BoxError>> + Send;
}

/// Boxed-future signature used by [`ErasedToolDefinition::func`].
///
/// Held behind an `Arc` so [`ErasedToolDefinition`] is cheaply
/// cloneable: the agent clones the parent's tool list for each
/// sub-agent it spawns (filtered to drop the `agent` tool itself),
/// and bumping a refcount per tool keeps that path allocation-free.
pub type ErasedToolFn = Arc<
    dyn for<'a> Fn(
            &'a mut dyn ToolContext,
            Value,
        )
            -> Pin<Box<dyn Future<Output = Result<ToolOutcome, BoxError>> + Send + 'a>>
        + Send
        + Sync,
>;

/// Type-erased tool definition for working with heterogeneous
/// collections of tools (the agent stores `Vec<ErasedToolDefinition>`).
///
/// Convert from a [`ToolDefinition`] via the blanket
/// `From<T> for ErasedToolDefinition` impl.
#[derive(Clone)]
pub struct ErasedToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub execution_mode: ExecutionMode,
    pub func: ErasedToolFn,
}

impl<T> From<T> for ErasedToolDefinition
where
    T: ToolDefinition + Clone + 'static,
{
    fn from(tool: T) -> Self {
        let name = tool.name().to_string();
        let description = tool.description().to_string();
        let input_schema = tool.input_schema();
        let execution_mode = tool.execution_mode();
        ErasedToolDefinition {
            name,
            description,
            input_schema,
            execution_mode,
            func: Arc::new(move |ctx, raw_input| {
                let parsed: Result<T::Input, _> = serde_json::from_value(raw_input);
                let tool = tool.clone();
                Box::pin(async move {
                    let input = parsed?;
                    tool.execute(ctx, input).await
                })
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool context
// ---------------------------------------------------------------------------

/// Runtime context passed to [`ToolDefinition::execute`].
///
/// Provides access to agent-scoped state (working directory, todo
/// list), sub-agent spawning, progress updates, and a cancellation
/// token tools must honor for long-running work.
pub trait ToolContext: Send {
    /// Current working directory for the session.
    fn working_directory(&self) -> PathBuf;

    /// Current todo list snapshot.
    fn get_todo_list(&self) -> Vec<TodoItem>;

    /// Replace the session's todo list.
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);

    /// Spawn a sub-agent on the current bus.
    ///
    /// The child shares the parent's event bus tagged with a fresh
    /// [`crate::events::AgentId::Sub`]. With [`SpawnMode::Blocking`]
    /// the call runs the child's initial turn inline (run cancellation
    /// derives from the parent's [`Self::cancellation`]) and resolves
    /// to [`SpawnResult::Completed`]. With [`SpawnMode::Background`]
    /// the run continues on a detached task whose cancellation is the
    /// background task's token, and the call resolves to
    /// [`SpawnResult::Started`] immediately.
    fn spawn_agent<'a>(
        &'a mut self,
        task: String,
        mode: SpawnMode,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnResult, BoxError>> + Send + 'a>>;

    /// Emit a partial [`ToolDetails`] snapshot through the bus as a
    /// [`crate::events::AgentEvent::ToolExecutionUpdate`]. Tools that
    /// produce many updates self-throttle (~10/s); the agent does not
    /// debounce.
    ///
    /// Awaited inline by the calling tool, which itself runs inline in
    /// the agent's turn. That ordering is the whole point: a foreground
    /// tool finishes (and the agent emits the terminal
    /// [`crate::events::AgentEvent::ToolExecutionEnd`]) only after this
    /// future resolves, so a straggling update can never overtake the
    /// end event on the bus.
    fn emit_update<'a>(
        &'a mut self,
        partial: ToolDetails,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Cancellation token tools must observe for long-running work.
    /// Cancellation propagates from `Agent::cancel` and from a parent
    /// agent's cancellation when this is a sub-agent.
    fn cancellation(&self) -> CancellationToken;

    /// Shared task registry handle (for the `task_*` tools).
    fn task_registry(&self) -> TaskRegistry;

    /// Register a background task and get the plumbing its detached
    /// driver needs: the task id, a cancel token (child of the
    /// registry root, NOT of the per-turn token), and an event sink.
    ///
    /// Caller contract: registration is synchronous, and the caller
    /// must spawn the driver with no await point in between. Tool
    /// futures are cancelled by drop, and a drop in that window would
    /// leave a phantom `Running` registry entry with no driver to
    /// ever flip it. The driver must emit [`TaskEventSink::started`]
    /// before any other task event.
    ///
    /// NOTE: ids are allocated by the registry, not by session state,
    /// so detached drivers never need the `&mut SessionState` borrow
    /// that makes foreground tools block.
    fn start_background_task(
        &mut self,
        kind: TaskKind,
        label: String,
        output: Arc<dyn TaskOutputSource>,
    ) -> StartedTask;
}

// ---------------------------------------------------------------------------
// Schema helper
// ---------------------------------------------------------------------------

/// Derive a JSON schema suitable for use as a tool's `input_schema`.
///
/// Strips the `title` and ensures `properties` / `required` are
/// always present so the resulting object validates as a function
/// parameter schema for both Anthropic and OpenAI.
pub fn derive_schema<T: JsonSchema>() -> Value {
    let generator = SchemaSettings::default()
        .with(|s| {
            // The meta schema link bloats the output without helping
            // the model.
            s.meta_schema = None;
        })
        .into_generator();
    let mut schema = generator.into_root_schema_for::<T>();

    // The title is just the type name and adds noise.
    schema.remove("title");

    if schema.get("properties").is_none() {
        schema.insert("properties".to_string(), json!({}));
    }
    if schema.get("required").is_none() {
        schema.insert("required".to_string(), json!([]));
    }

    // NOTE: The value comes from `schemars`, which always produces a
    // serializable schema object, so this `expect` is a true invariant
    // rather than an unchecked panic on a tool author's `Input` type.
    serde_json::to_value(&schema).expect("invalid schema object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_details_round_trips_each_variant() {
        // The persistence listener writes ToolDetails alongside each
        // tool_result message. This locks the {"kind": "..."} framing
        // that the session log and the TUI renderer both depend on.
        let cases = [
            ToolDetails::Text {
                summary: "hi".into(),
                body: "body".into(),
            },
            ToolDetails::Diff(DiffDetails::new("a.txt", "old", "new")),
            ToolDetails::Bash {
                command: "ls".into(),
                stdout: "out".into(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
                stdout_truncation: None,
                stderr_truncation: None,
                task_id: None,
            },
            ToolDetails::SubAgentReport {
                agent_id: 1,
                task: "do thing".into(),
                report: "done".into(),
            },
            ToolDetails::Todos { items: Vec::new() },
            ToolDetails::Image {
                summary: "/tmp/screenshot.png".into(),
                mime_type: "image/png".into(),
                original_dimensions: (1920, 1080),
                displayed_dimensions: (800, 450),
            },
            ToolDetails::Json(json!({"x": 1})),
        ];
        for case in cases {
            let json = serde_json::to_value(&case).expect("serialize");
            assert!(json.get("kind").is_some(), "missing kind tag: {json}");
            let _back: ToolDetails = serde_json::from_value(json).expect("deserialize round trip");
        }
    }

    #[test]
    fn compact_diff_uses_canonical_wire_shape() {
        let details = ToolDetails::Diff(DiffDetails::new("src/lib.rs", "old", "new"));
        let value = serde_json::to_value(&details).expect("serialize compact diff");

        assert_eq!(value["kind"], "diff");
        assert_eq!(value["format"], "display-v1");
        assert_eq!(value["path"], "src/lib.rs");
        assert_eq!(
            value["lines"],
            json!(["--- a/src/lib.rs", "+++ b/src/lib.rs", "- old", "+ new"])
        );
        assert_eq!(value["missing_newline"], json!([2, 3]));
        assert!(value.get("content_fingerprint").is_none());
        assert!(value.get("before").is_none());
        assert!(value.get("after").is_none());
    }

    #[test]
    fn legacy_diff_deserializes_and_reserializes_compactly() {
        let legacy = json!({
            "kind": "diff",
            "path": "src/lib.rs",
            "before": "same\nold\n",
            "after": "same\nnew\n",
        });

        let details: ToolDetails = serde_json::from_value(legacy).expect("legacy diff parses");
        let ToolDetails::Diff(diff) = &details else {
            panic!("expected diff details");
        };
        assert_eq!(diff.path(), "src/lib.rs");
        assert!(diff.lines().iter().any(|line| line.text() == "- old"));
        assert!(diff.lines().iter().any(|line| line.text() == "+ new"));

        let compact = serde_json::to_value(details).expect("serialize normalized diff");
        assert_eq!(compact["format"], "display-v1");
        assert!(compact.get("before").is_none());
        assert!(compact.get("after").is_none());
    }

    #[test]
    fn diff_preserves_headers_context_separators_and_identical_behavior() {
        let creation = DiffDetails::new("f.txt", "", "hello\nworld\n");
        assert_eq!(creation.lines()[0].text(), "+++ b/f.txt");
        assert!(
            creation
                .lines()
                .iter()
                .all(|line| line.text() != "--- a/f.txt")
        );

        let deletion = DiffDetails::new("f.txt", "hello\n", "");
        assert_eq!(deletion.lines()[0].text(), "--- a/f.txt");
        assert!(
            deletion
                .lines()
                .iter()
                .all(|line| line.text() != "+++ b/f.txt")
        );

        let before = "one\na\nb\nc\nd\ne\nf\ng\ntwo\n";
        let after = "ONE\na\nb\nc\nd\ne\nf\ng\nTWO\n";
        let diff = DiffDetails::new("f.txt", before, after);
        assert_eq!(
            diff.lines()
                .iter()
                .filter(|line| line.kind() == DiffLineKind::Separator)
                .count(),
            1
        );
        assert!(diff.lines().iter().any(|line| line.text() == "  a"));

        let identical = DiffDetails::new("f.txt", "same\n", "same\n");
        assert!(
            identical
                .lines()
                .iter()
                .all(|line| line.kind() == DiffLineKind::Header)
        );
    }

    #[test]
    fn diff_sanitizes_inputs_and_preserves_unicode_and_tabs() {
        let diff = DiffDetails::new(
            "src/odd\nname\r\n\x1b[31mlib.rs",
            "α\told\x1b[0m\n",
            "α\tnew\n",
        );

        assert_eq!(diff.path(), "src/odd\\nname\\nlib.rs");
        assert!(diff.lines().iter().any(|line| line.text() == "- α\told"));
        assert!(diff.lines().iter().any(|line| line.text() == "+ α\tnew"));
        assert!(diff.lines().iter().all(|line| {
            !line.text().contains('\x1b')
                && !line.text().contains('\r')
                && !line.text().contains('\n')
        }));
    }

    #[test]
    fn compact_diff_rejects_embedded_line_breaks() {
        for line in ["+ first\n+ second", "- first\r- second"] {
            let value = json!({
                "kind": "diff",
                "format": "display-v1",
                "path": "src/lib.rs",
                "lines": ["--- a/src/lib.rs", "+++ b/src/lib.rs", line],
                "before": "old\n",
                "after": "new\n",
            });

            serde_json::from_value::<ToolDetails>(value)
                .expect_err("embedded line break must be rejected");
        }
    }

    #[test]
    fn compact_diff_rejects_unknown_format_even_with_legacy_snapshots() {
        let value = json!({
            "kind": "diff",
            "format": "display-v2",
            "path": "src/lib.rs",
            "lines": ["--- a/src/lib.rs", "+++ b/src/lib.rs", "- old", "+ new"],
            "before": "old\n",
            "after": "new\n",
        });

        serde_json::from_value::<ToolDetails>(value)
            .expect_err("unknown compact format must not fall through to legacy");
    }

    #[test]
    fn diff_retains_missing_final_newline_indexes() {
        let diff = DiffDetails::new("f.txt", "same\nold", "same\nnew");
        let missing: Vec<&str> = diff
            .missing_newline_indexes()
            .iter()
            .map(|&index| {
                let index = usize::try_from(index).expect("u32 index fits usize");
                diff.lines()[index].text()
            })
            .collect();

        assert_eq!(missing, vec!["- old", "+ new"]);
        let fingerprint = diff.content_fingerprint();
        let round_trip: ToolDetails = serde_json::from_value(
            serde_json::to_value(ToolDetails::Diff(diff)).expect("serialize"),
        )
        .expect("deserialize");
        let ToolDetails::Diff(round_trip) = round_trip else {
            panic!("expected diff details");
        };
        assert_eq!(round_trip.missing_newline_indexes().len(), 2);
        assert_eq!(round_trip.content_fingerprint(), fingerprint);
    }

    #[test]
    fn broad_rewrite_with_expired_deadline_has_coarse_bounded_shape() {
        let before: String = (0..4_096).map(|index| format!("old-{index}\n")).collect();
        let after: String = (0..4_096).map(|index| format!("new-{index}\n")).collect();
        let expired = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("one second fits before now");

        let diff = DiffDetails::new_with_config("large.txt", before, after, |config| {
            config.deadline(expired);
        });

        assert_eq!(
            diff.lines()
                .iter()
                .filter(|line| line.kind() == DiffLineKind::Remove)
                .count(),
            4_096
        );
        assert_eq!(
            diff.lines()
                .iter()
                .filter(|line| line.kind() == DiffLineKind::Add)
                .count(),
            4_096
        );
        assert!(
            diff.lines()
                .iter()
                .all(|line| line.kind() != DiffLineKind::Context)
        );
    }

    #[test]
    fn compact_diff_size_does_not_scale_with_unchanged_file_size() {
        let payload = "x".repeat(80);
        let before: String = (0..15_000)
            .map(|index| format!("{index:05} {payload}\n"))
            .collect();
        let old = format!("07500 {payload}\n");
        let new = format!("07500 {}\n", "y".repeat(80));
        let after = before.replacen(&old, &new, 1);

        assert!(before.len() > 1_000_000);
        let value = serde_json::to_value(ToolDetails::Diff(DiffDetails::new(
            "large.txt",
            &before,
            &after,
        )))
        .expect("serialize large-file edit");
        let encoded = serde_json::to_vec(&value).expect("encode compact diff");

        assert!(value.get("before").is_none());
        assert!(value.get("after").is_none());
        assert!(
            encoded.len() < 2_000,
            "compact diff was {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn execution_mode_default_is_parallel() {
        assert_eq!(ExecutionMode::default(), ExecutionMode::Parallel);
    }

    #[test]
    fn derive_schema_shapes_input_for_the_wire() {
        // `derive_schema` faces the provider function-parameter format:
        // it must drop the `title` schemars emits (noise to the model)
        // and guarantee `properties`/`required` exist so the object
        // validates for both Anthropic and OpenAI. A `schemars` upgrade
        // reintroducing `title`, or a fieldless input producing neither
        // key, would silently break tool calls.
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct WithFields {
            command: String,
            count: u32,
        }

        // A fieldless input is the corner case: schemars can emit an
        // object schema with no `properties`/`required` at all.
        #[derive(JsonSchema)]
        struct Fieldless {}

        for schema in [derive_schema::<WithFields>(), derive_schema::<Fieldless>()] {
            assert!(
                schema.get("title").is_none(),
                "title not stripped: {schema}"
            );
            assert!(
                schema.get("properties").is_some(),
                "properties missing: {schema}"
            );
            assert!(
                schema.get("required").is_some(),
                "required missing: {schema}"
            );
        }
    }

    #[test]
    fn bash_details_deserialize_from_legacy_payload() {
        // Sessions captured before the truncation fields existed wrote
        // a `Bash` payload with only command/stdout/stderr. The
        // `#[serde(default)]` attributes must let such a line load with
        // the new fields defaulting cleanly, so old logs keep rendering
        // (falling back to the legacy `[Output truncated]` marker).
        let legacy = json!({
            "kind": "bash",
            "command": "ls",
            "stdout": "out",
            "stderr": "",
        });

        let details: ToolDetails = serde_json::from_value(legacy).expect("legacy bash parses");
        match details {
            ToolDetails::Bash {
                command,
                exit_code,
                truncated,
                full_output_path,
                stdout_truncation,
                stderr_truncation,
                task_id,
                ..
            } => {
                assert_eq!(command, "ls");
                assert_eq!(exit_code, None);
                assert!(!truncated);
                assert_eq!(full_output_path, None);
                assert!(stdout_truncation.is_none());
                assert!(stderr_truncation.is_none());
                assert_eq!(task_id, None);
            }
            other => panic!("expected Bash, got {other:?}"),
        }
    }
}
