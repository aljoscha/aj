//! The tool-execution cell: one tool call's bubble in the transcript.
//!
//! Renders a [`aj_app::chat::ToolEntry`] as a full-width tinted
//! [`Bubble`]: the `{glyph} {tool}({args})` header, an optional task
//! badge, and a body dispatched on the entry's
//! [`aj_agent::tool::ToolDetails`] variant. Sub-agent tool entries
//! flagged `header_only` render as a bare wrapped header line
//! instead, so they compose inside the sub-agent box.
//!
//! The cell is a plain function of the entry plus the session-wide
//! `tools_expanded` flag: the transcript's builder constructs a fresh
//! widget per draw, so there is no cache or event handling here.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use aj_agent::tool::{DiffLineKind, TaskId, TaskKind, TaskStatus, ToolDetails};
use aj_app::chat::{TaskInfo, ToolEntry, ToolStatus};
use aj_tools::sanitize_terminal_output;
use aj_tools::tools::bash::stream_marker;
use aj_tools::tools::todo::format_todo_list;
use serde_json::Value;
use vaxis::cell::Style;
use vaxis::vxfw::TextSpan;

use crate::bubble::{Bubble, BubbleImage};
use crate::image_store::ImageRender;
use crate::transcript::TranscriptStyles;

/// Maximum body lines rendered for a head-truncated tool output
/// (`Text` / `SubAgentReport` variants) when collapsed. Sized to give
/// a quick at-a-glance preview without flooding the scrollback.
const TEXT_COLLAPSED_LINES: usize = 10;

/// Maximum body lines rendered for a head-truncated `SubAgentReport`
/// body when collapsed. Aliased to [`TEXT_COLLAPSED_LINES`] for
/// parallelism but kept as its own constant so a divergence is a
/// one-line change. Shared with the sub-agent box, which folds its
/// done-report body the same way.
pub(crate) const REPORT_COLLAPSED_LINES: usize = TEXT_COLLAPSED_LINES;

/// Number of trailing lines kept per bash stream when collapsed.
const BASH_COLLAPSED_LINES: usize = 5;

/// Display label of the tools-expand chord shown in collapse hints,
/// resolved from the shared default binding table. Follows user
/// `[keybindings]` overrides once those land.
pub(crate) static EXPAND_KEY_LABEL: LazyLock<String> = LazyLock::new(|| {
    aj_app::keybindings::action_shortcut(aj_app::keybindings::ACTION_TOOLS_EXPAND)
        .expect("aj.tools.expand has a default chord")
});

/// Whether a collapse hint describes head- or tail-truncated content.
/// The phrasing (`N more lines` vs `N earlier lines`) keeps the hint
/// honest about which end of the stream got dropped.
#[derive(Clone, Copy)]
pub(crate) enum HintKind {
    /// Body was truncated to its head, the hidden lines come after
    /// the visible ones (`Text`, `SubAgentReport`).
    More,
    /// Body was truncated to its tail, the hidden lines come before
    /// the visible ones (`Bash` per-stream).
    Earlier,
}

/// Format the `… (N <kind> lines, <key> to expand)` hint line that
/// signals a collapsed body has more content. Shared with the user
/// bubble's task-notification fold.
pub(crate) fn expand_hint(more: usize, kind: HintKind) -> String {
    let word = match kind {
        HintKind::More => "more",
        HintKind::Earlier => "earlier",
    };
    let key = EXPAND_KEY_LABEL.as_str();
    format!("… ({more} {word} lines, {key} to expand)")
}

/// How the finished (or running) call should read to the viewer.
/// Drives the header glyph and the bubble's background tint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VisualStatus {
    Pending,
    Succeeded,
    Failed,
}

/// Decide which [`VisualStatus`] to paint the cell with.
///
/// The agent's `is_error` flag is reserved for catastrophic failures
/// (cancellation, timeout, schema errors). It deliberately does not
/// fire for a successful invocation that returns a non-zero exit
/// code, because the model reads `exit_code` from the structured
/// payload and decides for itself. For the human viewer, though, an
/// `[exit 1]` line under a green check reads wrong, so any explicit
/// non-zero exit paints as failed regardless of the flag.
///
/// A tracked background task's terminal status overrides both: the
/// launch call's own result only covers the spawn, and the badge's
/// outcome is what the user actually cares about. Untracked ids
/// (resumed cells, task still running) keep the base status.
fn derive_status(entry: &ToolEntry, tasks: &BTreeMap<TaskId, TaskInfo>) -> VisualStatus {
    let base = match entry.status {
        ToolStatus::Running => VisualStatus::Pending,
        ToolStatus::Done { is_error: true } => VisualStatus::Failed,
        ToolStatus::Done { is_error: false } => match &entry.details {
            Some(ToolDetails::Bash {
                exit_code: Some(code),
                ..
            }) if *code != 0 => VisualStatus::Failed,
            _ => VisualStatus::Succeeded,
        },
    };
    let Some(id) = badge_task_id(entry) else {
        return base;
    };
    match tasks.get(&id).map(|info| info.status) {
        Some(TaskStatus::Exited(Some(0))) => VisualStatus::Succeeded,
        Some(TaskStatus::Exited(_)) | Some(TaskStatus::Killed) => VisualStatus::Failed,
        Some(TaskStatus::Running) | None => base,
    }
}

/// The task id that badges this cell as a background launch, if any.
///
/// Sourced from the persisted `ToolDetails::Bash.task_id` so a
/// resumed transcript (where task events never fired and
/// `entry.task` is unset) still shows the badge. The live
/// `entry.task` covers the window between `TaskStart` and the launch
/// call's own result landing on the entry.
fn badge_task_id(entry: &ToolEntry) -> Option<TaskId> {
    if let Some(ToolDetails::Bash {
        task_id: Some(id), ..
    }) = &entry.details
    {
        return Some(*id);
    }
    entry.task
}

/// The header's background-task badge: `[task #N]` while the task
/// runs (or when its status is untracked, e.g. on a resumed cell),
/// `[task #N · exited 0]` etc. once the task reached a terminal
/// status. `None` for foreground tool calls.
///
/// Only bash launches carry the badge. An agent-kind task's launch
/// cell is the skipped `agent` tool call, and the sub-agent box
/// carries that status instead. The cell tint still follows the task
/// outcome for every kind, so only the badge is gated here.
fn task_badge(entry: &ToolEntry, tasks: &BTreeMap<TaskId, TaskInfo>) -> Option<String> {
    let id = badge_task_id(entry)?;
    let bash_details = matches!(
        &entry.details,
        Some(ToolDetails::Bash {
            task_id: Some(_),
            ..
        })
    );
    if !bash_details
        && !tasks
            .get(&id)
            .is_some_and(|info| matches!(info.kind, TaskKind::Bash { .. }))
    {
        return None;
    }
    Some(match tasks.get(&id).map(|info| info.status) {
        None | Some(TaskStatus::Running) => format!("[task #{id}]"),
        Some(TaskStatus::Exited(Some(code))) => format!("[task #{id} · exited {code}]"),
        Some(TaskStatus::Exited(None)) => format!("[task #{id} · terminated by signal]"),
        Some(TaskStatus::Killed) => format!("[task #{id} · killed]"),
    })
}

/// Build a single-line argument summary from the tool's input JSON.
/// The goal is a compact `k=v, k2=v2` preview. Nested values fall
/// back to a `…` placeholder.
fn format_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (k, v) in map {
                let v_str = match v {
                    Value::String(s) => format!("{k}={}", quote_for_summary(s)),
                    Value::Number(n) => format!("{k}={n}"),
                    Value::Bool(b) => format!("{k}={b}"),
                    Value::Null => format!("{k}=null"),
                    Value::Array(_) | Value::Object(_) => format!("{k}=…"),
                };
                parts.push(v_str);
            }
            parts.join(", ")
        }
        Value::String(s) => quote_for_summary(s),
        // Bare scalars or arrays go through the JSON form.
        other => other.to_string(),
    }
}

/// Wrap a free-form string in double quotes for the summary line.
/// Newlines / control characters are replaced with their `\n` / `\t`
/// escapes so the summary stays single-line even when the input
/// happened to be multi-line.
fn quote_for_summary(s: &str) -> String {
    const MAX_INLINE: usize = 60;
    let cleaned = s
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    if cleaned.chars().count() > MAX_INLINE {
        let head: String = cleaned.chars().take(MAX_INLINE).collect();
        format!("\"{head}…\"")
    } else {
        format!("\"{cleaned}\"")
    }
}

/// One rendered body line: a run of styled spans with no embedded
/// newlines. The widget flattens lines with `\n` separator spans,
/// which [`RichText`] treats as hard breaks.
type Line = Vec<TextSpan>;

fn span(text: impl Into<String>, style: Style) -> TextSpan {
    TextSpan {
        text: text.into(),
        style,
        ..TextSpan::default()
    }
}

/// A line made of a single styled span.
fn line(text: impl Into<String>, style: Style) -> Line {
    vec![span(text, style)]
}

/// Render the body lines for a [`ToolDetails`] variant.
///
/// Raw text fields pass through [`sanitize_terminal_output`] before styling.
/// Canonical diff lines were sanitized at construction or deserialization.
fn details_body(details: &ToolDetails, expanded: bool, styles: &TranscriptStyles) -> Vec<Line> {
    match details {
        ToolDetails::Text { summary, body } => {
            let summary = sanitize_terminal_output(summary);
            let body = sanitize_terminal_output(body);
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(line(summary, styles.dim));
            }
            let mut body_lines: Vec<&str> = body.split('\n').collect();
            // Trim a trailing empty line introduced by a body that
            // ended in `\n`. The bubble's bottom pad already handles
            // the vertical separation.
            if body_lines.last().is_some_and(|l| l.is_empty()) {
                body_lines.pop();
            }
            if !expanded && body_lines.len() > TEXT_COLLAPSED_LINES {
                let more = body_lines.len() - TEXT_COLLAPSED_LINES;
                body_lines.truncate(TEXT_COLLAPSED_LINES);
                lines.extend(body_lines.into_iter().map(|l| line(l, styles.text)));
                lines.push(line(expand_hint(more, HintKind::More), styles.dim));
            } else {
                lines.extend(body_lines.into_iter().map(|l| line(l, styles.text)));
            }
            lines
        }
        ToolDetails::Diff(diff) => diff
            .lines()
            .iter()
            .map(|diff_line| {
                let style = match diff_line.kind() {
                    DiffLineKind::Header | DiffLineKind::Separator | DiffLineKind::Context => {
                        styles.dim
                    }
                    DiffLineKind::Add => styles.diff_add,
                    DiffLineKind::Remove => styles.diff_remove,
                };
                line(diff_line.text(), style)
            })
            .collect(),
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
            // `stdout` / `stderr` are already sanitized at the bash tool
            // source. Running the transform again is cheap and keeps this arm
            // self-contained against future changes to the payload's provenance.
            let stdout = sanitize_terminal_output(stdout);
            let stderr = sanitize_terminal_output(stderr);
            let mut lines = vec![bash_command_line(command, styles)];

            if !stdout.is_empty() {
                push_stream_lines(&mut lines, &stdout, expanded, styles);
            }
            if let Some(t) = stdout_truncation {
                lines.push(line(
                    stream_marker("stdout", t, full_output_path.as_deref()),
                    styles.dim,
                ));
            }

            if !stderr.is_empty() {
                // Dim header so the eye notices the channel switch
                // without it competing with the actual error text.
                lines.push(line("STDERR:", styles.dim));
                push_stream_lines(&mut lines, &stderr, expanded, styles);
            }
            if let Some(t) = stderr_truncation {
                lines.push(line(
                    stream_marker("stderr", t, full_output_path.as_deref()),
                    styles.dim,
                ));
            }

            if let Some(code) = exit_code {
                let style = if *code == 0 { styles.dim } else { styles.error };
                lines.push(line(format!("[exit {code}]"), style));
            }

            // Legacy fallback marker: only when `truncated` is set but
            // neither structured per-stream summary is, typical of
            // sessions captured before the per-stream fields existed.
            if *truncated && stdout_truncation.is_none() && stderr_truncation.is_none() {
                let marker = match full_output_path {
                    Some(path) => format!("[Output truncated; full output at {}]", path.display()),
                    None => "[Output truncated]".to_string(),
                };
                lines.push(line(marker, styles.dim));
            }

            lines
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            let task = sanitize_terminal_output(task);
            let report = sanitize_terminal_output(report);
            let mut lines = vec![line(format!("sub-agent {agent_id}: {task}"), styles.dim)];
            let mut report_lines: Vec<&str> = report.split('\n').collect();
            if report_lines.last().is_some_and(|l| l.is_empty()) {
                report_lines.pop();
            }
            if !expanded && report_lines.len() > REPORT_COLLAPSED_LINES {
                let more = report_lines.len() - REPORT_COLLAPSED_LINES;
                report_lines.truncate(REPORT_COLLAPSED_LINES);
                lines.extend(report_lines.into_iter().map(|l| line(l, styles.text)));
                lines.push(line(expand_hint(more, HintKind::More), styles.dim));
            } else {
                lines.extend(report_lines.into_iter().map(|l| line(l, styles.text)));
            }
            lines
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from `aj-tools` so
            // the view matches the wire content the model sees.
            // `format_todo_list` sanitizes each item's content and
            // wraps completed items in SGR strikethrough markers,
            // which are translated into span attributes below (the
            // wrap engine renders raw escapes literally).
            let formatted = format_todo_list(items);
            formatted
                .trim_end_matches('\n')
                .split('\n')
                .map(|l| strikethrough_spans(l, styles.text))
                .collect()
        }
        ToolDetails::Json(value) => {
            let formatted =
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            formatted
                .split('\n')
                .map(|l| line(l, styles.text))
                .collect()
        }
        ToolDetails::Image {
            mime_type,
            original_dimensions: (orig_w, orig_h),
            displayed_dimensions: (disp_w, disp_h),
            ..
        } => {
            // The text fallback for an image: shown when inline rendering is
            // off (no terminal capability or the config gate). Renders the
            // source dimensions, and the displayed dimensions when a resize
            // occurred.
            let mime_type = sanitize_terminal_output(mime_type);
            let text = if (orig_w, orig_h) == (disp_w, disp_h) {
                format!("[image: {mime_type} · {orig_w}x{orig_h}]")
            } else {
                format!("[image: {mime_type} · {orig_w}x{orig_h} → {disp_w}x{disp_h}]")
            };
            vec![line(text, styles.dim)]
        }
    }
}

/// Push a stream's lines (`stdout` or `stderr`) into `out`, applying
/// tail compaction when `expanded` is false. The hint describing the
/// dropped lines is inserted before the visible tail so it reads in
/// the same direction as the remaining content.
///
/// A single trailing empty element produced by `split('\n')` on a
/// stream ending in `\n` is popped first so the visible tail never
/// ends in a stray blank row and the "N earlier lines" count reflects
/// real lines.
fn push_stream_lines(out: &mut Vec<Line>, stream: &str, expanded: bool, styles: &TranscriptStyles) {
    let mut all_lines: Vec<&str> = stream.split('\n').collect();
    if all_lines.last().is_some_and(|l| l.is_empty()) {
        all_lines.pop();
    }
    if expanded || all_lines.len() <= BASH_COLLAPSED_LINES {
        out.extend(all_lines.into_iter().map(|l| line(l, styles.text)));
        return;
    }
    let earlier = all_lines.len() - BASH_COLLAPSED_LINES;
    out.push(line(expand_hint(earlier, HintKind::Earlier), styles.dim));
    for l in &all_lines[all_lines.len() - BASH_COLLAPSED_LINES..] {
        out.push(line(*l, styles.text));
    }
}

/// Split `text` into spans, translating its SGR strikethrough markers
/// (`ESC[9m` on, `ESC[29m` off) into the span style's `strikethrough`
/// attribute. The caller contract is that the only intended escapes in `text`
/// are those two markers, so any other escape renders literally. Todo lists
/// meet this by sanitizing their content. The context notice adds only the
/// strike markers but does not sanitize the paths and skill names it wraps, so
/// a stray ESC in one of those would mis-parse.
pub(crate) fn strikethrough_spans(text: &str, base: Style) -> Line {
    const ON: &str = "\x1b[9m";
    const OFF: &str = "\x1b[29m";
    let struck = Style {
        strikethrough: true,
        ..base
    };
    let mut spans = Vec::new();
    let mut rest = text;
    let mut strike = false;
    loop {
        let marker = if strike { OFF } else { ON };
        match rest.find(marker) {
            Some(idx) => {
                if idx > 0 {
                    spans.push(span(&rest[..idx], if strike { struck } else { base }));
                }
                rest = &rest[idx + marker.len()..];
                strike = !strike;
            }
            None => {
                if !rest.is_empty() {
                    spans.push(span(rest, if strike { struck } else { base }));
                }
                break;
            }
        }
    }
    if spans.is_empty() {
        // Preserve blank lines: a line with zero spans would collapse
        // when flattened with `\n` separators.
        spans.push(span("", base));
    }
    spans
}

/// Build the header line: `{glyph} {tool}({args})[ {badge}]`.
fn header_line(
    entry: &ToolEntry,
    status: VisualStatus,
    tasks: &BTreeMap<TaskId, TaskInfo>,
    styles: &TranscriptStyles,
) -> Line {
    let glyph = match status {
        VisualStatus::Pending => span("…", styles.dim),
        VisualStatus::Succeeded => span("✓", styles.success),
        VisualStatus::Failed => span("✗", styles.error),
    };
    let mut spans = vec![
        glyph,
        span(" ", styles.text),
        span(&entry.tool, styles.bold),
        span("(", styles.text),
        span(format_args(&entry.args), styles.dim),
        span(")", styles.text),
    ];
    if let Some(badge) = task_badge(entry, tasks) {
        spans.push(span(" ", styles.text));
        spans.push(span(badge, styles.dim));
    }
    spans
}

/// Flatten lines into one span list with `\n` hard-break separators.
fn flatten_lines(lines: Vec<Line>, styles: &TranscriptStyles) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    for (i, l) in lines.into_iter().enumerate() {
        if i > 0 {
            spans.push(span("\n", styles.text));
        }
        spans.extend(l);
    }
    spans
}

/// The `$ {command}` line a bash cell renders as its first body line.
/// Shared by the full body ([`details_body`]) and the compact render so the
/// two stay identical. The command is sanitized and keeps any embedded
/// newlines, which wrap as extra rows.
fn bash_command_line(command: &str, styles: &TranscriptStyles) -> Line {
    line(
        format!("$ {}", sanitize_terminal_output(command)),
        styles.dim,
    )
}

/// Build the widget for `entry` under the current expansion flag.
///
/// In `compact` mode a tool cell renders header-only, except a bash cell keeps
/// its `$ command` line. The `expanded` tools-expand toggle wins over
/// `compact`: when both are set the full body renders, so tools-expand stays a
/// reveal-everything escape hatch even under compact mode.
///
/// `image` is how a tool-result image entry renders this frame, resolved by
/// the caller from the capability-and-config gate and the shared image store.
/// It only matters on the image arm: [`ImageRender::Transmitted`] places the
/// image, [`ImageRender::Pending`] reserves its rows blank while the transmit
/// is in flight, and [`ImageRender::Disabled`] falls back to the `[image: ...]`
/// text placeholder.
pub(crate) fn build_tool_cell(
    entry: &ToolEntry,
    tasks: &BTreeMap<TaskId, TaskInfo>,
    expanded: bool,
    compact: bool,
    styles: &TranscriptStyles,
    image: ImageRender,
) -> Bubble {
    let status = derive_status(entry, tasks);
    let header = header_line(entry, status, tasks, styles);

    if entry.header_only {
        // Header-only: just the wrapped header line, no bubble,
        // background, or body, so the tool composes inside the
        // sub-agent box's own painted background.
        return Bubble::entry(flatten_lines(vec![header], styles), None, styles.text);
    }

    let bg = match status {
        VisualStatus::Pending => styles.tool_pending_bg,
        VisualStatus::Succeeded => styles.tool_success_bg,
        VisualStatus::Failed => styles.tool_error_bg,
    };

    // A tool-result image renders graphically while it is live: the bubble
    // carries the header alone, with the image reserved below it via the
    // bubble's image block. `Transmitted` places it; `Pending` reserves the
    // rows blank. `Disabled` and `Failed` fall through to the `[image: ...]`
    // text placeholder in `details_body`.
    if let Some(ToolDetails::Image {
        displayed_dimensions,
        ..
    }) = &entry.details
    {
        let img = match image {
            ImageRender::Transmitted(id) => Some(BubbleImage {
                px: *displayed_dimensions,
                img_id: Some(id),
            }),
            ImageRender::Pending => Some(BubbleImage {
                px: *displayed_dimensions,
                img_id: None,
            }),
            // Both fall through to the `[image: ...]` text placeholder in
            // `details_body`: `Disabled` because images are off, `Failed`
            // because this image's transmit gave up.
            ImageRender::Disabled | ImageRender::Failed => None,
        };
        if let Some(bubble_image) = img {
            return Bubble::entry(flatten_lines(vec![header], styles), Some(bg), styles.text)
                .with_image(bubble_image);
        }
    }

    // A freshly started call has no details yet: the bubble shows
    // only the header under the pending tint.
    let mut lines = vec![header];
    if compact && !expanded {
        // Compact drops the body, but a bash cell keeps its command line so the
        // one datum worth scanning at a glance survives.
        if let Some(ToolDetails::Bash { command, .. }) = &entry.details {
            lines.push(bash_command_line(command, styles));
        }
    } else if let Some(details) = &entry.details {
        lines.extend(details_body(details, expanded, styles));
    }
    Bubble::entry(flatten_lines(lines, styles), Some(bg), styles.text)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Instant;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_agent::tool::{DiffDetails, TodoItem, TodoPriority, TodoStatus};
    use aj_app::chat::{ChatState, reduce};
    use aj_app::theme::{ColorMode, Theme};
    use vaxis::cell::Color;
    use vaxis::vxfw::{DrawContext, MaxSize, Size, Surface, Widget};

    use super::*;
    use crate::test_support::{flatten, rows};
    use crate::transcript::TranscriptView;

    fn styles() -> TranscriptStyles {
        TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        )
    }

    /// Styles with inline images enabled, for the image-placement tests.
    fn styles_with_images() -> TranscriptStyles {
        TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(ColorMode::Truecolor),
            crate::terminal::TerminalCaps {
                images: true,
                ..crate::terminal::TerminalCaps::default()
            },
        )
    }

    /// Styles with three distinct bubble tints, so tint-selection
    /// tests can tell the statuses apart (the bundled themes point
    /// all three tokens at one `toolBg` var on purpose).
    fn styles_with_distinct_tints() -> TranscriptStyles {
        TranscriptStyles {
            tool_pending_bg: Color::Rgb([1, 1, 1]),
            tool_success_bg: Color::Rgb([2, 2, 2]),
            tool_error_bg: Color::Rgb([3, 3, 3]),
            ..styles()
        }
    }

    fn draw_ctx(width: u16) -> DrawContext {
        crate::test_support::draw_ctx(width, None)
    }

    /// A running tool entry with the given name and args.
    fn entry(tool: &str, args: Value) -> ToolEntry {
        ToolEntry {
            call_id: "tu-1".into(),
            tool: tool.into(),
            args,
            status: ToolStatus::Running,
            details: None,
            content: Vec::new().into(),
            task: None,
            header_only: false,
        }
    }

    /// A finalized entry carrying `details`.
    fn done_entry(tool: &str, details: ToolDetails, is_error: bool) -> ToolEntry {
        ToolEntry {
            status: ToolStatus::Done { is_error },
            details: Some(details),
            ..entry(tool, serde_json::json!({}))
        }
    }

    fn bash_details(stdout: &str, exit_code: Option<i32>, task_id: Option<TaskId>) -> ToolDetails {
        ToolDetails::Bash {
            command: "cmd".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id,
        }
    }

    fn no_tasks() -> BTreeMap<TaskId, TaskInfo> {
        BTreeMap::new()
    }

    fn task_map(id: TaskId, status: TaskStatus) -> BTreeMap<TaskId, TaskInfo> {
        let mut map = BTreeMap::new();
        map.insert(
            id,
            TaskInfo {
                kind: TaskKind::Bash {
                    command: "cmd".into(),
                },
                label: "cmd".into(),
                owner: AgentId::Main,
                call_id: "tu-1".into(),
                status,
                started_at: Instant::now(),
                finished_at: None,
                cell: None,
            },
        );
        map
    }

    /// Composite a surface tree (buffer plus children by z-order)
    /// into a flat cell grid, the way `Surface::render` paints it.
    /// See `test_support` for the shared helpers.
    fn draw(cell: &mut Bubble, width: u16) -> Surface {
        cell.draw(&draw_ctx(width))
    }

    // ---- Header and bubble frame ---------------------------------------

    #[test]
    fn pending_cell_renders_bubble_with_padding_and_header() {
        let e = entry("read_file", serde_json::json!({"path": "/tmp/foo.txt"}));
        let s = styles_with_distinct_tints();
        let mut cell = build_tool_cell(&e, &no_tasks(), false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 60);
        let rows = rows(&surface);
        // One padding row above, one below the single content row,
        // then the untinted spacer row.
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[0], "", "top pad row is blank");
        assert_eq!(rows[1], " … read_file(path=\"/tmp/foo.txt\")");
        assert_eq!(rows[2], "", "bottom pad row is blank");
        assert_eq!(rows[3], "", "spacer row is blank");
        // Every bubble cell carries the pending tint, including the
        // tail cells after short lines. The spacer row does not.
        let grid = flatten(&surface);
        for (r, row) in grid.iter().enumerate().take(3) {
            for (c, cell) in row.iter().enumerate() {
                assert_eq!(
                    cell.style.bg, s.tool_pending_bg,
                    "cell ({r},{c}) not tinted",
                );
            }
        }
        assert!(grid[3].iter().all(|c| c.style.bg == Color::Default));
    }

    #[test]
    fn args_summary_formats_scalars_and_truncates_long_strings() {
        let args = serde_json::json!({
            "cmd": "echo hi",
            "count": 3,
            "flag": true,
            "opt": null,
            "nested": {"a": 1},
        });
        assert_eq!(
            format_args(&args),
            "cmd=\"echo hi\", count=3, flag=true, nested=…, opt=null",
        );
        let long = "x".repeat(200);
        let s = format_args(&Value::String(long.clone()));
        assert!(s.starts_with('"'));
        assert!(s.contains('…'));
        assert!(s.len() < long.len());
        assert_eq!(
            quote_for_summary("a\nb\tc\rd"),
            "\"a\\nb\\tc\\rd\"",
            "control characters escape to keep the summary one line",
        );
    }

    #[test]
    fn long_header_wraps_instead_of_truncating() {
        let e = entry(
            "bash",
            serde_json::json!({"command": "echo hi", "description": "x".repeat(120)}),
        );
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let surface = draw(&mut cell, 40);
        let rows = rows(&surface);
        let body: String = rows.join("");
        // The tail of the long description survives the wrap.
        assert!(body.contains("xxxx"), "{rows:?}");
        assert!(
            rows.iter().filter(|r| !r.is_empty()).count() > 1,
            "header wrapped to multiple rows: {rows:?}",
        );
    }

    #[test]
    fn success_and_error_glyphs_follow_the_result() {
        let ok = done_entry(
            "read_file",
            ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "line one\nline two".into(),
            },
            false,
        );
        let s = styles_with_distinct_tints();
        let mut cell = build_tool_cell(&ok, &no_tasks(), false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 60);
        let r = rows(&surface);
        assert_eq!(r[1], " ✓ read_file()");
        assert_eq!(r[2], " /tmp/foo.txt");
        assert_eq!(r[3], " line one");
        assert_eq!(r[4], " line two");
        assert_eq!(flatten(&surface)[0][0].style.bg, s.tool_success_bg);

        let err = done_entry(
            "bash",
            ToolDetails::Text {
                summary: "boom".into(),
                body: String::new(),
            },
            true,
        );
        let mut cell = build_tool_cell(&err, &no_tasks(), false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 60);
        assert_eq!(rows(&surface)[1], " ✗ bash()");
        assert_eq!(flatten(&surface)[0][0].style.bg, s.tool_error_bg);
    }

    #[test]
    fn degenerate_width_degrades_to_plain_rendering() {
        let e = done_entry(
            "bash",
            ToolDetails::Text {
                summary: "sum".into(),
                body: "body".into(),
            },
            false,
        );
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles_with_distinct_tints(),
            ImageRender::Disabled,
        );
        let surface = draw(&mut cell, 2);
        // No bubble: no children carrying an inset content surface,
        // and no tinted cells.
        assert!(surface.children.is_empty());
        assert!(
            flatten(&surface)
                .iter()
                .flatten()
                .all(|c| c.style.bg == Color::Default)
        );
    }

    // ---- Status derivation ----------------------------------------------

    #[test]
    fn nonzero_bash_exit_paints_failed_even_without_is_error() {
        let e = done_entry("bash", bash_details("", Some(1), None), false);
        let s = styles_with_distinct_tints();
        let mut cell = build_tool_cell(&e, &no_tasks(), false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 40);
        let r = rows(&surface);
        assert!(r[1].starts_with(" ✗"), "{r:?}");
        assert!(r.iter().any(|l| l.contains("[exit 1]")), "{r:?}");
        assert_eq!(flatten(&surface)[0][0].style.bg, s.tool_error_bg);
    }

    #[test]
    fn zero_or_missing_bash_exit_keeps_success() {
        for exit in [Some(0), None] {
            let e = done_entry("bash", bash_details("hi\n", exit, None), false);
            let mut cell = build_tool_cell(
                &e,
                &no_tasks(),
                false,
                false,
                &styles(),
                ImageRender::Disabled,
            );
            let surface = draw(&mut cell, 40);
            assert!(rows(&surface)[1].starts_with(" ✓"), "exit {exit:?}");
        }
    }

    // ---- Bash body -------------------------------------------------------

    #[test]
    fn bash_body_renders_command_streams_and_exit_line() {
        let details = ToolDetails::Bash {
            command: "make check".into(),
            stdout: "out line\n".into(),
            stderr: "uh oh\n".into(),
            exit_code: Some(2),
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id: None,
        };
        let s = styles();
        let lines = details_body(&details, true, &s);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert_eq!(
            texts,
            vec!["$ make check", "out line", "STDERR:", "uh oh", "[exit 2]"],
        );
        // The exit line is error-styled for a non-zero code, dim for
        // zero.
        assert_eq!(lines[4][0].style, s.error);
        let ok = details_body(&bash_details("", Some(0), None), true, &s);
        let exit_line = ok.last().expect("exit line");
        assert_eq!(exit_line[0].text, "[exit 0]");
        assert_eq!(exit_line[0].style, s.dim);
    }

    #[test]
    fn collapsed_bash_keeps_a_five_line_tail_with_earlier_hint() {
        let stdout = (1..=20)
            .map(|i| format!("out {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let e = done_entry("bash", bash_details(&stdout, Some(0), None), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(
            r.iter()
                .any(|l| l.contains("… (15 earlier lines, Alt+O to expand)")),
            "{r:?}",
        );
        for i in 16..=20 {
            assert!(r.iter().any(|l| l == &format!(" out {i}")), "tail {i}");
        }
        assert!(!r.iter().any(|l| l == " out 15"), "{r:?}");
    }

    #[test]
    fn collapsed_bash_pops_trailing_newline_before_counting() {
        // 6 real lines + trailing newline: hint says 1 earlier and
        // the visible tail is lines 2-6.
        let e = done_entry(
            "bash",
            bash_details("a\nb\nc\nd\ne\nf\n", Some(0), None),
            false,
        );
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(r.iter().any(|l| l.contains("(1 earlier lines")), "{r:?}",);
        assert!(r.iter().any(|l| l == " b"));
        assert!(!r.iter().any(|l| l == " a"));
    }

    #[test]
    fn bash_truncation_markers_render_per_stream_and_legacy_fallback() {
        use aj_agent::tool::{BashStreamTruncation, TruncationCause};
        let trunc = BashStreamTruncation {
            total_lines: 5000,
            total_bytes: 40000,
            output_lines: 2000,
            output_bytes: 16000,
            truncated_by: TruncationCause::Lines,
            last_line_partial: false,
            last_line_bytes: 0,
        };
        let details = ToolDetails::Bash {
            command: "seq 5000".into(),
            stdout: "line\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
            truncated: true,
            full_output_path: Some("/tmp/spill.log".into()),
            stdout_truncation: Some(trunc),
            stderr_truncation: None,
            task_id: None,
        };
        let texts: Vec<String> = details_body(&details, true, &styles())
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("Showing lines 3001-5000 of 5000 of stdout")
                    && t.contains("/tmp/spill.log")),
            "{texts:?}",
        );
        // Structured marker present, so no legacy fallback line.
        assert!(!texts.iter().any(|t| t.starts_with("[Output truncated")));

        // Legacy shape: `truncated` without per-stream summaries.
        let legacy = ToolDetails::Bash {
            command: "x".into(),
            stdout: "partial".into(),
            stderr: String::new(),
            exit_code: Some(0),
            truncated: true,
            full_output_path: Some("/tmp/spill.log".into()),
            stdout_truncation: None,
            stderr_truncation: None,
            task_id: None,
        };
        let texts: Vec<String> = details_body(&legacy, true, &styles())
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t == "[Output truncated; full output at /tmp/spill.log]"),
            "{texts:?}",
        );
    }

    // ---- Text collapse and the expansion flag ----------------------------

    #[test]
    fn text_body_collapses_to_ten_lines_and_expands_with_the_flag() {
        let body = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let details = ToolDetails::Text {
            summary: String::new(),
            body,
        };
        let e = done_entry("read_file", details, false);
        let mut collapsed = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut collapsed, 60));
        assert!(r.iter().any(|l| l == " line 10"));
        assert!(!r.iter().any(|l| l == " line 11"), "{r:?}");
        assert!(
            r.iter()
                .any(|l| l.contains("… (20 more lines, Alt+O to expand)")),
            "{r:?}",
        );

        let mut expanded = build_tool_cell(
            &e,
            &no_tasks(),
            true,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut expanded, 60));
        for i in 1..=30 {
            assert!(r.iter().any(|l| l == &format!(" line {i}")), "line {i}");
        }
        assert!(!r.iter().any(|l| l.contains("more lines")), "{r:?}");
    }

    // ---- Compact transcript mode -----------------------------------------

    /// Compact renders a non-bash tool header-only: the body is dropped.
    #[test]
    fn compact_hides_the_body_of_a_non_bash_tool() {
        let body = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let details = ToolDetails::Text {
            summary: String::new(),
            body,
        };
        let e = done_entry("read_file", details, false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            true,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(r.iter().any(|l| l.contains("read_file")), "header: {r:?}");
        assert!(!r.iter().any(|l| l.contains("line 15")), "no body: {r:?}");
    }

    /// Compact keeps a bash cell's `$ command` line but drops its output.
    #[test]
    fn compact_keeps_the_bash_command_line_only() {
        let e = done_entry("bash", bash_details("out line\n", Some(0), None), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            true,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(r.iter().any(|l| l.contains("$ cmd")), "command: {r:?}");
        assert!(
            !r.iter().any(|l| l.contains("out line")),
            "no output: {r:?}"
        );
        assert!(!r.iter().any(|l| l.contains("[exit 0]")), "no exit: {r:?}");
    }

    /// The tools-expand override wins over compact: with both set the full
    /// body renders, so it stays a reveal-everything escape hatch.
    #[test]
    fn tools_expand_overrides_compact() {
        let e = done_entry("bash", bash_details("out line\n", Some(0), None), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            true,
            true,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(r.iter().any(|l| l.contains("$ cmd")), "command: {r:?}");
        assert!(
            r.iter().any(|l| l.contains("out line")),
            "output shown: {r:?}"
        );
        assert!(
            r.iter().any(|l| l.contains("[exit 0]")),
            "exit shown: {r:?}"
        );
    }

    #[test]
    fn sanitizer_strips_ansi_and_control_bytes_from_text_bodies() {
        let details = ToolDetails::Text {
            summary: String::new(),
            body: "status\x1b[31mred\x1b[0m\rdone\x08!".into(),
        };
        let lines = details_body(&details, true, &styles());
        let text: String = lines[0].iter().map(|sp| sp.text.as_str()).collect();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(!text.contains('\r'), "{text:?}");
        assert!(!text.contains('\x08'), "{text:?}");
        assert!(text.contains("statusred"), "{text:?}");
    }

    // ---- Diff body ---------------------------------------------------------

    #[test]
    fn diff_body_renders_headers_signs_context_and_separator() {
        let before = "one\na\nb\nc\nd\ne\nf\ng\ntwo\n";
        let after = "ONE\na\nb\nc\nd\ne\nf\ng\nTWO\n";
        let details = ToolDetails::Diff(DiffDetails::new("src/lib.rs", before, after));
        let s = styles();
        let lines = details_body(&details, false, &s);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert_eq!(texts[0], "--- a/src/lib.rs");
        assert_eq!(texts[1], "+++ b/src/lib.rs");
        assert!(texts.contains(&"- one".to_string()));
        assert!(texts.contains(&"+ ONE".to_string()));
        assert!(texts.contains(&"  a".to_string()));
        assert!(texts.contains(&"…".to_string()), "{texts:?}");
        // Styling: removals red, additions green. Context and headers use
        // the faint attribute (styles.dim), not a color.
        let style_of = |needle: &str| {
            lines
                .iter()
                .find(|l| l[0].text == needle)
                .map(|l| l[0].style)
                .expect("line present")
        };
        assert_eq!(style_of("- one"), s.diff_remove);
        assert_eq!(style_of("+ ONE"), s.diff_add);
        assert_eq!(style_of("  a"), s.dim);
        assert_eq!(style_of("--- a/src/lib.rs"), s.dim);
    }

    // ---- Todos ---------------------------------------------------------------

    #[test]
    fn todos_translate_strikethrough_sgr_into_span_attributes() {
        let details = ToolDetails::Todos {
            items: vec![
                TodoItem {
                    id: "1".into(),
                    content: "ship it".into(),
                    priority: TodoPriority::High,
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    id: "2".into(),
                    content: "test it".into(),
                    priority: TodoPriority::Low,
                    status: TodoStatus::InProgress,
                },
            ],
        };
        let lines = details_body(&details, false, &styles());
        // No raw escape bytes survive into any span.
        for l in &lines {
            for sp in l {
                assert!(!sp.text.contains('\x1b'), "{:?}", sp.text);
            }
        }
        let completed = &lines[0];
        let struck: String = completed
            .iter()
            .filter(|sp| sp.style.strikethrough)
            .map(|sp| sp.text.as_str())
            .collect();
        assert_eq!(struck, "ship it");
        let plain: String = completed
            .iter()
            .filter(|sp| !sp.style.strikethrough)
            .map(|sp| sp.text.as_str())
            .collect();
        assert_eq!(plain, "✓  (high)");
        let in_progress: String = lines[1].iter().map(|sp| sp.text.as_str()).collect();
        assert_eq!(in_progress, "› test it (low)");
        assert!(lines[1].iter().all(|sp| !sp.style.strikethrough));
    }

    // ---- Json and Image -----------------------------------------------------

    #[test]
    fn json_body_pretty_prints() {
        let details = ToolDetails::Json(serde_json::json!({"a": 1, "b": [2, 3]}));
        let texts: Vec<String> = details_body(&details, false, &styles())
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert_eq!(texts[0], "{");
        assert!(texts.iter().any(|t| t.contains("\"a\": 1")), "{texts:?}");
        assert_eq!(texts.last().unwrap(), "}");
    }

    #[test]
    fn image_body_renders_textual_fallback() {
        let same = ToolDetails::Image {
            summary: "/tmp/pic.png".into(),
            mime_type: "image/png".into(),
            original_dimensions: (100, 50),
            displayed_dimensions: (100, 50),
        };
        let lines = details_body(&same, false, &styles());
        let text: String = lines[0].iter().map(|sp| sp.text.as_str()).collect();
        assert_eq!(text, "[image: image/png · 100x50]");

        let resized = ToolDetails::Image {
            summary: "/tmp/pic.jpg".into(),
            mime_type: "image/jpeg".into(),
            original_dimensions: (2000, 1000),
            displayed_dimensions: (800, 400),
        };
        let lines = details_body(&resized, false, &styles());
        let text: String = lines[0].iter().map(|sp| sp.text.as_str()).collect();
        assert_eq!(text, "[image: image/jpeg · 2000x1000 → 800x400]");
    }

    fn image_details() -> ToolDetails {
        ToolDetails::Image {
            summary: "/tmp/pic.png".into(),
            mime_type: "image/png".into(),
            original_dimensions: (100, 80),
            displayed_dimensions: (100, 80),
        }
    }

    /// With images enabled and a transmitted id, the cell drops the `[image:]`
    /// text and carries a `Placement` for that id.
    #[test]
    fn image_cell_places_the_image_when_enabled() {
        let e = done_entry("read_file", image_details(), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles_with_images(),
            ImageRender::Transmitted(9),
        );
        let surface = draw(&mut cell, 60);
        let placement = flatten(&surface)
            .into_iter()
            .flatten()
            .find_map(|c| c.image);
        assert!(
            matches!(placement, Some(p) if p.img_id == 9),
            "placement carries the id: {placement:?}",
        );
        assert!(
            !rows(&surface).iter().any(|l| l.contains("[image:")),
            "text fallback dropped when the image renders",
        );
    }

    /// With `ImageRender::Disabled` the cell keeps the `[image:]` text
    /// placeholder and writes no `Placement`. The capability-and-config gate
    /// lives in the caller (`resolve_image`), so this arm only sees the
    /// resolved decision.
    #[test]
    fn image_cell_falls_back_to_text_when_disabled() {
        let e = done_entry("read_file", image_details(), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles_with_images(),
            ImageRender::Disabled,
        );
        let surface = draw(&mut cell, 60);
        let r = rows(&surface);
        assert!(
            r.iter().any(|l| l.contains("[image: image/png · 100x80]")),
            "text placeholder shown: {r:?}",
        );
        assert!(
            flatten(&surface)
                .iter()
                .flatten()
                .all(|c| c.image.is_none()),
            "no placement when images are off",
        );
    }

    // ---- Sub-agent report -----------------------------------------------------

    #[test]
    fn sub_agent_report_renders_dim_header_and_collapses() {
        let report = (1..=15)
            .map(|i| format!("finding {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let details = ToolDetails::SubAgentReport {
            agent_id: 2,
            task: "scout the code".into(),
            report,
        };
        let s = styles();
        let lines = details_body(&details, false, &s);
        assert_eq!(lines[0][0].text, "sub-agent 2: scout the code");
        assert_eq!(lines[0][0].style, s.dim);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(texts.contains(&"finding 10".to_string()));
        assert!(!texts.contains(&"finding 11".to_string()));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("… (5 more lines, Alt+O to expand)")),
            "{texts:?}",
        );
    }

    // ---- header_only ------------------------------------------------------------

    #[test]
    fn header_only_renders_a_bare_wrapped_header() {
        let mut e = done_entry(
            "read_file",
            ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "secret body line".into(),
            },
            false,
        );
        e.header_only = true;
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles_with_distinct_tints(),
            ImageRender::Disabled,
        );
        let surface = draw(&mut cell, 60);
        let r = rows(&surface);
        // First row is the header, not a bg-painted pad, and there
        // is exactly one trailing spacer row.
        assert_eq!(r[0], "✓ read_file()");
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(!r.iter().any(|l| l.contains("secret body line")));
        assert!(
            flatten(&surface)
                .iter()
                .flatten()
                .all(|c| c.style.bg == Color::Default)
        );
    }

    // ---- Task badges ---------------------------------------------------------

    #[test]
    fn bash_task_id_renders_a_plain_badge_without_tracking() {
        // The badge must render off the persisted
        // `ToolDetails::Bash.task_id` alone (the resume path has no
        // task events), so an empty task map is the whole setup.
        let e = done_entry("bash", bash_details("", None, Some(3)), false);
        let mut cell = build_tool_cell(
            &e,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(r[1].contains("[task #3]"), "{r:?}");

        let fg = done_entry("bash", bash_details("", Some(0), None), false);
        let mut cell = build_tool_cell(
            &fg,
            &no_tasks(),
            false,
            false,
            &styles(),
            ImageRender::Disabled,
        );
        let r = rows(&draw(&mut cell, 60));
        assert!(!r.iter().any(|l| l.contains("[task #")), "{r:?}");
    }

    #[test]
    fn agent_kind_task_gets_tint_override_but_no_badge() {
        // An agent-kind task's `entry.task` fallback must not badge
        // the cell (aj badges bash launches only), while the terminal
        // task status still drives the tint.
        let s = styles_with_distinct_tints();
        let mut e = done_entry("bash", bash_details("", Some(0), None), false);
        e.task = Some(7);
        let mut tasks = task_map(7, TaskStatus::Killed);
        tasks.get_mut(&7).expect("task 7").kind = TaskKind::Agent {
            agent_id: 1,
            task: "investigate".into(),
        };
        let mut cell = build_tool_cell(&e, &tasks, false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 60);
        let r = rows(&surface);
        assert!(!r.iter().any(|l| l.contains("[task #")), "{r:?}");
        assert_eq!(flatten(&surface)[0][0].style.bg, s.tool_error_bg);
    }

    #[test]
    fn terminal_task_status_updates_badge_and_overrides_the_tint() {
        let s = styles_with_distinct_tints();
        let cases = [
            (
                TaskStatus::Exited(Some(0)),
                "[task #4 · exited 0]",
                s.tool_success_bg,
                " ✓",
            ),
            (
                TaskStatus::Exited(Some(2)),
                "[task #4 · exited 2]",
                s.tool_error_bg,
                " ✗",
            ),
            (
                TaskStatus::Exited(None),
                "[task #4 · terminated by signal]",
                s.tool_error_bg,
                " ✗",
            ),
            (
                TaskStatus::Killed,
                "[task #4 · killed]",
                s.tool_error_bg,
                " ✗",
            ),
        ];
        for (status, badge, tint, glyph) in cases {
            let e = done_entry("bash", bash_details("", None, Some(4)), false);
            let tasks = task_map(4, status);
            let mut cell = build_tool_cell(&e, &tasks, false, false, &s, ImageRender::Disabled);
            let surface = draw(&mut cell, 60);
            let r = rows(&surface);
            assert!(r[1].contains(badge), "{status:?}: {r:?}");
            assert!(r[1].starts_with(glyph), "{status:?}: {r:?}");
            assert_eq!(flatten(&surface)[0][0].style.bg, tint, "{status:?}");
        }
        // A still-running task keeps the plain badge and base tint.
        let e = done_entry("bash", bash_details("", None, Some(4)), false);
        let tasks = task_map(4, TaskStatus::Running);
        let mut cell = build_tool_cell(&e, &tasks, false, false, &s, ImageRender::Disabled);
        let surface = draw(&mut cell, 60);
        assert!(rows(&surface)[1].contains("[task #4]"));
        assert_eq!(flatten(&surface)[0][0].style.bg, s.tool_success_bg);
    }

    // ---- End-to-end through the reducer and the transcript view ---------------

    #[test]
    fn tool_events_reduce_and_render_through_the_transcript_view() {
        let mut chat = ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        );
        let mut lifecycle = aj_app::session::AgentLifecycle::default();
        let _ = reduce(
            &mut chat,
            &mut lifecycle,
            AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "tu-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
            },
        );
        let _ = reduce(
            &mut chat,
            &mut lifecycle,
            AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "tu-1".into(),
                tool: "bash".into(),
                result: bash_details("hi\n", Some(0), None),
                content: Vec::new().into(),
                is_error: false,
            },
        );
        let chat = Rc::new(RefCell::new(chat));
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let mut view = TranscriptView::new(
            Rc::clone(&chat),
            &theme,
            std::rc::Rc::new(std::cell::Cell::new(false)),
            std::rc::Rc::new(std::cell::RefCell::new(None)),
            std::rc::Rc::new(std::cell::Cell::new(None)),
            std::rc::Rc::new(std::cell::RefCell::new(
                crate::image_store::ImageStore::default(),
            )),
        );
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(60),
                height: Some(12),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        };
        let surface = view.draw(&ctx);
        let r = rows(&surface);
        assert!(
            r.iter().any(|l| l.contains("✓ bash(command=\"echo hi\")")),
            "{r:?}",
        );
        assert!(r.iter().any(|l| l.contains("$ cmd")), "{r:?}");
        assert!(r.iter().any(|l| l.contains("[exit 0]")), "{r:?}");
    }
}
