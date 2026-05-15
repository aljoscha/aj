//! Bash-execution rendering helpers.
//!
//! Specialises [`super::tool_execution::ToolExecutionComponent`]'s
//! body rendering for the [`aj_agent::tool::ToolDetails::Bash`]
//! variant. The component itself stays generic; this module just
//! formats the stdout/stderr/exit-code triple into the line list
//! the component appends to its scrollback.
//!
//! Today the formatting is text-only: each non-empty channel
//! (`stdout`, `stderr`) renders under a one-line dim header, and
//! the exit code / truncation marker tail goes underneath. Live
//! streaming (the `ToolExecutionUpdate` path that drives a 100ms
//! debounced partial snapshot) is wired through the generic
//! component too — this helper is called for every snapshot, so
//! the on-screen body stays consistent with the agent's view of
//! the running command.
//!
//! See `docs/aj-next-plan.md` §1.2 (`Bash` variant) and §1.3
//! (tool-update streaming).

use std::path::PathBuf;

use aj_tui::style;

/// Render a [`aj_agent::tool::ToolDetails::Bash`] payload to a
/// list of styled lines. The lines are append-ready: each one is
/// a complete row (no embedded newlines) carrying inline ANSI
/// escapes for any styling.
///
/// The layout matches the legacy CLI's `display_tool_result`
/// output verbatim where it can — same `stdout`-then-`stderr`
/// ordering, same trailing exit-code marker, same `[Output
/// truncated; full output at <path>]` notice — so users moving
/// between the two binaries don't have to re-learn how to read
/// the output.
#[allow(clippy::too_many_arguments)]
pub fn render_bash_body(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    truncated: bool,
    full_output_path: Option<&PathBuf>,
) -> Vec<String> {
    let mut lines = Vec::new();

    if !stdout.is_empty() {
        for line in stdout.split('\n') {
            // Trailing newlines surface as a single empty trailing
            // line; preserve it so the spacing between channels is
            // predictable.
            lines.push(line.to_string());
        }
    }

    if !stderr.is_empty() {
        // Dim header so the eye notices the channel switch
        // without it competing with the actual error text.
        lines.push(style::dim("STDERR:"));
        for line in stderr.split('\n') {
            lines.push(line.to_string());
        }
    }

    if let Some(code) = exit_code {
        let label = if code == 0 {
            style::dim(&format!("[exit {code}]"))
        } else {
            style::red(&format!("[exit {code}]"))
        };
        lines.push(label);
    }

    if truncated {
        let marker = match full_output_path {
            Some(path) => format!("[Output truncated; full output at {}]", path.display()),
            None => "[Output truncated]".to_string(),
        };
        lines.push(style::dim(&marker));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn renders_stdout_then_stderr_then_exit_marker() {
        let lines = render_bash_body("hello\nworld", "uh oh", Some(0), false, None);
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(
            plain,
            vec!["hello", "world", "STDERR:", "uh oh", "[exit 0]",]
        );
    }

    #[test]
    fn surfaces_a_truncation_path() {
        let p = PathBuf::from("/tmp/aj-bash-xyz.log");
        let lines = render_bash_body("partial", "", Some(0), true, Some(&p));
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert!(
            plain.last().unwrap().contains("/tmp/aj-bash-xyz.log"),
            "got {:?}",
            plain
        );
    }

    #[test]
    fn omits_exit_marker_when_no_code() {
        // The agent surfaces `exit_code: None` for a cancelled or
        // timed-out run; the wire `content` already explains the
        // failure to the model, so the rendered body just shows
        // whatever the child produced before being killed.
        let lines = render_bash_body("partial output", "", None, false, None);
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(plain, vec!["partial output"]);
    }
}
