//! Tool-execution component.
//!
//! Renders one tool call's lifecycle: the `name(arguments)` header
//! that appears as soon as the agent emits `ToolExecutionStart`,
//! optional progress updates, and the final structured result
//! when `ToolExecutionEnd` fires. Every tool variant flows through
//! this single component; the body rendering switches on the
//! [`ToolDetails`] variant. Specialised rendering helpers for
//! `Bash` and `Diff` live in [`super::bash_execution`] and
//! [`super::diff`] respectively.
//!
//! Visually the component renders as a bubble: a coloured
//! rectangle painted with one of three background tints depending
//! on the current [`Status`] (pending → neutral, succeeded →
//! greenish, failed → reddish). The header and body lines sit
//! inside the bubble at `padding_x = 1`, framed by one bg-painted
//! blank row above and below (`padding_y = 1`). Separation from
//! the surrounding chat elements comes from the auto-spacer the
//! [`crate::modes::interactive::event_pump::EventPump`] inserts
//! between sibling chat-container children.
//!
//! ## Render-cost shape
//!
//! The bubble's frame, padding and background paint live in an
//! owned [`aj_tui::components::text_box::TextBox`]; the header and
//! body each live in their own [`aj_tui::components::text::Text`]
//! child inside that box. Both kinds of widget cache their
//! rendered output keyed on inputs that change rarely
//! (`(text, width)` for `Text`; `(width, child-lines, bg-sample)`
//! for `TextBox`), so a finalised tool with kilobytes of body
//! costs one wrap pass at construction and then string-equality
//! checks on every subsequent frame. The hot render path in a
//! long chat with many finished tools is dominated by clones of
//! the cached `Vec<String>`, not by re-wrapping.
//!
//! Mutating callers (`update_partial`, `update_result`) tear the
//! `Text` children down and rebuild them so the next render
//! repopulates the caches with the new content. State that
//! doesn't affect visible output (status flip to `Succeeded` /
//! `Failed`) still rebuilds because the header glyph changes; the
//! body is dropped and re-installed in that path too, which is
//! fine because state changes are rare compared to renders.
//!
//! See `docs/aj-next-plan.md` §1.2 (`ToolDetails`) and §4
//! (`components/tool_execution.rs`).

use std::any::Any;
use std::sync::Arc;

use aj_agent::tool::ToolDetails;
use aj_tools::sanitize_terminal_output;
use aj_tui::ansi::wrap_text_with_ansi;
use aj_tui::component::Component;
use aj_tui::components::text::Text;
use aj_tui::components::text_box::TextBox;
use aj_tui::keys::InputEvent;
use aj_tui::style;
use serde_json::Value;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::bash_execution::render_bash_body;
use crate::modes::interactive::components::diff::render_unified_diff;

/// Horizontal padding inside the bubble (one column on each side
/// so the tinted rectangle reads as an inset block rather than
/// edge-to-edge text).
const PADDING_X: usize = 1;
/// Vertical padding inside the bubble. One bg-painted blank row
/// above and below the content matches the user-message bubble's
/// rhythm so the two kinds of bubbles compose cleanly when the
/// auto-spacer drops a plain blank line between them.
const PADDING_Y: usize = 1;

/// Minimum total render width at which the bubble framing kicks
/// in. Below this we fall back to a plain header+body listing so
/// the bg-padding pipeline (which assumes at least two cells of
/// horizontal padding plus one cell of content) doesn't try to
/// paint a degenerate row. The threshold also covers the headless
/// `width = 0` case used by some tests.
const MIN_BUBBLE_WIDTH: usize = 3;

/// Lifecycle states a tool execution moves through. Drives the
/// rendered status indicator in the header line and the bubble's
/// background tint so users can distinguish a not-yet-started call
/// from a finished one at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Tool args have been received but the tool body hasn't run
    /// yet (we observed `ToolExecutionStart`). Header shows a
    /// dim spinner glyph; the bubble paints with the neutral
    /// pending tint.
    Started,
    /// The tool finished and the viewer should read it as a
    /// success: header shows a green check, the bubble paints
    /// with the success tint. Maps from `is_error: false` *and*
    /// (for bash) a zero or unknown exit code. See
    /// [`derive_status`] for the full rule.
    Succeeded,
    /// The tool finished and the viewer should read it as a
    /// failure: header shows a red cross, the bubble paints
    /// with the error tint. Maps from `is_error: true` or (for
    /// bash) a non-zero exit code.
    Failed,
}

/// On-screen representation of a single tool call.
///
/// One component per `tool_use_id`. The event pump keys these by
/// id in a `HashMap<String, usize>` so each
/// `ToolExecutionStart` / `ToolExecutionUpdate` / `ToolExecutionEnd`
/// event reaches the right component. The component is a `Box<dyn
/// Component>` from the chat container's perspective; the event
/// pump downcasts via `as_any_mut` to call mutating methods.
pub struct ToolExecutionComponent {
    /// Tool name (`bash`, `read_file`, …) shown in the header.
    tool_name: String,
    /// JSON-encoded arguments, rendered inside the parens after
    /// the name. Stored as a string so we can re-render cheaply.
    args_pretty: String,
    /// Current execution status. Drives the header glyph and the
    /// bubble background tint.
    status: Status,
    /// Body lines rendered under the header. Populated from the
    /// `ToolDetails` variant on every `ToolExecutionUpdate` and
    /// finalized on `ToolExecutionEnd`.
    body: Vec<String>,
    /// Bg-paint closure for the in-flight state. Stored once at
    /// construction so the render path never has to rebuild the
    /// closure.
    bg_pending: Arc<dyn Fn(&str) -> String>,
    /// Bg-paint closure for the `Succeeded` state.
    bg_success: Arc<dyn Fn(&str) -> String>,
    /// Bg-paint closure for the `Failed` state.
    bg_error: Arc<dyn Fn(&str) -> String>,
    /// Cached child tree.
    ///
    /// The bubble owns a `TextBox` whose padding becomes the
    /// bubble's frame (1 cell horizontally, 1 row vertically) and
    /// whose `bg_fn` reflects the current [`Status`]. Inside it
    /// sit two zero-padding `Text` children: a header text and an
    /// optional body text. Both `Text` and `TextBox` carry their
    /// own caches, so steady-state renders return cached vectors
    /// without re-running the wrap pass.
    ///
    /// `rebuild_children` is the single mutating path: it tears
    /// down the existing children and reinstalls fresh ones, so
    /// `Text`'s `(text, width)` cache and `TextBox`'s
    /// `(child-lines, width, bg-sample)` cache both re-populate on
    /// the next render.
    bubble: TextBox,
}

impl ToolExecutionComponent {
    /// Build a new component for a tool call with the given name
    /// and arguments. The component starts in [`Status::Started`]
    /// and paints with the `tool_pending_bg` tint until the agent
    /// emits a result; [`Self::update_partial`] / [`Self::update_result`]
    /// flip the status and the matching bg.
    pub fn new(tool_name: String, args: &Value, theme: &ChatTheme) -> Self {
        let mut me = Self {
            tool_name,
            args_pretty: format_args(args),
            status: Status::Started,
            body: Vec::new(),
            bg_pending: Arc::clone(&theme.tool_pending_bg),
            bg_success: Arc::clone(&theme.tool_success_bg),
            bg_error: Arc::clone(&theme.tool_error_bg),
            bubble: TextBox::new(PADDING_X, PADDING_Y),
        };
        me.bubble.set_bg_fn(me.make_bg_box());
        me.rebuild_children();
        me
    }

    /// Replace the rendered body with the partial snapshot in
    /// `details`. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionUpdate`] (today
    /// only `bash` emits these).
    pub fn update_partial(&mut self, details: &ToolDetails) {
        self.body = render_details_body(details);
        self.rebuild_children();
    }

    /// Finalize the component with the tool's result. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionEnd`].
    pub fn update_result(&mut self, details: &ToolDetails, is_error: bool) {
        self.body = render_details_body(details);
        self.status = derive_status(details, is_error);
        // The status flip pulls a different bg closure into the
        // box. `set_bg_fn` doesn't invalidate the cache itself;
        // `TextBox::render` compares a sampled application of the
        // new closure against the cached sample, so a real bg
        // change reaches the screen on the next render even though
        // the children are unchanged.
        self.bubble.set_bg_fn(self.make_bg_box());
        self.rebuild_children();
    }

    /// Render the header line (`status tool(args)`). Kept private
    /// so the header style is uniform across every variant.
    fn header_line(&self) -> String {
        let glyph = match self.status {
            Status::Started => style::dim("…"),
            Status::Succeeded => style::green("✓"),
            Status::Failed => style::red("✗"),
        };
        let name = style::bold(&self.tool_name);
        format!("{glyph} {name}({})", style::dim(&self.args_pretty))
    }

    /// Build the boxed bg closure that matches the current status.
    ///
    /// The returned `Box<dyn Fn>` clones an [`Arc`] off the stored
    /// status closures and dispatches through it. Cloning the Arc
    /// keeps the bubble's closure pointing at the same underlying
    /// function as the rest of the chat theme, so calling sites
    /// that share a theme also share their bg paint cache lines.
    fn make_bg_box(&self) -> Box<dyn Fn(&str) -> String> {
        let arc = match self.status {
            Status::Started => Arc::clone(&self.bg_pending),
            Status::Succeeded => Arc::clone(&self.bg_success),
            Status::Failed => Arc::clone(&self.bg_error),
        };
        Box::new(move |s: &str| arc(s))
    }

    /// Tear down the bubble's children and rebuild them from the
    /// current header / body state.
    ///
    /// Called from every state-changing path (`new`,
    /// `update_partial`, `update_result`). `TextBox::clear`
    /// invalidates the bubble's frame cache, and each freshly
    /// constructed `Text` starts with an empty wrap cache, so the
    /// next render does the wrap pass once and caches it. Renders
    /// in between state changes return the cached output.
    fn rebuild_children(&mut self) {
        self.bubble.clear();

        // Header. Zero internal padding so the `TextBox`'s
        // 1-column inset is the only horizontal margin around it;
        // double-padding would push the header off the visual
        // grid the rest of the chat scrollback shares.
        let header_text = Text::new(&self.header_line(), 0, 0);
        self.bubble.add_child(Box::new(header_text));

        // Body. Joining with `\n` lets `wrap_text_with_ansi`
        // (called inside `Text::render`) see the body as one
        // multi-line input and track its ANSI state across the
        // implicit line breaks. The body helpers in this module
        // each emit fully self-terminated styled rows, so the
        // joined-vs-per-line wrap behaviour is identical for
        // today's inputs; the join is the simpler shape and
        // matches how `Text` already wraps the rest of the chat.
        if !self.body.is_empty() {
            let body_joined = self.body.join("\n");
            let body_text = Text::new(&body_joined, 0, 0);
            self.bubble.add_child(Box::new(body_text));
        }
    }
}

impl Component for ToolExecutionComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Tiny widths drop into a degraded "render plain" path so
        // the strict line-width check in `Tui::render` doesn't
        // trip on the bg-padding pipeline. Headless and zero-width
        // edge cases land here too. The bubble framing needs at
        // least one column on each side plus one column of content,
        // so anything below [`MIN_BUBBLE_WIDTH`] skips the box and
        // falls back to the original wrap-per-line path.
        if width < MIN_BUBBLE_WIDTH {
            let mut out = Vec::with_capacity(self.body.len() + 1);
            out.push(self.header_line());
            for line in &self.body {
                out.extend(wrap_text_with_ansi(line, width.max(1)));
            }
            return out;
        }

        // Steady-state hot path: delegates to the cached bubble.
        // First render after a state change rebuilds the caches;
        // every render in between is a `Vec<String>` clone out of
        // the box's cache.
        self.bubble.render(width)
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        // Propagate down. `TextBox::invalidate` drops its own
        // cache and calls `invalidate` on each child `Text`, so
        // the next render rebuilds from scratch.
        self.bubble.invalidate();
    }
}

impl AsRef<dyn Any> for ToolExecutionComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

/// Decide which [`Status`] to paint a finalized tool with.
///
/// The agent's `is_error` flag is reserved for catastrophic
/// failures (tool cancellation, timeout, JSON parse errors). It
/// deliberately does *not* fire for a successful invocation that
/// happens to return a non-zero exit code: per the bash tool's
/// contract the model can read the `exit_code` field of the
/// `ToolDetails::Bash` payload and decide what to do, and we don't
/// want to second-guess that for the model's input view.
///
/// For the *human* viewer, though, a `[exit 1]` line is plainly a
/// failure and a green ✓ on the same bubble reads wrong. So we
/// override here: any tool result that carries an explicit non-
/// zero exit code paints as [`Status::Failed`] regardless of the
/// agent-side flag. Other variants fall back to `is_error`.
fn derive_status(details: &ToolDetails, is_error: bool) -> Status {
    if is_error {
        return Status::Failed;
    }
    if let ToolDetails::Bash {
        exit_code: Some(code),
        ..
    } = details
    {
        if *code != 0 {
            return Status::Failed;
        }
    }
    Status::Succeeded
}

/// Build a single-line argument summary from the tool's input
/// JSON. The goal is a compact `command(arg1=val1, arg2=val2)`
/// preview that fits on one line; if the JSON is too verbose for
/// that, fall back to a `…` placeholder.
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
/// Newlines / control characters are replaced with their `\n` /
/// `\t` escapes so the header stays on one row even when the input
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

/// Render the body lines for a [`ToolDetails`] variant. Switching
/// here keeps each variant's specialised rendering close to the
/// component while letting [`Self::render`] stay variant-agnostic.
///
/// Every raw text field that originated outside this crate -- tool
/// summaries, command strings, sub-agent reports, the diff payload
/// -- passes through [`sanitize_terminal_output`] before any styling
/// is applied. Today the bash tool already sanitises its own stdout
/// / stderr at the source, so applying the transform a second time
/// here is a no-op for that path; the call covers every other variant
/// and guards against future tools that emit raw subprocess output
/// without going through the bash tool's helper. The transform
/// strips ANSI escapes, drops carriage returns, and removes other
/// terminal-control bytes that would otherwise disagree with the
/// renderer's width math (and produce a ragged right edge on the
/// surrounding bubble) or clobber adjacent cells via cursor moves
/// or erase-in-line side effects.
fn render_details_body(details: &ToolDetails) -> Vec<String> {
    match details {
        ToolDetails::Text { summary, body } => {
            let summary = sanitize_terminal_output(summary);
            let body = sanitize_terminal_output(body);
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(style::dim(&summary));
            }
            for line in body.split('\n') {
                lines.push(line.to_string());
            }
            // Trim a trailing empty line introduced by a body that
            // ended in `\n`; the surrounding bubble's bottom pad
            // already handles the vertical separation.
            if lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            lines
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => render_unified_diff(
            &sanitize_terminal_output(path),
            &sanitize_terminal_output(before),
            &sanitize_terminal_output(after),
        ),
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            truncated,
            full_output_path,
        } => {
            let command = sanitize_terminal_output(command);
            // `stdout` / `stderr` are already sanitised at the bash
            // tool source; running the transform again here is cheap
            // and keeps this match arm self-contained against future
            // changes to the bash payload's provenance.
            let stdout = sanitize_terminal_output(stdout);
            let stderr = sanitize_terminal_output(stderr);
            let mut lines = vec![style::dim(&format!("$ {command}"))];
            lines.extend(render_bash_body(
                &stdout,
                &stderr,
                *exit_code,
                *truncated,
                full_output_path.as_ref(),
            ));
            lines
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            let task = sanitize_terminal_output(task);
            let report = sanitize_terminal_output(report);
            let mut lines = vec![style::dim(&format!("sub-agent {agent_id}: {task}"))];
            for line in report.split('\n') {
                lines.push(line.to_string());
            }
            lines
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from `aj-tools`
            // so the interactive view matches the wire content the
            // model sees in `tool_result`. `format_todo_list`
            // sanitises each item's content internally, so the
            // strikethrough SGR it emits for completed items
            // survives but raw control bytes in the content do not.
            let formatted = aj_tools::tools::todo::format_todo_list(items);
            formatted.split('\n').map(|s| s.to_string()).collect()
        }
        ToolDetails::Json(value) => {
            let formatted =
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            formatted.split('\n').map(|s| s.to_string()).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_agent::tool::ToolDetails;
    use aj_tui::ansi::visible_width;

    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()))
    }

    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    /// True when the visible content of `line` (after stripping
    /// ANSI escapes) is empty or all-whitespace — i.e. the row is
    /// a bg-painted padding row.
    fn is_blank_row(line: &str) -> bool {
        strip_ansi(line).trim().is_empty()
    }

    #[test]
    fn header_includes_tool_name_and_args_summary() {
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        let mut c = ToolExecutionComponent::new("read_file".to_string(), &args, &theme());
        let lines = c.render(80);
        // First and last rows are bg-painted blank padding.
        assert!(is_blank_row(&lines[0]));
        assert!(is_blank_row(lines.last().expect("non-empty render")));
        // Header lands inside the bubble. After stripping ANSI and
        // trim, it starts with the spinner glyph and includes the
        // tool name plus the args summary.
        let header_plain = strip_ansi(&lines[1]);
        let header_trimmed = header_plain.trim_start();
        assert!(header_trimmed.starts_with("…"), "{header_trimmed:?}");
        assert!(header_trimmed.contains("read_file"));
        assert!(header_trimmed.contains("path=\"/tmp/foo.txt\""));
    }

    #[test]
    fn finalizing_with_text_body_renders_summary_and_body_inside_the_bubble() {
        let mut c =
            ToolExecutionComponent::new("read_file".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "line one\nline two".into(),
            },
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        // Success → ✓ glyph in the header row.
        assert!(
            lines
                .iter()
                .any(|l| l.trim_start().starts_with("✓") && l.contains("read_file")),
        );
        // Summary and body lines land somewhere inside the bubble.
        // Stripping ANSI leaves trailing spaces (the bg fills the
        // row to the full terminal width), so a `contains` check
        // against `trim` is the robust shape.
        assert!(
            lines.iter().any(|l| l.trim().contains("/tmp/foo.txt")),
            "{lines:#?}",
        );
        assert!(lines.iter().any(|l| l.trim().contains("line one")));
        assert!(lines.iter().any(|l| l.trim().contains("line two")));
    }

    #[test]
    fn error_status_renders_a_red_cross_in_the_header() {
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Text {
                summary: "boom".into(),
                body: String::new(),
            },
            true,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.trim_start().starts_with("✗")));
    }

    #[test]
    fn nonzero_bash_exit_code_paints_as_failure_even_without_is_error() {
        // The bash tool deliberately leaves `is_error: false` on a
        // non-zero exit so the model can read `exit_code` from the
        // structured payload and decide for itself; the *visual*
        // however needs to match what the user sees in the `[exit
        // N]` footer. Verify the bubble paints with the failure
        // glyph in that case.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Bash {
                command: "exit 1".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(1),
                truncated: false,
                full_output_path: None,
            },
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✗")),
            "expected ✗ glyph in header for non-zero exit; got {lines:?}",
        );
    }

    #[test]
    fn zero_bash_exit_code_still_paints_as_success() {
        // Don't regress the happy path: a zero exit must keep the
        // green check even though we now look at exit_code.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Bash {
                command: "echo hi".into(),
                stdout: "hi\n".into(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
            },
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✓")),
            "expected ✓ glyph in header for zero exit; got {lines:?}",
        );
    }

    #[test]
    fn missing_bash_exit_code_paints_as_success_when_not_flagged() {
        // Signal-terminated processes (e.g. SIGTERM after a
        // timeout) leave `exit_code: None`. Those are catastrophic
        // and the agent already raises `is_error: true` in that
        // path; with the flag clear we don't second-guess the
        // tool and keep the success styling.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Bash {
                command: "true".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                truncated: false,
                full_output_path: None,
            },
            false,
        );
        let lines: Vec<_> = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("✓")),
            "expected ✓ glyph in header when exit_code is None and is_error is false; got {lines:?}",
        );
    }

    #[test]
    fn long_string_args_get_truncated_with_an_ellipsis() {
        let long = "x".repeat(200);
        let s = format_args(&serde_json::Value::String(long.clone()));
        // The summary is wrapped in quotes; the inner body should be
        // capped well before the input length.
        assert!(s.starts_with('"'));
        assert!(s.contains('…'));
        assert!(s.len() < long.len());
    }

    #[test]
    fn body_lines_wider_than_width_get_wrapped_to_fit() {
        // Regression: clippy / build output regularly contains
        // single lines wider than the terminal (e.g. `help: try:
        // ...` suggestions naming a fully-qualified type). The
        // component must wrap them so the strict line-width check
        // in `Tui::render` doesn't panic on the next frame.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        let long_line = "x".repeat(300);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: long_line.clone(),
            },
            false,
        );
        let width = 80;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= width,
                "line {i} exceeds width: {w} > {width}: {line:?}",
            );
        }
    }

    #[test]
    fn header_wider_than_width_gets_wrapped_to_fit() {
        // A long `description=` field can push the header past the
        // terminal edge on its own. Make sure the wrap path covers
        // the header line too.
        let args = serde_json::json!({
            "command": "echo hi",
            "description": "x".repeat(200),
        });
        let mut c = ToolExecutionComponent::new("bash".to_string(), &args, &theme());
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w <= width,
                "line {i} exceeds width: {w} > {width}: {line:?}",
            );
        }
    }

    #[test]
    fn pending_running_succeeded_paint_with_distinct_backgrounds() {
        // Three renders, three SGR background sequences. The
        // bundled dark theme picks different RGB / 256-color
        // values for each of `toolPendingBg` / `toolSuccessBg` /
        // `toolErrorBg`, so the rendered rows must carry different
        // escape sequences. We grab the bg-paint prefix off the
        // first row of each render and compare them.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());

        let bg_sample = |c: &mut ToolExecutionComponent| -> String {
            let lines = c.render(40);
            // The first row is the top bg-pad — entirely a bg
            // escape + spaces + bg-close. Take everything up to
            // the first space; that's our prefix.
            let first = &lines[0];
            let cut = first.find(' ').unwrap_or(first.len());
            first[..cut].to_string()
        };

        let pending = bg_sample(&mut c);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: "ok".into(),
            },
            false,
        );
        let succeeded = bg_sample(&mut c);
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body: "boom".into(),
            },
            true,
        );
        let failed = bg_sample(&mut c);

        assert_ne!(pending, succeeded, "pending and succeeded share an escape");
        assert_ne!(pending, failed, "pending and failed share an escape");
        assert_ne!(succeeded, failed, "succeeded and failed share an escape");
    }

    #[test]
    fn repeated_renders_return_byte_identical_output() {
        // The whole point of the refactor: a finalised tool with a
        // multi-line body should render the same bytes on every
        // subsequent frame, *and* the second frame must not have
        // to re-do the wrap pass. We can't directly observe cache
        // hits from outside, but byte-equality on the rendered
        // output is the user-visible contract and is what the
        // diff-aware terminal write depends on. A regression that
        // produced subtly different ANSI on cache hit (e.g. a
        // closure that captured stateful counters) would surface
        // here.
        let body = (0..50)
            .map(|i| format!("line {i:02}: lorem ipsum dolor sit amet"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut c =
            ToolExecutionComponent::new("read_file".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/big.txt".into(),
                body,
            },
            false,
        );
        let first = c.render(80);
        let second = c.render(80);
        let third = c.render(80);
        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    #[test]
    fn body_with_carriage_returns_and_erase_in_line_renders_flush_width() {
        // Regression for the ragged-right-edge bug. Real tool output
        // (cargo / git / pip progress, ANSI-coloured clippy output)
        // ships `\r` overprints and `ESC[K` erase-in-line sequences
        // that disagree with the renderer's width measurement: `\r`
        // was counted as zero-width but the terminal honours it, and
        // `ESC[K` was stripped from measurement but its terminal side
        // effect erases trailing cells with the *default* background
        // -- chewing a hole in the bubble's painted tint. The body
        // sanitisation in `render_details_body` removes both before
        // they reach the wrap path, so every row of the bubble lands
        // at exactly the render width.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        // Progress-style overprint, an SGR-reset followed by an
        // erase-in-line (the canonical "ragged right edge"
        // pattern), and a control byte (`\x08`) for good measure.
        let body =
            "progress: 10%\rprogress: 20%\nstatus\x1b[0m\x1b[Kdone\nback\x08space\n".to_string();
        c.update_result(
            &ToolDetails::Text {
                summary: String::new(),
                body,
            },
            false,
        );
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w == width,
                "row {i} is not flush to width: visible_width={w}, expected={width}: {line:?}",
            );
            assert!(!line.contains('\r'), "row {i} still carries CR: {line:?}");
            // Erase-in-line and other non-styling CSI must not survive.
            assert!(
                !line.contains("\x1b[K"),
                "row {i} still carries erase-in-line: {line:?}",
            );
            // Backspace and other C0 controls must not survive.
            assert!(
                !line.chars().any(|c| {
                    let code = u32::from(c);
                    code <= 0x1f && c != '\t' && c != '\n' && c != '\x1b'
                }),
                "row {i} still carries a non-tab/-newline control byte: {line:?}",
            );
        }
    }

    #[test]
    fn bash_command_field_strips_control_bytes_from_header() {
        // Defence in depth: if a future caller stuffs an ANSI / CR /
        // control byte into `ToolDetails::Bash.command`, the dim-
        // styled `$ <command>` header line must still render flush
        // to width and contain only the visible command text.
        let mut c =
            ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}), &theme());
        c.update_result(
            &ToolDetails::Bash {
                command: "echo \x1b[31mboom\x1b[0m\rmore".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
            },
            false,
        );
        let width = 60;
        let lines = c.render(width);
        for (i, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            assert!(
                w == width,
                "row {i} not flush to width: visible_width={w}: {line:?}",
            );
            assert!(!line.contains('\r'), "row {i} still carries CR: {line:?}");
        }
        // The visible command text comes through, sans ANSI / CR.
        let plain: Vec<_> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(
            plain.iter().any(|l| l.contains("echo boommore")),
            "{plain:#?}",
        );
    }
}
