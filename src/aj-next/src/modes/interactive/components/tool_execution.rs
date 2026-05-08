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
//! See `docs/aj-next-plan.md` §1.2 (`ToolDetails`) and §4
//! (`components/tool_execution.rs`).

use std::any::Any;

use aj_agent::tool::ToolDetails;
use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;
use serde_json::Value;

use crate::modes::interactive::components::bash_execution::render_bash_body;
use crate::modes::interactive::components::diff::render_unified_diff;

/// Lifecycle states a tool execution moves through. Drives the
/// rendered status indicator in the header line so users can
/// distinguish a not-yet-started call from a finished one at a
/// glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Tool args have been received but the tool body hasn't run
    /// yet (we observed `ToolExecutionStart`). Header shows a
    /// dim spinner glyph.
    Started,
    /// The tool finished without flagging an error (we observed
    /// `ToolExecutionEnd { is_error: false }`). Header shows a
    /// green check.
    Succeeded,
    /// The tool finished with `is_error: true`. Header shows a red
    /// cross and the body renders with red emphasis.
    Failed,
}

/// On-screen representation of a single tool call.
///
/// One component per `tool_use_id`. The event pump keys these by
/// id in a `HashMap<String, *mut ToolExecutionComponent>` so each
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
    /// Current execution status. Drives the header glyph.
    status: Status,
    /// Body lines rendered under the header. Populated from the
    /// `ToolDetails` variant on every `ToolExecutionUpdate` and
    /// finalized on `ToolExecutionEnd`.
    body: Vec<String>,
}

impl ToolExecutionComponent {
    /// Build a new component for a tool call with the given name
    /// and arguments. The component starts in [`Status::Started`];
    /// the header alone renders until [`Self::update_partial`] or
    /// [`Self::update_result`] is called.
    pub fn new(tool_name: String, args: &Value) -> Self {
        Self {
            tool_name,
            args_pretty: format_args(args),
            status: Status::Started,
            body: Vec::new(),
        }
    }

    /// Replace the rendered body with the partial snapshot in
    /// `details`. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionUpdate`] (today
    /// only `bash` emits these).
    pub fn update_partial(&mut self, details: &ToolDetails) {
        self.body = render_details_body(details);
    }

    /// Finalize the component with the tool's result. Called from
    /// [`aj_agent::events::AgentEvent::ToolExecutionEnd`].
    pub fn update_result(&mut self, details: &ToolDetails, is_error: bool) {
        self.body = render_details_body(details);
        self.status = if is_error {
            Status::Failed
        } else {
            Status::Succeeded
        };
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
}

impl Component for ToolExecutionComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(self.body.len() + 2);
        // Empty leading line so each tool call has visual breathing
        // room from the previous chat entry; matches the
        // user/assistant message padding rhythm.
        out.push(String::new());
        out.push(self.header_line());
        for line in &self.body {
            // Indent body lines by two columns so they're clearly
            // attached to the header.
            out.push(format!("  {line}"));
        }
        out
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for ToolExecutionComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
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
fn render_details_body(details: &ToolDetails) -> Vec<String> {
    match details {
        ToolDetails::Text { summary, body } => {
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(style::dim(summary));
            }
            for line in body.split('\n') {
                lines.push(line.to_string());
            }
            // Trim a trailing empty line introduced by a body that
            // ended in `\n`; our caller already adds vertical
            // padding through the chat container's spacing.
            if lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            lines
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => render_unified_diff(path, before, after),
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            truncated,
            full_output_path,
        } => {
            let mut lines = vec![style::dim(&format!("$ {command}"))];
            lines.extend(render_bash_body(
                stdout,
                stderr,
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
            let mut lines = vec![style::dim(&format!("sub-agent {agent_id}: {task}"))];
            for line in report.split('\n') {
                lines.push(line.to_string());
            }
            lines
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from `aj-tools`
            // so the interactive view matches the wire content the
            // model sees in `tool_result`.
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

    #[test]
    fn header_includes_tool_name_and_args_summary() {
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        let mut c = ToolExecutionComponent::new("read_file".to_string(), &args);
        // No update yet → `Started` glyph in header.
        let lines: Vec<_> = c.render(80).into_iter().map(|l| strip_ansi(&l)).collect();
        assert_eq!(lines[0], "");
        assert!(lines[1].contains("read_file"));
        assert!(lines[1].contains("path=\"/tmp/foo.txt\""));
        assert!(lines[1].starts_with("…"));
    }

    #[test]
    fn finalizing_with_text_body_renders_summary_and_body() {
        let mut c = ToolExecutionComponent::new("read_file".to_string(), &serde_json::json!({}));
        c.update_result(
            &ToolDetails::Text {
                summary: "/tmp/foo.txt".into(),
                body: "line one\nline two".into(),
            },
            false,
        );
        let lines: Vec<_> = c.render(80).into_iter().map(|l| strip_ansi(&l)).collect();
        // Header transitions to ✓ on success.
        assert!(lines[1].starts_with("✓"));
        // Summary plus body lines, both indented by the
        // component's two-column body indent.
        assert!(lines.iter().any(|l| l == "  /tmp/foo.txt"));
        assert!(lines.iter().any(|l| l == "  line one"));
        assert!(lines.iter().any(|l| l == "  line two"));
    }

    #[test]
    fn error_status_renders_a_red_cross_in_the_header() {
        let mut c = ToolExecutionComponent::new("bash".to_string(), &serde_json::json!({}));
        c.update_result(
            &ToolDetails::Text {
                summary: "boom".into(),
                body: String::new(),
            },
            true,
        );
        let lines: Vec<_> = c.render(80).into_iter().map(|l| strip_ansi(&l)).collect();
        assert!(lines[1].starts_with("✗"));
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
}
